//! What newt deletes on its own, and deleting a session on request.
//!
//! Consumed inbox rows and live feed rows are operational: once a turn has committed, the
//! history holds what the agent remembers. The runner prunes both on a schedule. History
//! and checkpoints are the agent's memory and stay until the app deletes the session.

use std::time::Duration;

use crate::store::Store;

const BATCH: i64 = 5_000;

/// How long consumed events and live feed rows are kept. `None` keeps them forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub inbox: Option<Duration>,
    pub live: Option<Duration>,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            inbox: Some(Duration::from_hours(30 * 24)),
            live: Some(Duration::from_hours(7 * 24)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruned {
    pub inbox: u64,
    pub live: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    Deleted,
    NotFound,
    Running,
}

const PRUNE_INBOX: &str = "delete from inbox where id in ( \
       select id from inbox \
       where consumed_at < now() - make_interval(secs => $1) limit $2)";

const PRUNE_LIVE: &str = "delete from live_events where ctid in ( \
       select ctid from live_events \
       where created_at < now() - make_interval(secs => $1) limit $2)";

impl Store {
    /// Deletes consumed events and live feed rows older than `retention`, in batches so no
    /// statement holds locks for long.
    pub async fn prune(&self, retention: Retention) -> Result<Pruned, sqlx::Error> {
        Ok(Pruned {
            inbox: self.prune_older(PRUNE_INBOX, retention.inbox).await?,
            live: self.prune_older(PRUNE_LIVE, retention.live).await?,
        })
    }

    async fn prune_older(
        &self,
        sql: &'static str,
        age: Option<Duration>,
    ) -> Result<u64, sqlx::Error> {
        let Some(age) = age else {
            return Ok(0);
        };
        let mut total = 0;
        loop {
            let deleted = sqlx::query(sql)
                .bind(age.as_secs_f64())
                .bind(BATCH)
                .execute(self.pool())
                .await?
                .rows_affected();
            total += deleted;
            if deleted < u64::try_from(BATCH).unwrap_or(u64::MAX) {
                return Ok(total);
            }
        }
    }

    /// Deletes a session and everything stored for it, unless a runner holds its lease.
    pub async fn delete_session(&self, agent: &str, key: &str) -> Result<Deleted, sqlx::Error> {
        let deleted: Option<i32> = sqlx::query_scalar(
            "delete from sessions \
             where agent = $1 and key = $2 \
               and (lease_owner is null or lease_expires_at < now()) \
             returning 1",
        )
        .bind(agent)
        .bind(key)
        .fetch_optional(self.pool())
        .await?;
        if deleted.is_some() {
            return Ok(Deleted::Deleted);
        }
        let exists: Option<i32> =
            sqlx::query_scalar("select 1 from sessions where agent = $1 and key = $2")
                .bind(agent)
                .bind(key)
                .fetch_optional(self.pool())
                .await?;
        Ok(if exists.is_some() {
            Deleted::Running
        } else {
            Deleted::NotFound
        })
    }
}
