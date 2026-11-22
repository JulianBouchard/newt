use super::{LIVE_CHANNEL, Store, live_topic};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::types::Json;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveEvent {
    pub seq: i64,
    pub event: Value,
}

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

impl Store {
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
}
