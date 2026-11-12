//! The newt server: an HTTP event API, a Postgres-backed runner and the adapters that feed
//! turns from a context provider and MCP tool servers.
//!
//! Events land in a per-session inbox; the runner leases sessions, runs turns with
//! [`newt_core`], and commits history and journal entries in Postgres. See `docs/` in the
//! repository for the protocol, configuration and design.

pub mod backend;
pub mod checkpoints;
mod compaction;
pub mod config;
pub mod context;
pub mod db;
pub mod http;
pub mod journal;
pub mod journal_store;
pub mod live;
pub mod mcp;
pub mod models;
pub mod profile;
pub mod runner;
pub mod sessions;
pub mod sinks;
pub mod steering;
pub mod store;
mod turn_runner;
