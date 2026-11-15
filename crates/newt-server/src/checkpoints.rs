use newt_core::checkpoint::Checkpoint;
use newt_core::message::Message;
use sqlx::PgConnection;
use sqlx::types::Json;

use crate::sessions::{CommitError, Lease, Owned, fence, insert_history};
use crate::store::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRow {
    pub seq: i64,
    pub turn_id: String,
    pub tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionState {
    pub rows: Vec<LiveRow>,
    pub checkpoint_tokens: i64,
    pub open_commitments: Vec<String>,
    pub due: bool,
}

impl CompactionState {
    pub fn tokens(&self) -> i64 {
        self.checkpoint_tokens + self.rows.iter().map(|row| row.tokens).sum::<i64>()
    }
}

type CompactionRow = (
    Option<String>,
    Option<Json<Checkpoint>>,
    Json<Vec<(i64, String, i32)>>,
    Option<bool>,
);

const COMPACTION_STATE: &str = "with latest as ( \
       select through_seq, summary, snapshot from checkpoints \
       where agent = $1 and key = $2 order by through_seq desc limit 1) \
     select (select summary from latest), (select snapshot from latest), \
       coalesce((select jsonb_agg(jsonb_build_array(h.seq, h.turn_id, h.tokens) order by h.seq) \
         from history h where h.agent = $1 and h.key = $2 and h.kind = 'message' \
           and h.seq > coalesce((select through_seq from latest), -1)), '[]'::jsonb), \
       (select compaction_retry_after is null or compaction_retry_after <= now() \
         from sessions where agent = $1 and key = $2)";

impl Store {
    pub fn history(&self, lease: &Lease) -> Owned<Vec<Message>> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let mut connection = pool.acquire().await?;
            history_on(&mut connection, &lease).await
        })
    }

    pub fn compaction_state(&self, lease: &Lease) -> Owned<CompactionState> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let (summary, snapshot, Json(rows), due): CompactionRow =
                sqlx::query_as(COMPACTION_STATE)
                    .bind(&lease.agent)
                    .bind(&lease.key)
                    .fetch_one(&pool)
                    .await?;
            let due = due.ok_or(sqlx::Error::RowNotFound)?;
            Ok(CompactionState {
                due,
                rows: rows
                    .into_iter()
                    .map(|(seq, turn_id, tokens)| LiveRow {
                        seq,
                        turn_id,
                        tokens: i64::from(tokens),
                    })
                    .collect(),
                checkpoint_tokens: summary.as_ref().map_or(0, |summary| {
                    i64::try_from(summary.len() / 4).unwrap_or(i64::MAX)
                }),
                open_commitments: snapshot
                    .map(|Json(checkpoint)| checkpoint.commitments)
                    .unwrap_or_default(),
            })
        })
    }

    pub fn commit_compaction(
        &self,
        lease: &Lease,
        turn_id: String,
        flushed: Vec<Message>,
        through_seq: i64,
        checkpoint: Checkpoint,
    ) -> Owned<(), CommitError> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            let mut tx = pool.begin().await?;
            let next_seq = fence(&mut tx, &lease).await?;
            let seq =
                insert_history(&mut tx, &lease, &turn_id, "message", next_seq, &flushed).await?;
            sqlx::query(
                "insert into checkpoints (agent, key, through_seq, snapshot, summary) \
                 values ($1, $2, $3, $4, $5)",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(through_seq)
            .bind(Json(&checkpoint))
            .bind(checkpoint.render())
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "update sessions set next_history_seq = $3, compaction_failures = 0, \
                   compaction_retry_after = null where agent = $1 and key = $2",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }
}

impl Store {
    pub fn record_compaction_failure(&self, lease: &Lease, base_seconds: f64) -> Owned<i32> {
        let pool = self.pool().clone();
        let lease = lease.clone();
        Box::pin(async move {
            sqlx::query_scalar(
                "update sessions set compaction_failures = compaction_failures + 1, \
                   compaction_retry_after = now() + make_interval(secs => \
                     least($3 * power(2, compaction_failures), 86400)) \
                 where agent = $1 and key = $2 returning compaction_failures",
            )
            .bind(&lease.agent)
            .bind(&lease.key)
            .bind(base_seconds)
            .fetch_one(&pool)
            .await
        })
    }
}

pub fn tail_cut(rows: &[LiveRow], tail_budget: i64) -> Option<i64> {
    let mut tail_tokens = 0;
    let mut tail_start = rows.len();
    let mut index = rows.len();
    while index > 0 {
        let turn = &rows[index - 1].turn_id;
        let mut start = index;
        let mut turn_tokens = 0;
        while start > 0 && rows[start - 1].turn_id == *turn {
            start -= 1;
            turn_tokens += rows[start].tokens;
        }
        if tail_tokens + turn_tokens > tail_budget {
            break;
        }
        tail_tokens += turn_tokens;
        tail_start = start;
        index = start;
    }
    tail_start
        .checked_sub(1)
        .and_then(|last_compacted| rows.get(last_compacted))
        .map(|row| row.seq)
}

const HISTORY: &str = "with latest as ( \
       select through_seq, summary from checkpoints \
       where agent = $1 and key = $2 order by through_seq desc limit 1) \
     select (select summary from latest), \
       coalesce((select jsonb_agg(h.entry order by h.seq) from history h \
         where h.agent = $1 and h.key = $2 and h.kind = 'message' \
           and h.seq > coalesce((select through_seq from latest), -1)), '[]'::jsonb)";

pub(crate) async fn history_on(
    connection: &mut PgConnection,
    lease: &Lease,
) -> Result<Vec<Message>, sqlx::Error> {
    let (summary, Json(messages)): (Option<String>, Json<Vec<Message>>) = sqlx::query_as(HISTORY)
        .bind(&lease.agent)
        .bind(&lease.key)
        .fetch_one(&mut *connection)
        .await?;
    let mut history = Vec::with_capacity(messages.len() + 1);
    if let Some(summary) = summary {
        history.push(Message::user_text(summary));
    }
    history.extend(messages);
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(turns: &[(&str, i64)]) -> Vec<LiveRow> {
        turns
            .iter()
            .enumerate()
            .map(|(seq, (turn_id, tokens))| LiveRow {
                seq: i64::try_from(seq).unwrap_or_default() + 10,
                turn_id: (*turn_id).to_owned(),
                tokens: *tokens,
            })
            .collect()
    }

    #[test]
    fn the_tail_keeps_whole_recent_turns_within_its_budget() {
        let history = rows(&[("a", 50), ("a", 50), ("b", 30), ("b", 30), ("c", 40)]);
        assert_eq!(tail_cut(&history, 100), Some(11), "turns b and c stay");
        assert_eq!(tail_cut(&history, 50), Some(13), "only turn c fits");
        assert_eq!(
            tail_cut(&history, 10),
            Some(14),
            "nothing fits, all is compacted"
        );
        assert_eq!(
            tail_cut(&history, 1000),
            None,
            "everything fits, nothing to compact"
        );
    }
}
