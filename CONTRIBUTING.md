# Contributing

Thanks for looking. newt is young and moves fast, so open an issue before you start on anything big.

## Setup

You need the Rust toolchain pinned in `rust-toolchain.toml` (1.99.1, with clippy and rustfmt; rustup installs it on first use) and Docker for Postgres.

```sh
docker compose up -d --wait     # Postgres 18 on localhost:55432
export DATABASE_URL=postgres://newt:newt@localhost:55432/newt
export NEWT_DATABASE_URL=$DATABASE_URL
```

- `DATABASE_URL` is read by the tests: each `#[sqlx::test]` creates a throwaway database from it and applies `migrations/`.
- `NEWT_DATABASE_URL` is the variable `server.example.toml` names for the server's own connection (`database.url_env`).

Run the server against the example config:

```sh
NEWT_CONFIG=server.example.toml cargo run -p newt-server
```

The example config also needs `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` and `ASSISTANT_TOKEN` set (any value works until a turn calls the model). The [Quickstart](https://newt.julianbouchard.com/quickstart/) walks through the demo.

## Checks

CI (`.github/workflows/ci.yml`) runs these against Postgres 18, in this order. Run them locally before you push:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check        # licences, yanked crates, wildcard versions, unknown registries
cargo machete           # unused dependencies
```

CI then builds the release binary, starts it with `server.example.toml`, waits for `/healthz` to return 200 and sends it SIGTERM, which must exit cleanly. `cargo deny` and `cargo machete` are installed with `cargo install cargo-deny cargo-machete`.

## Code rules the compiler enforces

These come from `[workspace.lints]` in `Cargo.toml`. With `-D warnings`, every warning below fails CI.

- No `unsafe` (`forbid`).
- No `unwrap()`, `panic!`, `todo!`, `unimplemented!` or `dbg!` (`deny`). `expect()` is a warning: use it only for a true invariant and say why in the message. Tests may unwrap by opening the file with `#![expect(clippy::unwrap_used, reason = "...")]`.
- No `println!` or `eprintln!` (`deny`). Use `tracing`.
- No holding a `Mutex` guard or a `RefCell` borrow across `.await` (`deny`).
- No `#[allow(...)]` (`deny`). Suppress a lint with `#[expect(lint, reason = "...")]`; the reason is required.
- Clippy `all` and `pedantic` are on. Three pedantic lints are switched off: `module_name_repetitions`, `missing_errors_doc`, `must_use_candidate`.
- Public types implement `Debug` (`missing_debug_implementations`).
- `#[must_use]` results must be used (`unused_must_use` is `deny`).
- Large futures, `Rc<Mutex<_>>` and `clone()` on reference-counted pointers are warnings.

## Style the compiler does not check

- Errors: `thiserror` enums in `newt-core`, `anyhow` with `.context(...)` in `newt-server`. Do not drop the source error.
- Fail loudly on bad config or a bad provider response instead of falling back.
- Prefer concrete types. Add a trait only where the design already has a seam (`EventSink`, `Model`, `ToolHost`, `Journal`).
- Do not block the async runtime. Use tokio's APIs or `spawn_blocking`.
- Comments only for a constraint the code cannot express.
- A new dependency needs a reason in the pull request, and `cargo deny check` must pass.

## Tests

Tests check behaviour on inputs. A bug fix comes with a test that fails without the fix. No test calls a real model, and the `newt-core` tests don't need Postgres.

## Pull requests

- One change per pull request, with a description of what changed and why.
- Rebase on `main` and run the checks above first.
- If you change the `newt` MCP extension, a `_meta` field or a config key, update `docs/protocol.md` or `docs/config.md` and the matching guide under `site/` in the same pull request.
- By contributing you agree your work is licensed as `Apache-2.0 OR MIT`, like the rest of the project.

For security issues, see `SECURITY.md`. Please follow `CODE_OF_CONDUCT.md`.
