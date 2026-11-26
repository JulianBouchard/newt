use super::retry_later;
use crate::backend::{Backend, Prepared, SessionState, TurnEvent, TurnState};
use crate::counters::Counter;
use crate::events::ServerEventKind;
use crate::runner::{Inner, elapsed_ms};
use crate::sessions::{CommitError, Lease, PendingEvent, TurnCommit};
use newt_core::message::Message;
use newt_core::provider::{Model, ProviderError};
use newt_core::turn::TurnError;
use serde::Serialize;
use serde_json::Value;

/// What a turn's commit needs from its preparation: the provider's new
/// session state and, when the provider wants one, the turn-end callback.
pub(super) struct Ending {
    pub(super) agent: String,
    pub(super) key: String,
    pub(super) previous_state: Option<Value>,
    pub(super) state: SessionState,
    pub(super) turn_end: bool,
    pub(super) turn_state: TurnState,
    pub(super) reset: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    Completed,
    Failed,
    DeadLettered,
}

/// The turn-end callback body, without `events`: the statement that stores
/// it adds the events the turn consumed, steered ones included.
#[derive(Debug, Serialize)]
pub(super) struct TurnEndPayload<'a> {
    pub(super) agent: &'a str,
    pub(super) session: &'a str,
    pub(super) turn: &'a str,
    pub(super) outcome: Outcome,
    pub(super) state: Option<Value>,
    pub(super) turn_state: Value,
    pub(super) reply: Option<String>,
}

impl Ending {
    pub(super) fn of<H>(
        lease: &Lease,
        previous_state: Option<Value>,
        prepared: &Prepared<H>,
        reset: bool,
    ) -> Self {
        Self {
            agent: lease.agent.clone(),
            key: lease.key.clone(),
            previous_state,
            state: prepared.state.clone(),
            turn_end: prepared.turn_end,
            turn_state: prepared.turn_state.clone(),
            reset,
        }
    }

    pub(super) fn payload(
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

    pub(super) fn commit(
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
            reset: self.reset,
        })
    }
}

pub(super) fn turn_events(events: &[PendingEvent]) -> Vec<TurnEvent> {
    events
        .iter()
        .map(|event| TurnEvent {
            id: event.event_id.clone(),
            meta: event.meta.clone(),
        })
        .collect()
}

pub(super) async fn finish_turn<B: Backend, M: Model>(
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
    let cancelled = matches!(failure, Some(TurnError::Provider(ProviderError::Cancelled)));
    inner.metrics.count(match &failure {
        None => Counter::TurnsCompleted,
        Some(_) if cancelled => Counter::TurnsCancelled,
        Some(_) => Counter::TurnsFailed,
    });
    if cancelled && inner.turns.is_cancelled() {
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
                inner.emit(ServerEventKind::TurnCommitted {
                    session: format!("{}/{}", lease.agent, lease.key),
                    turn: turn_id.clone(),
                    commit_ms: elapsed_ms(committing),
                });
                if wants_turn_end {
                    inner.turn_ends.notify_one();
                }
            }
            Err(CommitError::Fenced) => {
                inner.metrics.count(Counter::CommitsFenced);
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
    let attempts = if cancelled && !effects {
        inner.store.release(lease).await?;
        0
    } else {
        retry_later(inner, lease).await?
    };
    if !effects && (cancelled || attempts >= inner.config.max_attempts) {
        tracing::error!(attempts, cancelled, agent = %lease.agent, key = %lease.key, "giving up on the turn, dead-lettering its events");
        inner
            .metrics
            .count_many(Counter::DeadLetters, consumed.len());
        inner
            .store
            .dead_letter(
                lease,
                consumed,
                if cancelled {
                    "cancelled".to_owned()
                } else {
                    error.to_string()
                },
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
            reset: false,
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
