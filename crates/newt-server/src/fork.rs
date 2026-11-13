//! Reading a session's history, and forking a session: a new session that
//! starts with a copy of another's history up to a turn.

use serde::Serialize;
use sqlx::PgPool;
use sqlx::types::Json;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryEntry {
    pub seq: i64,
    pub turn: String,
    pub kind: String,
    pub entry: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("no such session")]
    NoSession,
    #[error("the session has no turn with that id")]
    NoTurn,
    #[error("the target session already exists")]
    Exists,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Where a fork comes from and where it goes.
#[derive(Debug, Clone, Copy)]
pub struct Fork<'a> {
    pub agent: &'a str,
    pub key: &'a str,
    pub into_agent: &'a str,
    pub into_key: &'a str,
    /// Copy history through this turn; `None` copies all of it.
    pub through_turn: Option<&'a str>,
}

/// The session's history in order, or `None` when there is no such session.
pub async fn history(
    pool: &PgPool,
    agent: &str,
    key: &str,
) -> Result<Option<Vec<HistoryEntry>>, sqlx::Error> {
    let exists: Option<(i64,)> =
        sqlx::query_as("select next_history_seq from sessions where agent = $1 and key = $2")
            .bind(agent)
            .bind(key)
            .fetch_optional(pool)
            .await?;
    if exists.is_none() {
        return Ok(None);
    }
    let rows: Vec<(i64, String, String, Json<serde_json::Value>)> = sqlx::query_as(
        "select seq, turn_id, kind, entry from history where agent = $1 and key = $2 order by seq",
    )
    .bind(agent)
    .bind(key)
    .fetch_all(pool)
    .await?;
    Ok(Some(
        rows.into_iter()
            .map(|(seq, turn, kind, Json(entry))| HistoryEntry {
                seq,
                turn,
                kind,
                entry,
            })
            .collect(),
    ))
}

/// Creates the target session with the source's history and checkpoints up
/// to the cut, and returns how many history entries it copied. Nothing else
/// is copied: no pending events, journal or live feed.
pub async fn fork(pool: &PgPool, fork: Fork<'_>) -> Result<i64, ForkError> {
    let mut tx = pool.begin().await?;
    let next: Option<(i64,)> = sqlx::query_as(
        "select next_history_seq from sessions where agent = $1 and key = $2 for share",
    )
    .bind(fork.agent)
    .bind(fork.key)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((next,)) = next else {
        return Err(ForkError::NoSession);
    };
    let cut = match fork.through_turn {
        None => next - 1,
        Some(turn) => {
            let (last,): (Option<i64>,) = sqlx::query_as(
                "select max(seq) from history where agent = $1 and key = $2 and turn_id = $3",
            )
            .bind(fork.agent)
            .bind(fork.key)
            .bind(turn)
            .fetch_one(&mut *tx)
            .await?;
            last.ok_or(ForkError::NoTurn)?
        }
    };
    let created = sqlx::query(
        "insert into sessions (agent, key, next_history_seq) values ($1, $2, $3) \
         on conflict (agent, key) do nothing",
    )
    .bind(fork.into_agent)
    .bind(fork.into_key)
    .bind(cut + 1)
    .execute(&mut *tx)
    .await?;
    if created.rows_affected() == 0 {
        return Err(ForkError::Exists);
    }
    let copied = sqlx::query(
        "insert into history (agent, key, seq, turn_id, kind, entry, tokens, created_at) \
         select $3, $4, seq, turn_id, kind, entry, tokens, created_at from history \
         where agent = $1 and key = $2 and seq <= $5",
    )
    .bind(fork.agent)
    .bind(fork.key)
    .bind(fork.into_agent)
    .bind(fork.into_key)
    .bind(cut)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "insert into checkpoints (agent, key, through_seq, snapshot, summary, created_at) \
         select $3, $4, through_seq, snapshot, summary, created_at from checkpoints \
         where agent = $1 and key = $2 and through_seq <= $5",
    )
    .bind(fork.agent)
    .bind(fork.key)
    .bind(fork.into_agent)
    .bind(fork.into_key)
    .bind(cut)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(i64::try_from(copied.rows_affected()).unwrap_or(i64::MAX))
}
