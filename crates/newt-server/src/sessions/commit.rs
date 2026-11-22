use super::{CommitError, Lease, Owned, PendingEvent, TurnCommit, seconds};
use std::time::Duration;

use newt_core::message::Message;
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};

use crate::store::Store;

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

pub(super) struct EncodedMessages {
    entries: Vec<String>,
    tokens: Vec<i32>,
}

pub(super) fn encode_messages(messages: &[Message]) -> Result<EncodedMessages, sqlx::Error> {
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

pub(super) const COMMIT_TURN: &str = "with bumped as ( \
       update sessions set next_history_seq = next_history_seq + $8, failed_attempts = 0, \
         retry_after = null, lease_expires_at = now() + make_interval(secs => $10), \
         provider_state = coalesce($11, provider_state), \
         cancel_turn = null, cancel_requested_at = null \
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

pub(super) type CommitRow = (bool, i64, i64, i64, i64);

pub(super) async fn commit(
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

pub(super) type PendingRow = (
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
}
