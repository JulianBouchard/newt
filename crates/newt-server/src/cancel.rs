//! Cancelling a running turn from any instance: the request is a flag on the
//! session row, the owner finds it by notification or at its next heartbeat.

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
