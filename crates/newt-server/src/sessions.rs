use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use newt_core::message::Message;
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};

use crate::store::Store;

const MAX_BACKOFF_SECONDS: f64 = 300.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub agent: String,
    pub key: String,
    pub owner: String,
    pub epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEvent {
    pub inbox_id: i64,
    pub event_id: String,
    pub interactive: bool,
    pub steer: bool,
    pub text: String,
    pub meta: Option<Value>,
}

/// What a turn's commit writes besides its history: the events it consumed,
/// the provider's new session state, and the turn-end callback to deliver.
#[derive(Debug, Clone, Default)]
pub struct TurnCommit {
    pub turn_id: String,
    pub messages: Vec<Message>,
    pub consumed: Vec<i64>,
    pub provider_state: Option<Value>,
    pub turn_end: Option<Value>,
    /// The turn ran on a reset history: when the commit adds history rows, it
    /// marks everything before the first of them as no longer part of the
    /// session's conversation. A commit with no rows resets nothing.
    pub reset: bool,
}

/// A turn-end callback claimed for delivery. `attempts` counts this one.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnd {
    pub id: i64,
    pub agent: String,
    pub payload: Value,
    pub attempts: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("the lease moved to another owner or epoch; nothing was committed")]
    Fenced,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub type Owned<T, E = sqlx::Error> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'static>>;

fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

impl Store {
    pub fn pending_events(&self, lease: &Lease) -> Owned<Vec<PendingEvent>> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let mut connection = pool.acquire().await?;
            pending_events_on(&mut connection, &lease).await
        })
    }

    pub fn commit_turn(
        &self,
        lease: &Lease,
        turn: TurnCommit,
        ttl: Duration,
    ) -> Owned<(), CommitError> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move { commit(&pool, &lease, &turn, ttl).await })
    }

    /// Takes up to `limit` turn-end callbacks that are due and pushes their
    /// next attempt out, so another runner does not deliver them at the
    /// same time.
    pub fn claim_turn_ends(&self, limit: i64) -> Owned<Vec<TurnEnd>> {
        let pool = self.pool().clone();
        Box::pin(async move {
            let rows: Vec<(i64, String, Json<Value>, i32)> = sqlx::query_as(CLAIM_TURN_ENDS)
                .bind(limit)
                .bind(TURN_END_MAX_BACKOFF_SECONDS)
                .bind(TURN_END_LEASE_SECONDS)
                .fetch_all(&pool)
                .await?;
            Ok(rows
                .into_iter()
                .map(|(id, agent, Json(payload), attempts)| TurnEnd {
                    id,
                    agent,
                    payload,
                    attempts,
                })
                .collect())
        })
    }

    pub fn delete_turn_end(&self, id: i64) -> Owned<()> {
        let pool = self.pool().clone();
        Box::pin(async move {
            sqlx::query("delete from turn_ends where id = $1")
                .bind(id)
                .execute(&pool)
                .await?;
            Ok(())
        })
    }

    pub fn release(&self, lease: &Lease) -> Owned<()> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            sqlx::query(
                "update sessions set lease_owner = null, lease_expires_at = null \
                 where agent = $1 and key = $2 and lease_owner = $3 and lease_epoch = $4",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(&lease.owner)
            .bind(lease.epoch)
            .execute(&pool)
            .await?;
            Ok(())
        })
    }

    pub fn release_with_backoff(&self, lease: &Lease, base_seconds: f64) -> Owned<i32> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let attempts: Option<i32> = sqlx::query_scalar(
                "update sessions set lease_owner = null, lease_expires_at = null, \
                   failed_attempts = failed_attempts + 1, \
                   retry_after = now() + make_interval(secs => \
                     least($5, $6 * power(2, failed_attempts))) \
                 where agent = $1 and key = $2 and lease_owner = $3 and lease_epoch = $4 \
                 returning failed_attempts",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(&lease.owner)
            .bind(lease.epoch)
            .bind(MAX_BACKOFF_SECONDS)
            .bind(base_seconds)
            .fetch_optional(&pool)
            .await?;
            Ok(attempts.unwrap_or(0))
        })
    }

    /// Gives up on events no turn could answer. A `turn_end` payload, when
    /// the provider wants one, is queued with them.
    pub fn dead_letter(
        &self,
        lease: &Lease,
        inbox_ids: Vec<i64>,
        reason: String,
        turn_end: Option<Value>,
    ) -> Owned<()> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let mut tx = pool.begin().await?;
            if let Some(payload) = &turn_end {
                let turn = payload["turn"].as_str().unwrap_or_default().to_owned();
                sqlx::query(DEAD_LETTER_TURN_END)
                    .bind(&lease.agent)
                    .bind(&lease.key)
                    .bind(turn)
                    .bind(Json(payload))
                    .bind(&inbox_ids)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "update inbox set consumed_at = now(), dead_letter = $4 \
                 where agent = $1 and key = $2 and id = any($3)",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(&inbox_ids)
            .bind(&reason)
            .execute(&mut *tx)
            .await?;
            sqlx::query("delete from turn_journal where agent = $1 and key = $2")
                .bind(&lease.agent)
                .bind(&lease.key)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "update sessions set failed_attempts = 0, retry_after = null \
                 where agent = $1 and key = $2",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    pub fn heartbeat(
        &self,
        owner: &str,
        ttl: Duration,
        running: Vec<(String, String)>,
    ) -> Owned<u64> {
        let pool = self.pool().clone();
        let owner = owner.to_owned();
        let (agents, keys): (Vec<String>, Vec<String>) = running.into_iter().unzip();
        Box::pin(async move {
            let result = sqlx::query(
                "update sessions set lease_expires_at = now() + make_interval(secs => $2) \
                 where lease_owner = $1 \
                   and (agent, key) in (select * from unnest($3::text[], $4::text[]))",
            )
            .bind(&owner)
            .bind(seconds(ttl))
            .bind(&agents)
            .bind(&keys)
            .execute(&pool)
            .await?;
            Ok(result.rows_affected())
        })
    }

    pub fn sessions_needing_work(&self, limit: i64) -> Owned<Vec<(String, String)>> {
        let pool = self.pool().clone();
        Box::pin(async move {
            sqlx::query_as(
                "select s.agent, s.key from sessions s \
                 where (s.lease_owner is null or s.lease_expires_at < now()) \
                   and (s.retry_after is null or s.retry_after <= now()) \
                   and exists (select 1 from inbox i \
                     where i.agent = s.agent and i.key = s.key and i.consumed_at is null) \
                 order by (select min(i.id) from inbox i \
                   where i.agent = s.agent and i.key = s.key and i.consumed_at is null) \
                 limit $1",
            )
            .bind(limit)
            .fetch_all(&pool)
            .await
        })
    }
}

pub(crate) async fn fence(tx: &mut sqlx::PgConnection, lease: &Lease) -> Result<i64, CommitError> {
    let row: Option<(Option<String>, i64, i64)> = sqlx::query_as(
        "select lease_owner, lease_epoch, next_history_seq from sessions \
         where agent = $1 and key = $2 for update",
    )
    .bind(&lease.agent)
    .bind(&lease.key)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((owner, epoch, next_seq)) = row else {
        return Err(CommitError::Fenced);
    };
    if owner.as_deref() != Some(lease.owner.as_str()) || epoch != lease.epoch {
        return Err(CommitError::Fenced);
    }
    Ok(next_seq)
}

struct EncodedMessages {
    entries: Vec<String>,
    tokens: Vec<i32>,
}

fn encode_messages(messages: &[Message]) -> Result<EncodedMessages, sqlx::Error> {
    let mut encoded = EncodedMessages {
        entries: Vec::with_capacity(messages.len()),
        tokens: Vec::with_capacity(messages.len()),
    };
    for message in messages {
        let entry =
            serde_json::to_string(message).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        encoded
            .tokens
            .push(i32::try_from(entry.len() / 4).unwrap_or(i32::MAX));
        encoded.entries.push(entry);
    }
    Ok(encoded)
}

pub(crate) async fn insert_history(
    tx: &mut sqlx::PgConnection,
    lease: &Lease,
    turn_id: &str,
    kind: &str,
    seq: i64,
    messages: &[Message],
) -> Result<i64, sqlx::Error> {
    let encoded = encode_messages(messages)?;
    sqlx::query(
        "insert into history (agent, key, seq, turn_id, kind, entry, tokens) \
         select $1, $2, $3 + message.ordinality - 1, $4, $5, message.entry::jsonb, message.tokens \
         from unnest($6::text[], $7::int4[]) with ordinality as message(entry, tokens, ordinality)",
    )
    .bind(&lease.agent)
    .bind(&lease.key)
    .bind(seq)
    .bind(turn_id)
    .bind(kind)
    .bind(&encoded.entries)
    .bind(&encoded.tokens)
    .execute(&mut *tx)
    .await?;
    Ok(seq + i64::try_from(messages.len()).unwrap_or(i64::MAX))
}

const COMMIT_TURN: &str = "with bumped as ( \
       update sessions set next_history_seq = next_history_seq + $8, failed_attempts = 0, \
         retry_after = null, lease_expires_at = now() + make_interval(secs => $10), \
         provider_state = coalesce($11, provider_state) \
       where agent = $1 and key = $2 and lease_owner = $3 and lease_epoch = $4 \
       returning next_history_seq - $8 as first_seq), \
     added as ( \
       insert into history (agent, key, seq, turn_id, kind, entry, tokens) \
       select $1, $2, bumped.first_seq + message.ordinality - 1, $5, 'message', \
         message.entry::jsonb, message.tokens \
       from bumped, unnest($6::text[], $7::int4[]) with ordinality as message(entry, tokens, ordinality) \
       returning 1), \
     marked as ( \
       insert into checkpoints (agent, key, through_seq, snapshot, summary) \
       select $1, $2, bumped.first_seq - 1, '{}'::jsonb, '' from bumped where $13 and $8 > 0 \
       on conflict (agent, key, through_seq) do update set snapshot = '{}'::jsonb, summary = '' \
       returning 1), \
     consumed as ( \
       update inbox set consumed_at = now() \
       where agent = $1 and key = $2 and id = any($9) and exists (select 1 from bumped) \
       returning 1), \
     cleared as ( \
       delete from turn_journal \
       where agent = $1 and key = $2 and turn_id = $5 and exists (select 1 from bumped) \
       returning 1), \
     ended as ( \
       insert into turn_ends (agent, key, turn_id, payload) \
       select $1, $2, $5, jsonb_set($12, '{events}', coalesce((select jsonb_agg( \
           jsonb_build_object('id', i.event_id, 'meta', i.payload->'meta') order by i.id) \
         from inbox i where i.agent = $1 and i.key = $2 and i.id = any($9)), '[]'::jsonb)) \
       from bumped where $12 is not null \
       returning 1) \
     select exists (select 1 from bumped), (select count(*) from added), \
       (select count(*) from consumed), (select count(*) from cleared), \
       (select count(*) from ended)";

const TURN_END_MAX_BACKOFF_SECONDS: f64 = 300.0;
const TURN_END_LEASE_SECONDS: f64 = 30.0;

const CLAIM_TURN_ENDS: &str = "with due as ( \
       select id from turn_ends where next_attempt_at <= now() \
       order by id limit $1 for update skip locked) \
     update turn_ends t set attempts = t.attempts + 1, \
       next_attempt_at = now() + make_interval(secs => \
         greatest($3, least($2, power(2, least(t.attempts, 10))))) \
     from due where t.id = due.id \
     returning t.id, t.agent, t.payload, t.attempts";

const DEAD_LETTER_TURN_END: &str = "insert into turn_ends (agent, key, turn_id, payload) \
     select $1, $2, $3, jsonb_set($4, '{events}', coalesce((select jsonb_agg( \
         jsonb_build_object('id', i.event_id, 'meta', i.payload->'meta') order by i.id) \
       from inbox i where i.agent = $1 and i.key = $2 and i.id = any($5)), '[]'::jsonb))";

type CommitRow = (bool, i64, i64, i64, i64);

async fn commit(
    pool: &PgPool,
    lease: &Lease,
    turn: &TurnCommit,
    ttl: Duration,
) -> Result<(), CommitError> {
    let encoded = encode_messages(&turn.messages)?;
    let count = i64::try_from(turn.messages.len()).unwrap_or(i64::MAX);
    let (committed, _added, _consumed, _cleared, _ended): CommitRow = sqlx::query_as(COMMIT_TURN)
        .bind(&lease.agent)
        .bind(&lease.key)
        .bind(&lease.owner)
        .bind(lease.epoch)
        .bind(&turn.turn_id)
        .bind(&encoded.entries)
        .bind(&encoded.tokens)
        .bind(count)
        .bind(&turn.consumed)
        .bind(seconds(ttl))
        .bind(turn.provider_state.as_ref().map(Json))
        .bind(turn.turn_end.as_ref().map(Json))
        .bind(turn.reset)
        .fetch_one(pool)
        .await?;
    if !committed {
        return Err(CommitError::Fenced);
    }
    Ok(())
}

type PendingRow = (
    i64,
    String,
    i16,
    String,
    Option<String>,
    Option<Json<Value>>,
);

pub(crate) async fn pending_events_on(
    connection: &mut PgConnection,
    lease: &Lease,
) -> Result<Vec<PendingEvent>, sqlx::Error> {
    let rows: Vec<PendingRow> = sqlx::query_as(
        "select id, event_id, priority, policy, payload->>'text', payload->'meta' from inbox \
         where agent = $1 and key = $2 and consumed_at is null order by id",
    )
    .bind(&lease.agent)
    .bind(&lease.key)
    .fetch_all(&mut *connection)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(inbox_id, event_id, priority, policy, text, meta)| PendingEvent {
                inbox_id,
                event_id,
                interactive: priority == 0,
                steer: policy == "steer",
                text: text.unwrap_or_default(),
                meta: meta.map(|Json(meta)| meta).filter(|meta| !meta.is_null()),
            },
        )
        .collect())
}
