use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::pool::PoolConnection;
use sqlx::types::Json;
use sqlx::{PgPool, Postgres};

pub const LIVE_CHANNEL: &str = "newt_live";
pub const INBOX_CHANNEL: &str = "newt_inbox";
pub const CANCEL_CHANNEL: &str = "newt_cancel";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundKind {
    #[default]
    Message,
    Signal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    Steer,
    Queue,
    Collect,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    #[default]
    Interactive,
    Background,
}

impl InboundEvent {
    /// A sha256 over the event's content, `text` and `meta`, with object keys
    /// sorted so the same content always hashes the same.
    #[must_use]
    pub fn content_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        let mut canonical = String::new();
        push_canonical(
            &json!({ "text": self.text, "meta": self.meta }),
            &mut canonical,
        );
        Sha256::digest(canonical.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }
}

fn push_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                push_canonical(&fields[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

impl InboundKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Signal => "signal",
        }
    }
}

impl Policy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
            Self::Collect => "collect",
        }
    }
}

impl Priority {
    fn rank(self) -> i16 {
        match self {
            Self::Interactive => 0,
            Self::Background => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundEvent {
    pub id: String,
    #[serde(default)]
    pub kind: InboundKind,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub priority: Priority,
    pub text: String,
    #[serde(default)]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub inbox_id: i64,
    pub duplicate: bool,
    /// Milliseconds spent waiting for a database connection.
    #[serde(skip)]
    pub pool_wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveEvent {
    pub seq: i64,
    pub event: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub lease_owner: Option<String>,
    pub lease_epoch: i64,
    pub pending_events: i64,
    pub history_entries: i64,
    pub live_seq: i64,
}

/// How many unconsumed events a session may hold when its agent sets no cap.
pub const DEFAULT_MAX_PENDING_EVENTS: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("the session already holds {max_pending} unconsumed events")]
    Full { max_pending: i64 },
    #[error("an event with this id and different content was already accepted")]
    Conflict,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
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

    pub async fn accept_event(
        &self,
        agent: &str,
        key: &str,
        event: &InboundEvent,
        detail: impl Into<LiveDetail>,
    ) -> Result<Accepted, AcceptError> {
        let detail = detail.into();
        let max_pending = self.max_pending(agent);
        let (mut connection, pool_wait_ms) = self.connection().await?;
        let hash = event.content_hash();
        let (inbox_id, fresh, _received, _notified, stored_hash): (
            Option<i64>,
            bool,
            i64,
            i64,
            Option<String>,
        ) = sqlx::query_as(ACCEPT)
            .bind(agent)
            .bind(key)
            .bind(&event.id)
            .bind(event.kind.as_str())
            .bind(event.policy.as_str())
            .bind(event.priority.rank())
            .bind(Json(json!({ "text": event.text, "meta": event.meta })))
            .bind(INBOX_CHANNEL)
            .bind(LIVE_CHANNEL)
            .bind(live_topic(agent, key))
            .bind(detail.meta)
            .bind(max_pending)
            .bind(&hash)
            .bind(detail.text)
            .fetch_one(&mut *connection)
            .await?;
        let (inbox_id, stored_hash) = match inbox_id {
            Some(inbox_id) => (inbox_id, stored_hash),
            None => sqlx::query_as::<_, (i64, Option<String>)>(
                "select id, payload_hash from inbox where agent = $1 and key = $2 and event_id = $3",
            )
            .bind(agent)
            .bind(key)
            .bind(&event.id)
            .fetch_optional(&mut *connection)
            .await?
            .ok_or(AcceptError::Full { max_pending })?,
        };
        if !fresh && stored_hash.is_some_and(|stored| stored != hash) {
            return Err(AcceptError::Conflict);
        }
        Ok(Accepted {
            inbox_id,
            duplicate: !fresh,
            pool_wait_ms,
        })
    }

    pub async fn append_live_batch(
        &self,
        agent: &str,
        key: &str,
        events: Vec<Value>,
        notify_remote: bool,
    ) -> Result<(), sqlx::Error> {
        if events.is_empty() {
            return Ok(());
        }
        let count = i64::try_from(events.len()).unwrap_or(i64::MAX);
        let events: Vec<Json<Value>> = events.into_iter().map(Json).collect();
        let statement = if notify_remote {
            APPEND_LIVE_AND_NOTIFY
        } else {
            APPEND_LIVE
        };
        let mut query = sqlx::query(statement)
            .bind(agent)
            .bind(key)
            .bind(count)
            .bind(events);
        if notify_remote {
            query = query.bind(LIVE_CHANNEL).bind(live_topic(agent, key));
        }
        query.execute(&self.pool).await?;
        Ok(())
    }

    pub async fn notify_live(&self, agent: &str, key: &str) -> Result<(), sqlx::Error> {
        sqlx::query("select pg_notify($1, $2)")
            .bind(LIVE_CHANNEL)
            .bind(live_topic(agent, key))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn live_since(
        &self,
        agent: &str,
        key: &str,
        cursor: i64,
        limit: i64,
    ) -> Result<Vec<LiveEvent>, sqlx::Error> {
        let rows: Vec<(i64, Json<Value>)> = sqlx::query_as(
            "select seq, event from live_events \
             where agent = $1 and key = $2 and seq > $3 order by seq limit $4",
        )
        .bind(agent)
        .bind(key)
        .bind(cursor)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(seq, Json(event))| LiveEvent { seq, event })
            .collect())
    }

    /// The frames a client resuming after `cursor` can no longer get because
    /// retention deleted them, as the first and last missing sequence numbers.
    pub async fn live_gap(
        &self,
        agent: &str,
        key: &str,
        cursor: i64,
    ) -> Result<Option<(i64, i64)>, sqlx::Error> {
        if cursor <= 0 {
            return Ok(None);
        }
        let (oldest, last): (Option<i64>, Option<i64>) = sqlx::query_as(
            "select (select min(seq) from live_events where agent = $1 and key = $2), \
                    (select next_live_seq from sessions where agent = $1 and key = $2)",
        )
        .bind(agent)
        .bind(key)
        .fetch_one(&self.pool)
        .await?;
        let Some(last) = last.filter(|last| *last > cursor) else {
            return Ok(None);
        };
        let first_kept = oldest.unwrap_or(last + 1);
        Ok((first_kept > cursor + 1).then_some((cursor + 1, first_kept - 1)))
    }

    pub async fn received_seq(
        &self,
        agent: &str,
        key: &str,
        event_id: &str,
    ) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar(
            "select seq from live_events \
             where agent = $1 and key = $2 \
               and event ->> 'type' = 'received' and event ->> 'event_id' = $3 \
             order by seq desc limit 1",
        )
        .bind(agent)
        .bind(key)
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await
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

/// What an accepted event's `received` live frame carries besides its ids: the event's
/// `meta` (`live_meta`) and its text (`live_stream`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveDetail {
    pub meta: bool,
    pub text: bool,
}

impl From<bool> for LiveDetail {
    fn from(meta: bool) -> Self {
        Self { meta, text: false }
    }
}

const ACCEPT: &str = "with gate as (select newt_locked_pending_events($1, $2) as pending), \
     added as ( \
       insert into inbox (agent, key, event_id, kind, policy, priority, payload, payload_hash) \
       select $1, $2, $3, $4, $5, $6, $7, $13 from gate where gate.pending < $12 \
       on conflict (agent, key, event_id) do nothing returning id), \
     session as ( \
       insert into sessions (agent, key, next_live_seq) values ($1, $2, 1) \
       on conflict (agent, key) do update set next_live_seq = sessions.next_live_seq + 1 \
         where exists (select 1 from added) \
       returning next_live_seq), \
     existing as (select id, payload_hash from inbox where agent = $1 and key = $2 and event_id = $3), \
     received as ( \
       insert into live_events (agent, key, seq, event) \
       select $1, $2, session.next_live_seq, \
         jsonb_build_object('type', 'received', 'event_id', $3::text, 'inbox_id', added.id) \
           || case when $11 and jsonb_typeof($7::jsonb -> 'meta') not in ('null') \
                then jsonb_build_object('meta', $7::jsonb -> 'meta') else '{}'::jsonb end \
           || case when $14 then jsonb_build_object('text', $7::jsonb -> 'text') else '{}'::jsonb end \
       from session, added returning 1), \
     notified as (select pg_notify($8, $10), pg_notify($9, $10) from added) \
     select coalesce((select id from added), (select id from existing)), \
       exists (select 1 from added), (select count(*) from received), \
       (select count(*) from notified), \
       (select payload_hash from existing)";

const APPEND_LIVE: &str = "with relaxed as (select set_config('synchronous_commit', 'off', true)), \
     bumped as ( \
       update sessions set next_live_seq = next_live_seq + $3 \
       where agent = $1 and key = $2 returning next_live_seq), \
     added as ( \
       insert into live_events (agent, key, seq, event) \
       select $1, $2, (select next_live_seq from bumped) - $3 + ordinality, event \
       from unnest($4::jsonb[]) with ordinality as batch(event, ordinality) \
       returning 1) \
     select (select count(*) from relaxed), (select count(*) from added)";

const APPEND_LIVE_AND_NOTIFY: &str = "with relaxed as (select set_config('synchronous_commit', 'off', true)), \
     bumped as ( \
       update sessions set next_live_seq = next_live_seq + $3 \
       where agent = $1 and key = $2 returning next_live_seq), \
     added as ( \
       insert into live_events (agent, key, seq, event) \
       select $1, $2, (select next_live_seq from bumped) - $3 + ordinality, event \
       from unnest($4::jsonb[]) with ordinality as batch(event, ordinality) \
       returning 1) \
     select (select count(*) from relaxed), (select count(*) from added), pg_notify($5, $6)";

pub fn live_topic(agent: &str, key: &str) -> String {
    format!("{agent}/{key}")
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

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    fn event(body: Value) -> InboundEvent {
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn the_hash_ignores_key_order_and_the_id_but_not_content() {
        let base = event(
            json!({ "id": "a", "text": "hi", "meta": { "x": 1, "y": [1, { "b": 2, "a": 3 }] } }),
        );
        let reordered = event(
            json!({ "id": "b", "meta": { "y": [1, { "a": 3, "b": 2 }], "x": 1 }, "text": "hi" }),
        );
        assert_eq!(base.content_hash(), reordered.content_hash());
        assert_eq!(base.content_hash().len(), 64);

        let other_text = event(
            json!({ "id": "a", "text": "ho", "meta": { "x": 1, "y": [1, { "b": 2, "a": 3 }] } }),
        );
        let other_meta = event(
            json!({ "id": "a", "text": "hi", "meta": { "x": 2, "y": [1, { "b": 2, "a": 3 }] } }),
        );
        let no_meta = event(json!({ "id": "a", "text": "hi" }));
        let hashes = [
            base.content_hash(),
            other_text.content_hash(),
            other_meta.content_hash(),
            no_meta.content_hash(),
        ];
        for (index, hash) in hashes.iter().enumerate() {
            assert_eq!(
                hashes.iter().filter(|other| *other == hash).count(),
                1,
                "{index}"
            );
        }
    }

    #[test]
    fn a_missing_meta_and_a_null_meta_hash_alike() {
        let missing = event(json!({ "id": "a", "text": "hi" }));
        let null = event(json!({ "id": "a", "text": "hi", "meta": null }));
        assert_eq!(missing.content_hash(), null.content_hash());
    }
}
