//! Cancelling a running turn from any instance: the request is a flag on the
//! session row, the owner finds it by notification or at its next heartbeat.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio_util::sync::CancellationToken;

use crate::sessions::Owned;
use crate::store::{CANCEL_CHANNEL, Store, live_topic};

/// What a cancel request found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelRequest {
    SessionNotFound,
    /// No turn holds a live lease, or `turn` names another one.
    NotRunning,
    /// The turn the flag now names.
    Requested(String),
}

const REQUEST: &str = "with running as ( \
       select j.turn_id from sessions s \
       join turn_journal j on j.agent = s.agent and j.key = s.key and j.kind = 'start' \
       where s.agent = $1 and s.key = $2 \
         and s.lease_owner is not null and s.lease_expires_at > now() \
       order by j.created_at desc limit 1), \
     flagged as ( \
       update sessions s set cancel_turn = r.turn_id, \
         cancel_requested_at = case when s.cancel_turn = r.turn_id \
           then s.cancel_requested_at else now() end \
       from running r \
       where s.agent = $1 and s.key = $2 and ($3::text is null or $3 = r.turn_id) \
       returning r.turn_id), \
     notified as (select pg_notify($4, $5) from flagged) \
     select exists (select 1 from sessions where agent = $1 and key = $2), \
       (select turn_id from flagged), (select count(*) from notified)";

impl Store {
    /// Flags the session's running turn for cancellation and notifies every
    /// instance. `turn`, when given, must be the running one.
    pub async fn request_cancel(
        &self,
        agent: &str,
        key: &str,
        turn: Option<&str>,
    ) -> Result<CancelRequest, sqlx::Error> {
        let (known, flagged, _notified): (bool, Option<String>, i64) = sqlx::query_as(REQUEST)
            .bind(agent)
            .bind(key)
            .bind(turn)
            .bind(CANCEL_CHANNEL)
            .bind(live_topic(agent, key))
            .fetch_one(self.pool())
            .await?;
        Ok(match (known, flagged) {
            (false, _) => CancelRequest::SessionNotFound,
            (true, None) => CancelRequest::NotRunning,
            (true, Some(turn)) => CancelRequest::Requested(turn),
        })
    }

    /// The turn a cancel was requested for, when `owner` still holds the
    /// session.
    pub fn requested_cancel(&self, owner: &str, agent: &str, key: &str) -> Owned<Option<String>> {
        let pool = self.pool().clone();
        let (owner, agent, key) = (owner.to_owned(), agent.to_owned(), key.to_owned());
        Box::pin(async move {
            let turn: Option<Option<String>> = sqlx::query_scalar(
                "select cancel_turn from sessions \
                 where agent = $1 and key = $2 and lease_owner = $3",
            )
            .bind(agent)
            .bind(key)
            .bind(owner)
            .fetch_optional(&pool)
            .await?;
            Ok(turn.flatten())
        })
    }
}

type Session = (String, String);

/// The turns running on this instance, by session, so a cancel request
/// that reaches this instance can stop the one it names.
#[derive(Debug, Default)]
pub(crate) struct RunningTurns(Mutex<HashMap<Session, (String, CancellationToken)>>);

impl RunningTurns {
    fn turns(&self) -> MutexGuard<'_, HashMap<Session, (String, CancellationToken)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a turn of this session is running on this instance.
    pub(crate) fn is_running(&self, agent: &str, key: &str) -> bool {
        self.turns()
            .contains_key(&(agent.to_owned(), key.to_owned()))
    }

    /// Cancels the turn `turn` of this session when it runs on this
    /// instance. False when another turn, or none, is running here, or it
    /// was already cancelled.
    pub(crate) fn cancel(&self, agent: &str, key: &str, turn: &str) -> bool {
        self.turns()
            .get(&(agent.to_owned(), key.to_owned()))
            .filter(|(running, _)| running == turn)
            .is_some_and(|(_, token)| {
                let live = !token.is_cancelled();
                token.cancel();
                live
            })
    }

    pub(crate) fn track(
        self: &Arc<Self>,
        agent: &str,
        key: &str,
        turn: &str,
        token: CancellationToken,
    ) -> TrackedTurn {
        let session = (agent.to_owned(), key.to_owned());
        self.turns()
            .insert(session.clone(), (turn.to_owned(), token));
        TrackedTurn {
            turns: Arc::clone(self),
            session,
        }
    }
}

/// A turn that can be cancelled by session; dropping it forgets the turn.
pub(crate) struct TrackedTurn {
    turns: Arc<RunningTurns>,
    session: Session,
}

impl Drop for TrackedTurn {
    fn drop(&mut self) {
        self.turns.turns().remove(&self.session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_tracked_turn_cancels_its_token_once_and_only_by_its_id() {
        let running = Arc::new(RunningTurns::default());
        assert!(!running.cancel("assistant", "a", "t1"));
        let token = CancellationToken::new();
        let tracked = running.track("assistant", "a", "t1", token.clone());
        assert!(running.is_running("assistant", "a"));
        assert!(!running.cancel("assistant", "b", "t1"));
        assert!(!running.cancel("assistant", "a", "t0"));
        assert!(!token.is_cancelled());
        assert!(running.cancel("assistant", "a", "t1"));
        assert!(token.is_cancelled());
        assert!(!running.cancel("assistant", "a", "t1"));
        drop(tracked);
        assert!(!running.is_running("assistant", "a"));
        assert!(!running.cancel("assistant", "a", "t1"));
    }
}
