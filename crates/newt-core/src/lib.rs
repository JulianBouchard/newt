//! The agent turn loop, independent of any server or database.
//!
//! A turn sends a prompt in three layers: a frozen prefix (system text, skills index,
//! tools), the session's history, and the turn's scratch. Only the scratch changes
//! between model calls, so the provider's prompt cache keeps the rest. Tool results carry
//! a retention class: `ephemeral` results stay in the turn, `durable` ones are committed
//! to history, and `outward` calls are journaled before they run so a crash replays them
//! with the same idempotency key.
//!
//! Start at [`turn::run_turn`]; the workspace is in [`workspace`], providers in
//! [`provider`], and observability sinks in [`sink`].

mod batch;
pub mod batch_tool;
pub mod checkpoint;
mod commit;
pub mod event;
pub mod journal;
pub mod message;
mod nul;
pub mod prompt;
pub mod provider;
pub mod sink;
pub mod skills;
pub mod sse;
pub mod steering;
pub mod tools;
pub mod trace;
pub mod turn;
pub mod workspace;
