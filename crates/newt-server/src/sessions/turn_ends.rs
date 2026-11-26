use super::{Lease, Owned, TurnEnd};

use serde_json::Value;
use sqlx::types::Json;

use crate::store::Store;

pub(super) const TURN_END_MAX_BACKOFF_SECONDS: f64 = 300.0;
pub(super) const TURN_END_LEASE_SECONDS: f64 = 30.0;

pub(super) const CLAIM_TURN_ENDS: &str = "with due as ( \
       select id from turn_ends where next_attempt_at <= now() \
       order by id limit $1 for update skip locked) \
     update turn_ends t set attempts = t.attempts + 1, \
       next_attempt_at = now() + make_interval(secs => \
         greatest($3, least($2, power(2, least(t.attempts, 10))))) \
     from due where t.id = due.id \
     returning t.id, t.agent, t.payload, t.attempts";

pub(super) const DEAD_LETTER_TURN_END: &str = "insert into turn_ends (agent, key, turn_id, payload) \
     select $1, $2, $3, jsonb_set($4, '{events}', coalesce((select jsonb_agg( \
         jsonb_build_object('id', i.event_id, 'meta', i.payload->'meta') order by i.id) \
       from inbox i where i.agent = $1 and i.key = $2 and i.id = any($5)), '[]'::jsonb))";

impl Store {
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

    /// Gives up on events no turn could answer: consumes `inbox_ids` as dead
    /// letters, queues the `turn_end` payload when the provider wants one,
    /// drops the session's journal and releases the session, all while
    /// `lease` still holds it. False when another owner holds the session by
    /// now: then nothing changes.
    pub fn dead_letter(
        &self,
        lease: &Lease,
        inbox_ids: Vec<i64>,
        reason: String,
        turn_end: Option<Value>,
    ) -> Owned<bool> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let mut tx = pool.begin().await?;
            let released = sqlx::query(
                "update sessions set failed_attempts = 0, retry_after = null, \
                   cancel_turn = null, cancel_requested_at = null, \
                   lease_owner = null, lease_expires_at = null \
                 where agent = $1 and key = $2 and lease_owner = $3 and lease_epoch = $4",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(&lease.owner)
            .bind(lease.epoch)
            .execute(&mut *tx)
            .await?;
            if released.rows_affected() == 0 {
                tx.rollback().await?;
                return Ok(false);
            }
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
            tx.commit().await?;
            Ok(true)
        })
    }
}
