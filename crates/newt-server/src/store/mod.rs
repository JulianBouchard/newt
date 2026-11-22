mod accept;
mod events;
mod live_feed;

pub use accept::LiveDetail;
pub use events::{
    AcceptError, Accepted, DEFAULT_MAX_PENDING_EVENTS, InboundEvent, InboundKind, Policy, Priority,
};
pub use live_feed::LiveEvent;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};

pub const LIVE_CHANNEL: &str = "newt_live";
pub const INBOX_CHANNEL: &str = "newt_inbox";
pub const CANCEL_CHANNEL: &str = "newt_cancel";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub lease_owner: Option<String>,
    pub lease_epoch: i64,
    pub pending_events: i64,
    pub history_entries: i64,
    pub live_seq: i64,
}

#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    max_pending: Arc<HashMap<String, usize>>,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            max_pending: Arc::default(),
        }
    }

    /// Per-agent caps on a session's unconsumed events; an agent not listed
    /// gets [`DEFAULT_MAX_PENDING_EVENTS`].
    #[must_use]
    pub fn with_max_pending(mut self, caps: HashMap<String, usize>) -> Self {
        self.max_pending = Arc::new(caps);
        self
    }

    fn max_pending(&self, agent: &str) -> i64 {
        let cap = self
            .max_pending
            .get(agent)
            .copied()
            .unwrap_or(DEFAULT_MAX_PENDING_EVENTS);
        i64::try_from(cap).unwrap_or(i64::MAX)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// A connection, and the milliseconds the pool took to hand it over.
    pub async fn connection(&self) -> Result<(PoolConnection<Postgres>, u64), sqlx::Error> {
        timed_connection(&self.pool).await
    }

    pub async fn session_status(
        &self,
        agent: &str,
        key: &str,
    ) -> Result<Option<SessionStatus>, sqlx::Error> {
        let row: Option<(Option<String>, i64, i64, i64, i64)> = sqlx::query_as(
            "select s.lease_owner, s.lease_epoch, \
               (select count(*) from inbox i where i.agent = s.agent and i.key = s.key and i.consumed_at is null), \
               (select count(*) from history h where h.agent = s.agent and h.key = s.key), \
               s.next_live_seq \
             from sessions s where s.agent = $1 and s.key = $2",
        )
        .bind(agent)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(lease_owner, lease_epoch, pending_events, history_entries, live_seq)| SessionStatus {
                lease_owner,
                lease_epoch,
                pending_events,
                history_entries,
                live_seq,
            },
        ))
    }
}

pub fn live_topic(agent: &str, key: &str) -> String {
    format!("{agent}/{key}")
}

/// A live notification's payload: the session's topic and the sequence number
/// of its newest frame, so an instance can tell its own frames from others'.
/// Session keys never contain `/`, so the last segment is always the number.
pub fn live_payload(agent: &str, key: &str, seq: i64) -> String {
    format!("{agent}/{key}/{seq}")
}

/// A connection from `pool`, and the milliseconds spent waiting for it.
pub async fn timed_connection(
    pool: &PgPool,
) -> Result<(PoolConnection<Postgres>, u64), sqlx::Error> {
    let started = std::time::Instant::now();
    let connection = pool.acquire().await?;
    let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok((connection, waited))
}
