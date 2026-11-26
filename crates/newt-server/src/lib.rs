//! The newt server: an HTTP event API, a Postgres-backed runner, and the MCP client that
//! runs each agent from its home server and the servers it adds.
//!
//! Events land in a per-session inbox; the runner leases sessions, runs turns with
//! [`newt_core`], and commits history and journal entries in Postgres. See `docs/` in the
//! repository for the protocol, configuration and design.

pub mod backend;
pub mod cache_card;
pub mod cache_keys;
pub mod cancel;
pub mod checkpoints;
mod compaction;
pub mod config;
pub mod counters;
pub mod db;
#[cfg(feature = "embedded-postgres")]
pub mod embedded;
pub mod events;
pub mod fork;
pub mod home;
pub mod http;
pub mod journal;
pub mod journal_store;
pub mod live;
pub mod mcp;
pub mod metrics;
pub mod models;
pub mod profile;
pub mod replay;
pub mod retention;
pub mod runner;
pub mod sessions;
pub mod sinks;
pub mod steering;
pub mod store;
pub mod testing;
mod turn_runner;
pub mod turn_start;
