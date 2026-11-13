use newt_core::event::EventKind;
use newt_core::journal::JournalEntry;
use newt_core::message::Message;
use newt_core::prompt::Lane;
use newt_core::provider::{Model, ProviderError};
use newt_core::turn::{TurnDeps, TurnError, TurnInput, TurnSettings, run_turn};

use crate::backend::{Backend, Prepared};
use crate::journal::TurnJournal;
use crate::live::Channel;
use crate::runner::{Inner, elapsed_ms};
use crate::sessions::{CommitError, Lease, PendingEvent};
use crate::sinks::{LiveSink, TeeSink};
use crate::steering::SessionSteering;
use crate::store::live_topic;
use crate::turn_start::NextTurn;

struct TurnPlan {
    turn_id: String,
    events: Vec<PendingEvent>,
    replay: Vec<JournalEntry>,
    next_seq: i32,
    fresh: bool,
}

pub(crate) async fn run_session<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    agent: &str,
    key: &str,
) -> anyhow::Result<()> {
    let ttl = inner.config.lease_ttl;
    let started = std::time::Instant::now();
    let (claimed, pool_wait_ms) = inner.store.claim(agent, key, &inner.owner, ttl).await?;
    inner.emit(EventKind::SessionClaimed {
        session: format!("{agent}/{key}"),
        claimed: claimed.is_some(),
        claim_ms: elapsed_ms(started),
        pool_wait_ms,
    });
    let Some(lease) = claimed else {
        return Ok(());
    };
    let result = session_loop(inner, &lease).await;
    if let Err(error) = &result {
        tracing::error!(%error, agent, key, "session failed, releasing its lease");
        inner
            .store
            .release_with_backoff(&lease, inner.config.backoff_base.as_secs_f64())
            .await?;
    }
    result
}

async fn session_loop<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
) -> anyhow::Result<()> {
    let (agent, key) = (lease.agent.as_str(), lease.key.as_str());
    let lease = lease.clone();
    loop {
        if inner.stopping.is_cancelled() {
            inner.store.release(&lease).await?;
            return Ok(());
        }
        let plan = match inner.store.next_turn(&lease).await? {
            NextTurn::Resume { open, events } => {
                tracing::info!(agent, key, turn = %open.turn_id, "resuming an interrupted turn");
                TurnPlan {
                    events,
                    turn_id: open.turn_id,
                    replay: open.entries,
                    next_seq: open.next_seq,
                    fresh: false,
                }
            }
            NextTurn::Fresh(pending) if pending.is_empty() => {
                if inner.store.release_if_idle(&lease).await? {
                    return Ok(());
                }
                continue;
            }
            NextTurn::Fresh(pending) => TurnPlan {
                turn_id: uuid::Uuid::now_v7().to_string(),
                events: pending,
                replay: Vec::new(),
                next_seq: 1,
                fresh: true,
            },
        };
        if !run_one_turn(inner, &lease, plan).await? {
            return Ok(());
        }
    }
}

async fn run_one_turn<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    plan: TurnPlan,
) -> anyhow::Result<bool> {
    let Some(Ready {
        prepared,
        history,
        consumed,
    }) = ready_turn(inner, lease, &plan).await?
    else {
        return Ok(false);
    };
    let (events, lane) = event_messages(&plan.events);
    let steered_before = plan.replay.iter().filter_map(|entry| match entry {
        JournalEntry::Steered { id, .. } => id.parse::<i64>().ok(),
        _ => None,
    });
    let steering = SessionSteering::new(
        inner.store.clone(),
        lease.clone(),
        consumed.iter().copied().chain(steered_before),
        inner
            .notifications
            .watch(Channel::Inbox, live_topic(&lease.agent, &lease.key)),
    );
    let turn_cancel = inner.turns.child_token();
    let (journal, journal_writer) = TurnJournal::spawn(
        inner.store.clone(),
        lease.clone(),
        plan.turn_id.clone(),
        plan.next_seq,
        turn_cancel.clone(),
    );
    let (live, live_writer) = LiveSink::spawn(
        inner.store.clone(),
        inner.notifications.clone(),
        lease.agent.clone(),
        lease.key.clone(),
    );
    let session = format!("{}/{}", lease.agent, lease.key);
    let cache_key = inner.cache_keys.key(&lease.agent, &lease.key);
    let count_model_call = || inner.cache_keys.record_call(&lease.agent, &cache_key);

    let result = run_turn(
        &TurnDeps {
            model: &inner.model,
            host: &prepared.host,
            steering: &steering,
            journal: &journal,
            sink: &TeeSink {
                live: &live,
                instrumentation: inner.instrumentation.as_ref(),
                on_model_call: Some(&count_model_call),
            },
            cancel: &turn_cancel,
        },
        TurnInput {
            session: &session,
            cache_key: &cache_key,
            turn: &plan.turn_id,
            system: &prepared.system,
            external_tools: &prepared.tools,
            history: &history,
            context: prepared.context.clone(),
            skills: &prepared.skills,
            events,
            workspace: &prepared.workspace,
            replay: plan.replay,
        },
        &TurnSettings {
            model: &prepared.model,
            first_call_effort: prepared.first_call_effort,
            effort: prepared.effort,
            max_output_tokens: prepared.max_output_tokens,
            max_model_calls: prepared.max_model_calls,
            max_turn_tokens: prepared.max_turn_tokens,
            recite: prepared.recite,
            lane,
            compaction: None,
        },
    )
    .await;
    drop(live);
    live_writer.await?;
    drop(journal);
    journal_writer.await?;

    let committed = finish_turn(inner, lease, plan.turn_id, consumed, result).await?;
    if committed {
        crate::compaction::compact_if_over_budget(inner, lease, &prepared).await?;
    }
    Ok(committed)
}

fn event_messages(events: &[PendingEvent]) -> (Vec<Message>, Lane) {
    let messages = events
        .iter()
        .map(|event| Message::user_text(event.text.clone()))
        .collect();
    let lane = if events.iter().any(|event| event.interactive) {
        Lane::Interactive
    } else {
        Lane::Background
    };
    (messages, lane)
}

struct Ready<H> {
    prepared: Prepared<H>,
    history: Vec<Message>,
    consumed: Vec<i64>,
}

async fn ready_turn<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    plan: &TurnPlan,
) -> anyhow::Result<Option<Ready<B::Host>>> {
    let consumed: Vec<i64> = plan.events.iter().map(|event| event.inbox_id).collect();
    let timed_prepare = async {
        let started = std::time::Instant::now();
        let prepared = inner.backend.prepare(&lease.agent, &lease.key).await;
        (prepared, elapsed_ms(started))
    };
    let ((prepared, prepare_ms), begun) = tokio::join!(
        timed_prepare,
        begin_turn(inner, lease, &plan.turn_id, &consumed, plan.fresh)
    );
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, agent = %lease.agent, key = %lease.key, "turn preparation failed");
            inner
                .store
                .release_with_backoff(lease, inner.config.backoff_base.as_secs_f64())
                .await?;
            return Ok(None);
        }
    };
    let Some((history, history_ms, pool_wait_ms)) = begun? else {
        tracing::warn!(agent = %lease.agent, key = %lease.key, "lease lost before the turn started");
        return Ok(None);
    };
    inner.emit(EventKind::TurnPrepared {
        session: format!("{}/{}", lease.agent, lease.key),
        turn: plan.turn_id.clone(),
        prepare_ms,
        history_ms,
        pool_wait_ms,
    });
    Ok(Some(Ready {
        prepared,
        history,
        consumed,
    }))
}

async fn begin_turn<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    turn_id: &str,
    consumed: &[i64],
    fresh: bool,
) -> anyhow::Result<Option<(Vec<Message>, u64, u64)>> {
    let started = std::time::Instant::now();
    let Some((history, pool_wait_ms)) = inner
        .store
        .begin_turn(lease, turn_id, consumed, fresh)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some((history, elapsed_ms(started), pool_wait_ms)))
}

async fn finish_turn<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    turn_id: String,
    mut consumed: Vec<i64>,
    result: Result<newt_core::turn::TurnOutcome, newt_core::turn::TurnFailure>,
) -> anyhow::Result<bool> {
    let (committed, steered, effects, failure) = match result {
        Ok(outcome) => (outcome.committed, outcome.steered, true, None),
        Err(failure) => (
            failure.committed,
            failure.steered,
            failure.effects,
            Some(failure.error),
        ),
    };
    if matches!(failure, Some(TurnError::Provider(ProviderError::Cancelled))) {
        inner.store.release(lease).await?;
        return Ok(false);
    }
    consumed.extend(steered.iter().filter_map(|id| id.parse::<i64>().ok()));
    if failure.is_none() || effects {
        let committing = std::time::Instant::now();
        let commit = inner
            .store
            .commit_turn(
                lease,
                turn_id.clone(),
                committed,
                consumed.clone(),
                inner.config.lease_ttl,
            )
            .await;
        match commit {
            Ok(()) => inner.emit(EventKind::TurnCommitted {
                session: format!("{}/{}", lease.agent, lease.key),
                turn: turn_id,
                commit_ms: elapsed_ms(committing),
            }),
            Err(CommitError::Fenced) => {
                tracing::warn!(agent = %lease.agent, key = %lease.key, "lease lost before commit, turn discarded");
                return Ok(false);
            }
            Err(CommitError::Database(error)) => return Err(error.into()),
        }
    }
    let Some(error) = failure else {
        return Ok(true);
    };
    tracing::error!(%error, agent = %lease.agent, key = %lease.key, "turn failed");
    let attempts = inner
        .store
        .release_with_backoff(lease, inner.config.backoff_base.as_secs_f64())
        .await?;
    if !effects && attempts >= inner.config.max_attempts {
        tracing::error!(attempts, agent = %lease.agent, key = %lease.key, "giving up on the turn, dead-lettering its events");
        inner
            .store
            .dead_letter(lease, consumed, error.to_string())
            .await?;
    }
    Ok(false)
}
