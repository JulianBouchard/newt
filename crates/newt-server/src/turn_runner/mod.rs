mod ending;
mod preparation;

use std::panic::AssertUnwindSafe;

use crate::backend::Backend;
use crate::counters::Counter;
use crate::journal::TurnJournal;
use crate::live::{Channel, Watch};
use crate::runner::{Inner, elapsed_ms};
use crate::sessions::Lease;
use crate::sinks::{LiveSink, TeeSink};
use crate::steering::SessionSteering;
use crate::store::live_topic;
use crate::turn_start::NextTurn;
use ending::{Ending, finish_turn};
use futures_util::FutureExt;
use newt_core::event::EventKind;
use newt_core::journal::{Journal, JournalEntry};
use newt_core::provider::Model;
use newt_core::turn::{TurnDeps, TurnInput, run_turn};
use preparation::{Ready, TurnPlan, apply_choice, event_messages, ready_turn};

pub(crate) async fn run_session<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    agent: &str,
    key: &str,
) -> anyhow::Result<()> {
    let ttl = inner.config.lease_ttl;
    let started = std::time::Instant::now();
    let changes = inbox_watch(inner, agent, key);
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
    let result = match AssertUnwindSafe(session_loop(inner, &lease, changes))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("the session task panicked")),
    };
    if let Err(error) = &result {
        tracing::error!(%error, agent, key, "session failed, releasing its lease");
        retry_later(inner, &lease).await?;
    }
    result
}

async fn retry_later<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
) -> anyhow::Result<i32> {
    let attempts = inner
        .store
        .release_with_backoff(lease, inner.config.backoff_base.as_secs_f64())
        .await?;
    inner.metrics.count(Counter::Retries);
    Ok(attempts)
}

async fn session_loop<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    first_changes: Watch,
) -> anyhow::Result<()> {
    let (agent, key) = (lease.agent.as_str(), lease.key.as_str());
    let lease = lease.clone();
    let mut changes = first_changes;
    loop {
        if inner.stopping.is_cancelled() {
            inner.store.release(&lease).await?;
            return Ok(());
        }
        let plan = match inner.store.next_turn_or_release(&lease).await? {
            NextTurn::Idle { released: true } => return Ok(()),
            NextTurn::Idle { released: false } => {
                tracing::warn!(agent, key, "lease moved while the session was idle");
                return Ok(());
            }
            NextTurn::Resume {
                open,
                events,
                state,
            } => {
                tracing::info!(agent, key, turn = %open.turn_id, "resuming an interrupted turn");
                TurnPlan {
                    events,
                    turn_id: open.turn_id,
                    replay: open.entries,
                    next_seq: open.next_seq,
                    fresh: false,
                    state,
                }
            }
            NextTurn::Fresh { events, state } => TurnPlan {
                turn_id: uuid::Uuid::now_v7().to_string(),
                events,
                replay: Vec::new(),
                next_seq: 1,
                fresh: true,
                state,
            },
        };
        if !run_one_turn(inner, &lease, plan, changes).await? {
            return Ok(());
        }
        changes = inbox_watch(inner, agent, key);
    }
}

/// A watch on the session's inbox notifications, made before the read that
/// plans a turn so steering hears every event that read did not see.
fn inbox_watch<B: Backend, M: Model>(inner: &Inner<B, M>, agent: &str, key: &str) -> Watch {
    inner
        .notifications
        .watch(Channel::Inbox, live_topic(agent, key))
}

async fn run_one_turn<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    plan: TurnPlan,
    changes: Watch,
) -> anyhow::Result<bool> {
    let Some(Ready {
        mut prepared,
        mut history,
        consumed,
    }) = ready_turn(inner, lease, &plan).await?
    else {
        return Ok(false);
    };
    let (reset, new_choice) = apply_choice(&plan.replay, &mut prepared, &mut history);
    let (events, lane) = event_messages(&plan.events);
    let steered_before = plan.replay.iter().filter_map(|entry| match entry {
        JournalEntry::Steered { id, .. } => id.parse::<i64>().ok(),
        _ => None,
    });
    let steering = SessionSteering::new(
        inner.store.clone(),
        lease.clone(),
        consumed.iter().copied().chain(steered_before),
        inbox_watch(inner, &lease.agent, &lease.key),
        changes,
    );
    let turn_cancel = inner.turns.child_token();
    let _cancellable =
        inner
            .running_turns
            .track(&lease.agent, &lease.key, &plan.turn_id, turn_cancel.clone());
    let (journal, journal_writer) = TurnJournal::spawn(
        inner.store.clone(),
        lease.clone(),
        plan.turn_id.clone(),
        plan.next_seq,
        turn_cancel.clone(),
    );
    if let Some(choice) = new_choice {
        journal.append(JournalEntry::Choice(choice));
    }
    let (live, live_writer) = LiveSink::spawn(
        inner.store.clone(),
        inner.notifications.clone(),
        lease.agent.clone(),
        lease.key.clone(),
        inner.live_stream.contains(&lease.agent),
    );
    let session = format!("{}/{}", lease.agent, lease.key);
    let cache_key = inner.cache_keys.key(&lease.agent, &lease.key);
    let count_model_call = || inner.cache_keys.record_call(&lease.agent, &cache_key);

    inner.metrics.count(Counter::TurnsStarted);
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
        &prepared.settings.turn(lane, reset),
    )
    .await;
    drop(live);
    live_writer.await?;
    drop(journal);
    journal_writer.await?;

    let ending = Ending::of(lease, plan.state, &prepared, reset);
    let committed = finish_turn(inner, lease, plan.turn_id, consumed, result, ending).await?;
    if committed {
        crate::compaction::compact_if_over_budget(inner, lease, &prepared).await?;
    }
    Ok(committed)
}
