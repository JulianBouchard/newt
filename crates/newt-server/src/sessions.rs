use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use newt_core::message::Message;
use sqlx::{PgConnection, PgPool};

use crate::store::{Store, timed_connection};

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
    pub fn claim(
        &self,
        agent: &str,
        key: &str,
        owner: &str,
        ttl: Duration,
    ) -> Owned<(Option<Lease>, u64)> {
        let pool = self.pool().clone();
        let (agent, key, owner) = (agent.to_owned(), key.to_owned(), owner.to_owned());
        Box::pin(async move {
            let (mut connection, waited) = timed_connection(&pool).await?;
            let epoch: Option<i64> = sqlx::query_scalar(
                "update sessions set lease_owner = $3, lease_epoch = lease_epoch + 1, \
                   lease_expires_at = now() + make_interval(secs => $4) \
                 where agent = $1 and key = $2 \
                   and (lease_owner is null or lease_expires_at < now()) \
                   and (retry_after is null or retry_after <= now()) \
                 returning lease_epoch",
            )
            .bind(&agent)
            .bind(&key)
            .bind(&owner)
            .bind(seconds(ttl))
            .fetch_optional(&mut *connection)
            .await?;
            let lease = epoch.map(|epoch| Lease {
                agent,
                key,
                owner,
                epoch,
            });
            Ok((lease, waited))
        })
    }

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
        turn_id: String,
        messages: Vec<Message>,
        consumed: Vec<i64>,
        ttl: Duration,
    ) -> Owned<(), CommitError> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move { commit(&pool, &lease, &turn_id, &messages, &consumed, ttl).await })
    }

    pub fn release_if_idle(&self, lease: &Lease) -> Owned<bool> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let released: Option<i32> = sqlx::query_scalar(
                "update sessions s set lease_owner = null, lease_expires_at = null \
                 where agent = $1 and key = $2 and lease_owner = $3 and lease_epoch = $4 \
                   and not exists (select 1 from inbox i \
                     where i.agent = s.agent and i.key = s.key and i.consumed_at is null) \
                 returning 1",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(&lease.owner)
            .bind(lease.epoch)
            .fetch_optional(&pool)
            .await?;
            Ok(released.is_some())
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

    pub fn dead_letter(&self, lease: &Lease, inbox_ids: Vec<i64>, reason: String) -> Owned<()> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let mut tx = pool.begin().await?;
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

pub(crate) async fn insert_history(
    tx: &mut sqlx::PgConnection,
    lease: &Lease,
    turn_id: &str,
    kind: &str,
    mut seq: i64,
    messages: &[Message],
) -> Result<i64, sqlx::Error> {
    for message in messages {
        let encoded =
            serde_json::to_string(message).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        let tokens = i32::try_from(encoded.len() / 4).unwrap_or(i32::MAX);
        sqlx::query(
            "insert into history (agent, key, seq, turn_id, kind, entry, tokens) \
             values ($1, $2, $3, $4, $5, $6::jsonb, $7)",
        )
        .bind(&lease.agent)
        .bind(&lease.key)
        .bind(seq)
        .bind(turn_id)
        .bind(kind)
        .bind(encoded)
        .bind(tokens)
        .execute(&mut *tx)
        .await?;
        seq += 1;
    }
    Ok(seq)
}

async fn commit(
    pool: &PgPool,
    lease: &Lease,
    turn_id: &str,
    messages: &[Message],
    consumed: &[i64],
    ttl: Duration,
) -> Result<(), CommitError> {
    let mut tx = pool.begin().await?;
    let next_seq = fence(&mut tx, lease).await?;
    let seq = insert_history(&mut tx, lease, turn_id, "message", next_seq, messages).await?;
    sqlx::query(
        "update inbox set consumed_at = now() where agent = $1 and key = $2 and id = any($3)",
    )
    .bind(&lease.agent)
    .bind(&lease.key)
    .bind(consumed)
    .execute(&mut *tx)
    .await?;
    sqlx::query("delete from turn_journal where agent = $1 and key = $2 and turn_id = $3")
        .bind(&lease.agent)
        .bind(&lease.key)
        .bind(turn_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "update sessions set next_history_seq = $3, failed_attempts = 0, retry_after = null, \
           lease_expires_at = now() + make_interval(secs => $4) \
         where agent = $1 and key = $2",
    )
    .bind(&lease.agent)
    .bind(&lease.key)
    .bind(seq)
    .bind(seconds(ttl))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn pending_events_on(
    connection: &mut PgConnection,
    lease: &Lease,
) -> Result<Vec<PendingEvent>, sqlx::Error> {
    let rows: Vec<(i64, String, i16, String, Option<String>)> = sqlx::query_as(
        "select id, event_id, priority, policy, payload->>'text' from inbox \
         where agent = $1 and key = $2 and consumed_at is null order by id",
    )
    .bind(&lease.agent)
    .bind(&lease.key)
    .fetch_all(&mut *connection)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(inbox_id, event_id, priority, policy, text)| PendingEvent {
                inbox_id,
                event_id,
                interactive: priority == 0,
                steer: policy == "steer",
                text: text.unwrap_or_default(),
            },
        )
        .collect())
}
