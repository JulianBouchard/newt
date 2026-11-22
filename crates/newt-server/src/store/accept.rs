use super::{AcceptError, Accepted, INBOX_CHANNEL, InboundEvent, LIVE_CHANNEL, Store, live_topic};

use serde_json::json;
use sqlx::types::Json;

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

impl Store {
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
}
