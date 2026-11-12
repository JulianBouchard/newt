# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). newt is pre-1.0 and has no tagged releases yet.

## [Unreleased]

### Added

- **Runtime.** Turn loop with the frozen prefix, history and per-turn scratch layers, and retention classes (`ephemeral`, `durable`, `outward`) that decide what reaches history. Steering of a running turn by new events, a batch tool, and a crash-safe turn journal with exactly-once outward calls through idempotency keys. A Postgres runner with fenced leases, `NOTIFY` wakeups, a periodic sweep, retry backoff and dead-lettering. Events API with read receipts and a live SSE feed.
- **Providers.** Streaming clients for the Anthropic Messages and OpenAI Responses protocols, with per-route concurrency limits that reserve a slot for interactive turns, one retry of a retryable error before anything has streamed, fallback along a model alias's route chain, and a 10 minute deadline per model call. Prompt cache keys shared across an agent's sessions (`runner.cache_key_buckets`).
- **Workspace.** A reactive workspace of pages fed by a context provider: invalidation by key prefix, refetch on read, page data returned by tools, and lazy loading that runs the first model call while pages load in the background. Pinned turn context that joins the cached prefix.
- **Context provider.** Bootstrap per version, per-turn context (`instructions`, `constraints`, `pinned`) and page refetch over HTTP. Skills as a first-class feature, loaded on demand with `load_skill`, and visible to tools through `newt/skills_loaded`.
- **MCP.** Tools from MCP servers over Streamable HTTP, with the `newt/*` `_meta` fields for retention, invalidation, idempotency, summaries and page data.
- **Compaction.** Checkpoints written by the agent's own model, with commitments that cannot be dropped silently, a kept tail of recent turns, and raw history left in place. The built-in `remember` tool for durable facts.
- **Observability.** One typed event stream feeding a JSONL file, the live feed and an OTLP/HTTP sink (one trace per turn, with a Langfuse profile, an enricher and a redactor). `newt-report` summarises timing, cache hits and flags from the JSONL file.
- **Load testing.** `newt-loadtest` with a fake model that speaks the OpenAI Responses stream and simulates prompt caching, a mock context provider and MCP server, and a driver that reports read receipt, turn latency, model calls, cache rate and tool calls.
- **Demo.** `newt-demo`, a personal assistant that serves a context provider and MCP tools in one process.
- **Workspace setup.** Cargo workspace with `newt-core`, `newt-server`, `newt-demo` and `newt-loadtest`; dual `Apache-2.0 OR MIT` licensing; CI running format, clippy, tests, `cargo deny`, `cargo machete` and a boot smoke test; the newt logo.
