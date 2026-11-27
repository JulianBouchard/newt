mod commit;
mod leases;
mod turn_ends;

pub(crate) use commit::{fence, insert_history};

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use newt_core::message::Message;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub agent: String,
    pub key: String,
    pub owner: String,
    pub epoch: i64,
    /// Failed attempts at the session's turn when the lease was taken: each
    /// failed turn counts, and so does taking over a lease that expired,
    /// which is what a crash mid-turn leaves. A commit resets it.
    pub failed_attempts: i32,
    /// Whether the lease was taken over from an owner that let it expire
    /// instead of releasing it.
    pub taken_over: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEvent {
    pub inbox_id: i64,
    pub event_id: String,
    pub interactive: bool,
    pub steer: bool,
    pub text: String,
    pub meta: Option<Value>,
}

/// What a turn's commit writes besides its history: the events it consumed,
/// the provider's new session state, and the turn-end callback to deliver.
#[derive(Debug, Clone, Default)]
pub struct TurnCommit {
    pub turn_id: String,
    pub messages: Vec<Message>,
    pub consumed: Vec<i64>,
    pub provider_state: Option<Value>,
    pub turn_end: Option<Value>,
    /// The turn ran on a reset history: when the commit adds history rows, it
    /// marks everything before the first of them as no longer part of the
    /// session's conversation. A commit with no rows resets nothing.
    pub reset: bool,
}

/// A turn-end callback claimed for delivery. `attempts` counts this one.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnd {
    pub id: i64,
    pub agent: String,
    pub payload: Value,
    pub attempts: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("the lease moved to another owner or epoch; nothing was committed")]
    Fenced,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub type Owned<T, E = sqlx::Error> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'static>>;

fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}
