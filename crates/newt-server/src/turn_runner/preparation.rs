use super::ending::turn_events;
use super::retry_later;
use crate::backend::{Backend, Prepared, TurnChoice, TurnRequest};
use crate::events::ServerEventKind;
use crate::runner::{Inner, elapsed_ms};
use crate::sessions::{Lease, PendingEvent};
use newt_core::journal::JournalEntry;
use newt_core::message::Message;
use newt_core::prompt::Lane;
use newt_core::provider::Model;
use serde_json::Value;

pub(super) struct TurnPlan {
    pub(super) turn_id: String,
    pub(super) events: Vec<PendingEvent>,
    pub(super) replay: Vec<JournalEntry>,
    pub(super) next_seq: i32,
    pub(super) fresh: bool,
    pub(super) state: Option<Value>,
}

/// Applies what the provider chose for the turn to its settings and history.
/// Once a model call of the turn is journaled the turn is fixed: a resume
/// keeps the journaled choice, or the bootstrap's settings with no reset when
/// none was journaled, whatever `/turn` answers now. Before that, an attempt
/// is a fresh start and the provider's answer counts. Returns whether the
/// history was reset, and the choice to journal.
pub(super) fn apply_choice<H>(
    replay: &[JournalEntry],
    prepared: &mut Prepared<H>,
    history: &mut Vec<Message>,
) -> anyhow::Result<(bool, Option<TurnChoice>)> {
    let journaled = TurnChoice::last_journaled(replay)?;
    let answered = replay
        .iter()
        .any(|entry| matches!(entry, JournalEntry::ModelResponse { .. }));
    let (choice, to_journal) = match (journaled, answered) {
        (Some(choice), true) => (Some(choice), None),
        (None, true) => (None, None),
        (Some(_), false) => {
            let choice = prepared.choice.clone().unwrap_or_else(|| TurnChoice {
                model: prepared.settings.model.clone(),
                first_call_effort: prepared.settings.first_call_effort,
                effort: prepared.settings.effort,
                reset: false,
            });
            (Some(choice.clone()), Some(choice))
        }
        (None, false) => (prepared.choice.clone(), prepared.choice.clone()),
    };
    let Some(choice) = choice else {
        return Ok((false, None));
    };
    prepared.settings.model.clone_from(&choice.model);
    prepared.settings.first_call_effort = choice.first_call_effort;
    prepared.settings.effort = choice.effort;
    if choice.reset {
        history.clear();
    }
    Ok((choice.reset, to_journal))
}

pub(super) fn event_messages(events: &[PendingEvent]) -> (Vec<Message>, Lane) {
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

pub(super) struct Ready<H> {
    pub(super) prepared: Prepared<H>,
    pub(super) history: Vec<Message>,
    pub(super) consumed: Vec<i64>,
}

pub(super) async fn ready_turn<B: Backend, M: Model>(
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
            retry_later(inner, lease).await?;
            return Ok(None);
        }
    };
    let Some((history, history_ms, pool_wait_ms)) = begun? else {
        tracing::warn!(agent = %lease.agent, key = %lease.key, "lease lost before the turn started");
        return Ok(None);
    };
    inner.emit(ServerEventKind::TurnPrepared {
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

pub(super) async fn begin_turn<B: Backend, M: Model>(
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
