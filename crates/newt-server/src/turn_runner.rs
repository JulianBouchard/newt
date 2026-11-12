use newt_core::journal::JournalEntry;
use newt_core::message::Message;
use newt_core::prompt::Lane;
use newt_core::provider::{Model, ProviderError};
use newt_core::turn::{TurnDeps, TurnError, TurnInput, TurnSettings, run_turn};

use crate::backend::Backend;
use crate::journal::TurnJournal;
use crate::runner::Inner;
use crate::sessions::{CommitError, Lease, PendingEvent};
use crate::sinks::{LiveSink, TeeSink};
use crate::steering::SessionSteering;

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
    let Some(lease) = inner.store.claim(agent, key, &inner.owner, ttl).await? else {
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
        let plan = if let Some(open) = inner.store.open_turn(&lease).await? {
            tracing::info!(agent, key, turn = %open.turn_id, "resuming an interrupted turn");
            TurnPlan {
                events: inner.store.events_by_ids(&lease, &open.inbox_ids).await?,
                turn_id: open.turn_id,
                replay: open.entries,
                next_seq: open.next_seq,
                fresh: false,
            }
        } else {
            let pending = inner.store.pending_events(&lease).await?;
            if pending.is_empty() {
                if inner.store.release_if_idle(&lease).await? {
                    return Ok(());
                }
                continue;
            }
            TurnPlan {
                turn_id: uuid::Uuid::now_v7().to_string(),
                events: pending,
                replay: Vec::new(),
                next_seq: 1,
                fresh: true,
            }
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
    let prepared = match inner.backend.prepare(&lease.agent, &lease.key).await {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, agent = %lease.agent, key = %lease.key, "turn preparation failed");
            inner
                .store
                .release_with_backoff(lease, inner.config.backoff_base.as_secs_f64())
                .await?;
            return Ok(false);
        }
    };
    let consumed: Vec<i64> = plan.events.iter().map(|event| event.inbox_id).collect();
    if plan.fresh
        && !inner
            .store
            .start_turn(lease, &plan.turn_id, &consumed)
            .await?
    {
        tracing::warn!(agent = %lease.agent, key = %lease.key, "lease lost before the turn started");
        return Ok(false);
    }
    let history = inner.store.history(lease).await?;
    let events: Vec<Message> = plan
        .events
        .iter()
        .map(|event| Message::user_text(event.text.clone()))
        .collect();
    let lane = if plan.events.iter().any(|event| event.interactive) {
        Lane::Interactive
    } else {
        Lane::Background
    };
    let steered_before = plan.replay.iter().filter_map(|entry| match entry {
        JournalEntry::Steered { id, .. } => id.parse::<i64>().ok(),
        _ => None,
    });
    let steering = SessionSteering::new(
        inner.store.clone(),
        lease.clone(),
        consumed.iter().copied().chain(steered_before),
        inner.notifications.subscribe(),
    );
    let turn_cancel = inner.turns.child_token();
    let (journal, journal_writer) = TurnJournal::spawn(
        inner.store.clone(),
        lease.clone(),
        plan.turn_id.clone(),
        plan.next_seq,
        turn_cancel.clone(),
    );
    let (live, live_writer) =
        LiveSink::spawn(inner.store.clone(), lease.agent.clone(), lease.key.clone());
    let cache_key = format!("{}/{}", lease.agent, lease.key);

    let result = run_turn(
        &TurnDeps {
            model: &inner.model,
            host: &prepared.host,
            steering: &steering,
            journal: &journal,
            sink: &TeeSink {
                live: &live,
                instrumentation: inner.instrumentation.as_ref(),
            },
            cancel: &turn_cancel,
        },
        TurnInput {
            session: &cache_key,
            turn: &plan.turn_id,
            system: &prepared.system,
            external_tools: &prepared.tools,
            history: &history,
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
            lane,
        },
    )
    .await;
    drop(live);
    live_writer.await?;
    drop(journal);
    journal_writer.await?;

    finish_turn(inner, lease, plan.turn_id, consumed, result).await
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
        match inner
            .store
            .commit_turn(
                lease,
                turn_id,
                committed,
                consumed.clone(),
                inner.config.lease_ttl,
            )
            .await
        {
            Ok(()) => {}
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
