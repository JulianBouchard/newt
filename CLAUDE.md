# newt

An agent runtime: a long-lived agent per customer, the app's backend as its world, and a database's discipline about context. Rust, one binary plus Postgres.

- `crates/newt-core`: library. Event model, sinks, and (later) the agent loop, context model and provider clients. No HTTP server, no database.
- `crates/newt-server`: the `newt` binary. Config (`server.toml`), Postgres (sqlx, migrations in `migrations/`), HTTP (axum).

The owner is learning Rust and reviews what you write. Write code a TypeScript engineer can follow: plain types, plain functions, few generics.

## Commands

```sh
docker compose up -d --wait          # local Postgres on :55432
export DATABASE_URL=postgres://newt:newt@localhost:55432/newt   # server tests create a database per test
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
NEWT_CONFIG=server.example.toml NEWT_DATABASE_URL=postgres://newt:newt@localhost:55432/newt cargo run -p newt-server
```

Run fmt, clippy and the tests after every change. A Stop hook enforces it. Never finish with warnings.

## Rules

1. **No `unsafe`.** Forbidden at the workspace level.
2. **No panics in non-test code.** No `unwrap`, `panic!`, `todo!`, `unimplemented!`. `expect` only for true invariants, with a message saying why it can't fail. Tests may unwrap.
3. **Errors:** `thiserror` enums in `newt-core`, `anyhow` with `.context(...)` in `newt-server`. Never `String` errors, never drop the source, never `let _ =` on a `Result`.
4. **Suppress lints only with `#[expect(lint, reason = "...")]`.** `#[allow]` is denied.
5. **Async:** never block the runtime (no `std::fs`, `std::thread::sleep` or heavy CPU in async code; use tokio or `spawn_blocking`). Never hold a lock across `.await`. Channels are bounded, and every one has a decision about what happens when it's full. Spawned tasks are owned: keep the `JoinHandle` or a `CancellationToken`.
6. **Ownership before sharing.** Reach for owned data, `Arc<T>` of immutable data, or a channel before `Arc<Mutex<T>>`. Every `.clone()` must be cheap or necessary.
7. **Concrete before abstract.** No trait with one implementer unless it's a seam the design calls for (`EventSink`, providers). No builders for small structs, no macros of our own.
8. **Types carry meaning.** Enums for closed sets, newtypes for ids once they cross module boundaries, serde types for every JSON shape. No hand-built JSON strings.
9. **Postgres:** runtime queries via sqlx; transactions around anything that must commit together; never hold a connection across a model call.
10. **Fail loudly.** A bad config, a missing env var, an invalid bootstrap: error with a clear message. No silent fallbacks.
11. **Tests assert behaviour on inputs.** No tests that restate the implementation. Bug fixes come with a test that fails without the fix.
12. **Comments only for a constraint the code can't express.** Names and types carry the meaning.
13. **New dependency:** say why, check it's maintained, keep default features off where it's heavy. `cargo deny check` must pass.

## Review

Before calling work done, run the `rust-review` skill on the diff.

## Design

Decisions live in the design doc (private for now). The parts that shape code: retention classes (`ephemeral`, `durable`, `outward`) decide what reaches history; outward tools are exactly-once through idempotency keys and a journal; the turn waits on Postgres only before outward calls and at commit; one typed event stream feeds tracing, the live feed and webhooks.
