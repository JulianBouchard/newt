use super::{Lease, Owned, seconds};
use std::time::Duration;

use newt_core::prompt::Lane;

use crate::store::Store;

pub(super) const MAX_BACKOFF_SECONDS: f64 = 300.0;

impl Store {
    /// The lane a woken session's turn will run in: interactive when any of
    /// its unconsumed events is, or when it has none.
    pub fn pending_lane(&self, agent: &str, key: &str) -> Owned<Lane> {
        let pool = self.pool().clone();
        let (agent, key) = (agent.to_owned(), key.to_owned());
        Box::pin(async move {
            let interactive: Option<bool> = sqlx::query_scalar(
                "select bool_or(priority = 0) from inbox \
                 where agent = $1 and key = $2 and consumed_at is null",
            )
            .bind(&agent)
            .bind(&key)
            .fetch_one(&pool)
            .await?;
            Ok(if interactive == Some(false) {
                Lane::Background
            } else {
                Lane::Interactive
            })
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

    /// The session's failed attempts so far, while `lease` holds it; 0 when
    /// it no longer does.
    pub fn failed_attempts(&self, lease: &Lease) -> Owned<i32> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let attempts: Option<i32> = sqlx::query_scalar(
                "select failed_attempts from sessions \
                 where agent = $1 and key = $2 and lease_owner = $3 and lease_epoch = $4",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(&lease.owner)
            .bind(lease.epoch)
            .fetch_optional(&pool)
            .await?;
            Ok(attempts.unwrap_or(0))
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

    /// Renews the leases of the running sessions and returns the ones with a
    /// cancel requested: their agent, key and the turn it names.
    pub fn heartbeat(
        &self,
        owner: &str,
        ttl: Duration,
        running: Vec<(String, String)>,
    ) -> Owned<Vec<(String, String, String)>> {
        let pool = self.pool().clone();
        let owner = owner.to_owned();
        let (agents, keys): (Vec<String>, Vec<String>) = running.into_iter().unzip();
        Box::pin(async move {
            let renewed: Vec<(String, String, Option<String>)> = sqlx::query_as(
                "update sessions set lease_expires_at = now() + make_interval(secs => $2) \
                 where lease_owner = $1 \
                   and (agent, key) in (select * from unnest($3::text[], $4::text[])) \
                 returning agent, key, cancel_turn",
            )
            .bind(&owner)
            .bind(seconds(ttl))
            .bind(&agents)
            .bind(&keys)
            .fetch_all(&pool)
            .await?;
            Ok(renewed
                .into_iter()
                .filter_map(|(agent, key, turn)| turn.map(|turn| (agent, key, turn)))
                .collect())
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
