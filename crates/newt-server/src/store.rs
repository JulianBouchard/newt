use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::pool::PoolConnection;
use sqlx::types::Json;
use sqlx::{PgPool, Postgres};

pub const LIVE_CHANNEL: &str = "newt_live";
pub const INBOX_CHANNEL: &str = "newt_inbox";

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

#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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
        live_meta: bool,
    ) -> Result<Accepted, sqlx::Error> {
        let (mut connection, pool_wait_ms) = self.connection().await?;
        let (inbox_id, fresh, _received, _notified): (i64, bool, i64, i64) = sqlx::query_as(ACCEPT)
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
            .bind(live_meta)
            .fetch_one(&mut *connection)
            .await?;
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

const ACCEPT: &str = "with added as ( \
       insert into inbox (agent, key, event_id, kind, policy, priority, payload) \
       values ($1, $2, $3, $4, $5, $6, $7) \
       on conflict (agent, key, event_id) do nothing returning id), \
     session as ( \
       insert into sessions (agent, key, next_live_seq) values ($1, $2, 1) \
       on conflict (agent, key) do update set next_live_seq = sessions.next_live_seq + 1 \
         where exists (select 1 from added) \
       returning next_live_seq), \
     existing as (select id from inbox where agent = $1 and key = $2 and event_id = $3), \
     received as ( \
       insert into live_events (agent, key, seq, event) \
       select $1, $2, session.next_live_seq, \
         jsonb_build_object('type', 'received', 'event_id', $3::text, 'inbox_id', added.id) \
           || case when $11 and jsonb_typeof($7::jsonb -> 'meta') not in ('null') \
                then jsonb_build_object('meta', $7::jsonb -> 'meta') else '{}'::jsonb end \
       from session, added returning 1), \
     notified as (select pg_notify($8, $10), pg_notify($9, $10) from added) \
     select coalesce((select id from added), (select id from existing)), \
       exists (select 1 from added), (select count(*) from received), \
       (select count(*) from notified)";

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
