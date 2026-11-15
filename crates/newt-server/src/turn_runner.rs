use newt_core::event::EventKind;
use newt_core::journal::JournalEntry;
use newt_core::message::Message;
use newt_core::prompt::Lane;
use newt_core::provider::{Model, ProviderError};
use newt_core::turn::{TurnDeps, TurnError, TurnInput, TurnSettings, run_turn};
use serde::Serialize;
use serde_json::Value;

use crate::backend::{Backend, Prepared, SessionState, TurnEvent, TurnRequest, TurnState};
use crate::journal::TurnJournal;
use crate::live::{Channel, Watch};
use crate::runner::{Inner, elapsed_ms};
use crate::sessions::{CommitError, Lease, PendingEvent, TurnCommit};
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
    state: Option<Value>,
}

/// What a turn's commit needs from its preparation: the provider's new
/// session state and, when the provider wants one, the turn-end callback.
struct Ending {
    agent: String,
    key: String,
    previous_state: Option<Value>,
    state: SessionState,
    turn_end: bool,
    turn_state: TurnState,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Completed,
    Failed,
    DeadLettered,
}

/// The turn-end callback body, without `events`: the statement that stores
/// it adds the events the turn consumed, steered ones included.
#[derive(Debug, Serialize)]
struct TurnEndPayload<'a> {
    agent: &'a str,
    session: &'a str,
    turn: &'a str,
    outcome: Outcome,
    state: Option<Value>,
    turn_state: Value,
    reply: Option<String>,
}

impl Ending {
    fn payload(
        &self,
        turn_id: &str,
        outcome: Outcome,
        reply: Option<String>,
    ) -> Result<Option<Value>, serde_json::Error> {
        if !self.turn_end {
            return Ok(None);
        }
        let state = match outcome {
            Outcome::DeadLettered => self.previous_state.clone(),
            Outcome::Completed | Outcome::Failed => self.state.get(),
        };
        serde_json::to_value(TurnEndPayload {
            agent: &self.agent,
            session: &self.key,
            turn: turn_id,
            outcome,
            state,
            turn_state: self.turn_state.get(),
            reply,
        })
        .map(Some)
    }

    fn commit(
        &self,
        turn_id: String,
        messages: Vec<Message>,
        consumed: Vec<i64>,
        failed: bool,
        reply: Option<String>,
    ) -> Result<TurnCommit, serde_json::Error> {
        let outcome = if failed {
            Outcome::Failed
        } else {
            Outcome::Completed
        };
        let turn_end = self.payload(&turn_id, outcome, reply)?;
        Ok(TurnCommit {
            turn_id,
            messages,
            consumed,
            provider_state: self.state.changed(),
            turn_end,
        })
    }
}

fn turn_events(events: &[PendingEvent]) -> Vec<TurnEvent> {
    events
        .iter()
        .map(|event| TurnEvent {
            id: event.event_id.clone(),
            meta: event.meta.clone(),
        })
        .collect()
}

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
    let result = session_loop(inner, &lease, changes).await;
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
        inbox_watch(inner, &lease.agent, &lease.key),
        changes,
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

    let ending = Ending {
        agent: lease.agent.clone(),
        key: lease.key.clone(),
        previous_state: plan.state,
        state: prepared.state.clone(),
        turn_end: prepared.turn_end,
        turn_state: prepared.turn_state.clone(),
    };
    let committed = finish_turn(inner, lease, plan.turn_id, consumed, result, ending).await?;
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
        let request = TurnRequest {
            events: turn_events(&plan.events),
            state: plan.state.clone(),
        };
        let prepared = inner
            .backend
            .prepare(&lease.agent, &lease.key, &request)
            .await;
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
    ending: Ending,
) -> anyhow::Result<bool> {
    let (committed, steered, effects, failure, reply) = match result {
        Ok(outcome) => (
            outcome.committed,
            outcome.steered,
            true,
            None,
            outcome.reply,
        ),
        Err(failure) => (
            failure.committed,
            failure.steered,
            failure.effects,
            Some(failure.error),
            None,
        ),
    };
    if matches!(failure, Some(TurnError::Provider(ProviderError::Cancelled))) {
        inner.store.release(lease).await?;
        return Ok(false);
    }
    consumed.extend(steered.iter().filter_map(|id| id.parse::<i64>().ok()));
    if failure.is_none() || effects {
        let committing = std::time::Instant::now();
        let wants_turn_end = ending.turn_end;
        let turn = ending.commit(
            turn_id.clone(),
            committed,
            consumed.clone(),
            failure.is_some(),
            reply,
        )?;
        let commit = inner
            .store
            .commit_turn(lease, turn, inner.config.lease_ttl)
            .await;
        match commit {
            Ok(()) => {
                inner.emit(EventKind::TurnCommitted {
                    session: format!("{}/{}", lease.agent, lease.key),
                    turn: turn_id.clone(),
                    commit_ms: elapsed_ms(committing),
                });
                if wants_turn_end {
                    inner.turn_ends.notify_one();
                }
            }
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
            .dead_letter(
                lease,
                consumed,
                error.to_string(),
                ending.payload(&turn_id, Outcome::DeadLettered, None)?,
            )
            .await?;
        if ending.turn_end {
            inner.turn_ends.notify_one();
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    fn ending() -> Ending {
        let state = SessionState::starting_from(Some(serde_json::json!({ "step": 1 })));
        if let Value::Object(patch) = serde_json::json!({ "step": 2 }) {
            state.patch(&patch);
        }
        Ending {
            agent: "assistant".into(),
            key: "ana".into(),
            previous_state: Some(serde_json::json!({ "step": 1 })),
            state,
            turn_end: true,
            turn_state: TurnState::default(),
        }
    }

    #[test]
    fn a_failed_turn_reports_the_state_it_commits_and_a_dead_letter_the_previous_one() {
        let ending = ending();
        let failed = ending
            .commit("t1".into(), Vec::new(), Vec::new(), true, None)
            .unwrap();
        assert_eq!(
            failed.provider_state,
            Some(serde_json::json!({ "step": 2 }))
        );
        assert_eq!(
            failed.turn_end.as_ref().unwrap()["state"],
            serde_json::json!({ "step": 2 })
        );

        assert_eq!(failed.turn_end.as_ref().unwrap()["reply"], Value::Null);

        let dead = ending
            .payload("t1", Outcome::DeadLettered, None)
            .unwrap()
            .unwrap();
        assert_eq!(dead["outcome"], "dead_lettered");
        assert_eq!(dead["state"], serde_json::json!({ "step": 1 }));
    }

    #[test]
    fn no_turn_end_is_built_when_the_provider_did_not_ask() {
        let ending = Ending {
            turn_end: false,
            ..ending()
        };
        assert_eq!(
            ending.payload("t1", Outcome::Completed, None).unwrap(),
            None
        );
    }
}
