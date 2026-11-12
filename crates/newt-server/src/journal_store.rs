use newt_core::journal::JournalEntry;
use serde_json::json;
use sqlx::types::Json;

use crate::sessions::{Lease, Owned, PendingEvent};
use crate::store::Store;

#[derive(Debug, Clone, PartialEq)]
pub struct OpenTurn {
    pub turn_id: String,
    pub inbox_ids: Vec<i64>,
    pub entries: Vec<JournalEntry>,
    pub next_seq: i32,
}

const FENCED_INSERT: &str = "insert into turn_journal (agent, key, turn_id, seq, kind, payload) \
     select $1, $2, $3, $4, $5, $6 where exists (select 1 from sessions \
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
    pub fn start_turn(&self, lease: &Lease, turn_id: &str, inbox_ids: &[i64]) -> Owned<bool> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        let turn_id = turn_id.to_owned();
        let header = json!({ "inbox_ids": inbox_ids });
        Box::pin(async move {
            let written: Option<i32> = sqlx::query_scalar(FENCED_INSERT)
                .bind(&lease.agent)
                .bind(&lease.key)
                .bind(&turn_id)
                .bind(0_i32)
                .bind("start")
                .bind(Json(header))
                .bind(&lease.owner)
                .bind(lease.epoch)
                .fetch_optional(&pool)
                .await?;
            Ok(written.is_some())
        })
    }

    pub fn append_journal(
        &self,
        lease: &Lease,
        turn_id: &str,
        seq: i32,
        entry: JournalEntry,
    ) -> Owned<bool> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        let turn_id = turn_id.to_owned();
        Box::pin(async move {
            let written: Option<i32> = sqlx::query_scalar(FENCED_INSERT)
                .bind(&lease.agent)
                .bind(&lease.key)
                .bind(&turn_id)
                .bind(seq)
                .bind(entry_kind(&entry))
                .bind(Json(&entry))
                .bind(&lease.owner)
                .bind(lease.epoch)
                .fetch_optional(&pool)
                .await?;
            Ok(written.is_some())
        })
    }

    pub fn open_turn(&self, lease: &Lease) -> Owned<Option<OpenTurn>> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let rows: Vec<(String, i32, String, Json<serde_json::Value>)> = sqlx::query_as(
                "select turn_id, seq, kind, payload from turn_journal \
                 where agent = $1 and key = $2 order by turn_id, seq",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .fetch_all(&pool)
            .await?;
            let Some(turn_id) = rows.first().map(|row| row.0.clone()) else {
                return Ok(None);
            };
            let mut open = OpenTurn {
                turn_id: turn_id.clone(),
                inbox_ids: Vec::new(),
                entries: Vec::new(),
                next_seq: 0,
            };
            for (row_turn, seq, kind, Json(payload)) in rows {
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
        })
    }

    pub fn events_by_ids(&self, lease: &Lease, ids: &[i64]) -> Owned<Vec<PendingEvent>> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        let ids = ids.to_vec();
        Box::pin(async move {
            let rows: Vec<(i64, String, i16, String, Option<String>)> = sqlx::query_as(
                "select id, event_id, priority, policy, payload->>'text' from inbox \
                 where agent = $1 and key = $2 and id = any($3) order by id",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(&ids)
            .fetch_all(&pool)
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
        })
    }
}
