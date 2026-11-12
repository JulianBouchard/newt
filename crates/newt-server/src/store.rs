use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};

pub const LIVE_CHANNEL: &str = "newt_live";

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

    pub async fn accept_event(
        &self,
        agent: &str,
        key: &str,
        event: &InboundEvent,
    ) -> Result<Accepted, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("insert into sessions (agent, key) values ($1, $2) on conflict do nothing")
            .bind(agent)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        let inserted: Option<i64> = sqlx::query_scalar(
            "insert into inbox (agent, key, event_id, kind, policy, priority, payload) \
             values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict (agent, key, event_id) do nothing returning id",
        )
        .bind(agent)
        .bind(key)
        .bind(&event.id)
        .bind(event.kind.as_str())
        .bind(event.policy.as_str())
        .bind(event.priority.rank())
        .bind(Json(json!({ "text": event.text, "meta": event.meta })))
        .fetch_optional(&mut *tx)
        .await?;
        let accepted = if let Some(inbox_id) = inserted {
            let received =
                json!({ "type": "received", "event_id": event.id, "inbox_id": inbox_id });
            append_live_in(&mut tx, agent, key, &received).await?;
            Accepted {
                inbox_id,
                duplicate: false,
            }
        } else {
            let inbox_id: i64 = sqlx::query_scalar(
                "select id from inbox where agent = $1 and key = $2 and event_id = $3",
            )
            .bind(agent)
            .bind(key)
            .bind(&event.id)
            .fetch_one(&mut *tx)
            .await?;
            Accepted {
                inbox_id,
                duplicate: true,
            }
        };
        tx.commit().await?;
        Ok(accepted)
    }

    pub async fn append_live(
        &self,
        agent: &str,
        key: &str,
        event: &Value,
    ) -> Result<i64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let seq = append_live_in(&mut tx, agent, key, event).await?;
        tx.commit().await?;
        Ok(seq)
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

async fn append_live_in(
    connection: &mut PgConnection,
    agent: &str,
    key: &str,
    event: &Value,
) -> Result<i64, sqlx::Error> {
    let seq: i64 = sqlx::query_scalar(
        "update sessions set next_live_seq = next_live_seq + 1 \
         where agent = $1 and key = $2 returning next_live_seq",
    )
    .bind(agent)
    .bind(key)
    .fetch_one(&mut *connection)
    .await?;
    sqlx::query("insert into live_events (agent, key, seq, event) values ($1, $2, $3, $4)")
        .bind(agent)
        .bind(key)
        .bind(seq)
        .bind(Json(event))
        .execute(&mut *connection)
        .await?;
    sqlx::query("select pg_notify($1, $2)")
        .bind(LIVE_CHANNEL)
        .bind(live_topic(agent, key))
        .execute(&mut *connection)
        .await?;
    Ok(seq)
}

pub fn live_topic(agent: &str, key: &str) -> String {
    format!("{agent}/{key}")
}
