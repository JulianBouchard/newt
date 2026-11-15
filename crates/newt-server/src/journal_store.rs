use newt_core::journal::JournalEntry;
use sqlx::types::Json;

use crate::sessions::{Lease, Owned};
use crate::store::Store;

#[derive(Debug, Clone, PartialEq)]
pub struct OpenTurn {
    pub turn_id: String,
    pub inbox_ids: Vec<i64>,
    pub entries: Vec<JournalEntry>,
    pub next_seq: i32,
}

const FENCED_BATCH_RELAXED: &str = "with relaxed as (select set_config('synchronous_commit', 'off', true)), \
     written as ( \
       insert into turn_journal (agent, key, turn_id, seq, kind, payload) \
       select $1, $2, $3, $4 + entry.ordinality::int - 1, entry.kind, entry.payload \
       from unnest($5::text[], $6::jsonb[]) with ordinality as entry(kind, payload, ordinality) \
       where exists (select 1 from sessions \
         where agent = $1 and key = $2 and lease_owner = $7 and lease_epoch = $8 for share) \
       returning 1) \
     select 1 from written, relaxed";

const FENCED_BATCH: &str = "insert into turn_journal (agent, key, turn_id, seq, kind, payload) \
     select $1, $2, $3, $4 + entry.ordinality::int - 1, entry.kind, entry.payload \
     from unnest($5::text[], $6::jsonb[]) with ordinality as entry(kind, payload, ordinality) \
     where exists (select 1 from sessions \
       where agent = $1 and key = $2 and lease_owner = $7 and lease_epoch = $8 for share) \
     returning 1";

fn entry_kind(entry: &JournalEntry) -> &'static str {
    match entry {
        JournalEntry::Steered { .. } => "steered",
        JournalEntry::ModelResponse { .. } => "model_response",
        JournalEntry::OutwardIntent { .. } => "outward_intent",
        JournalEntry::ToolOutput { .. } => "tool_output",
    }
}

impl Store {
    pub fn append_journal_batch(
        &self,
        lease: &Lease,
        turn_id: &str,
        first_seq: i32,
        entries: &[JournalEntry],
        durable: bool,
    ) -> Owned<bool> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        let turn_id = turn_id.to_owned();
        let kinds: Vec<&'static str> = entries.iter().map(entry_kind).collect();
        let payloads: Vec<Json<JournalEntry>> = entries.iter().cloned().map(Json).collect();
        let expected = entries.len();
        Box::pin(async move {
            let statement = if durable {
                FENCED_BATCH
            } else {
                FENCED_BATCH_RELAXED
            };
            let written: Vec<i32> = sqlx::query_scalar(statement)
                .bind(&lease.agent)
                .bind(&lease.key)
                .bind(&turn_id)
                .bind(first_seq)
                .bind(kinds)
                .bind(payloads)
                .bind(&lease.owner)
                .bind(lease.epoch)
                .fetch_all(&pool)
                .await?;
            Ok(written.len() == expected)
        })
    }
}

pub(crate) type JournalRow = (String, i32, String, serde_json::Value);

/// The open turn in a session's journal rows (ordered by turn and seq), if
/// there is one.
pub(crate) fn open_turn_from(rows: Vec<JournalRow>) -> Result<Option<OpenTurn>, sqlx::Error> {
    let Some(turn_id) = rows.first().map(|row| row.0.clone()) else {
        return Ok(None);
    };
    let mut open = OpenTurn {
        turn_id: turn_id.clone(),
        inbox_ids: Vec::new(),
        entries: Vec::new(),
        next_seq: 0,
    };
    for (row_turn, seq, kind, payload) in rows {
        if row_turn != turn_id {
            tracing::error!(%row_turn, %turn_id, "two open turns in one session; resuming the oldest");
            continue;
        }
        open.next_seq = open.next_seq.max(seq + 1);
        if kind == "start" {
            open.inbox_ids = serde_json::from_value(payload["inbox_ids"].clone())
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        } else {
            open.entries.push(
                serde_json::from_value(payload)
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
            );
        }
    }
    Ok(Some(open))
}
