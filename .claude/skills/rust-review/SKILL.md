---
name: rust-review
description: Review a Rust diff in this repo before calling work done. Two fresh-context reviewers check it against the checklist, then you fix what they agree on. Use after any change under crates/ or migrations/, or when asked to review Rust.
---

# Rust review

The Bun port to Rust worked because every file got two adversarial reviewers in separate contexts, then a fixer. Do the same here.

## Steps

1. Run `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`. Fix failures first; review never replaces the compiler.
2. Get the diff: `git diff` (plus untracked files under `crates/` and `migrations/`).
3. Start two reviewer subagents in parallel. Give each the diff, the checklist below, and the rules in `CLAUDE.md`. Tell them to report only real problems as `file:line: problem. fix.`, and to say "clean" if there are none. Do not share your reasoning with them.
4. Fix every finding both reviewers raise. For a finding only one raises, check it yourself and fix it or note why not.
5. Re-run step 1. Report what changed in one short list.

## Checklist

1. Each `.clone()`: needed, or is it silencing the borrow checker? Inside a loop or on a large value?
2. `Arc<Mutex<T>>`: could ownership, immutable `Arc<T>` or a channel do it?
3. Any lock or `RefCell` borrow held across `.await`?
4. Blocking work in async code: `std::fs`, `std::thread::sleep`, sync clients, heavy CPU?
5. Channels: bounded, with a stated behaviour when full?
6. Cancellation: in each `select!` branch and each spawned task, what is lost if the future is dropped? Are tasks owned (handle or token)?
7. Panics: `unwrap`, `expect` without an invariant, indexing that can be out of bounds, `let _ =` on a `Result`.
8. Errors: right type for the crate, source kept, context added, nothing swallowed.
9. Abstraction: traits with one implementer, deep generic bounds, `Box<dyn Trait>` where a concrete type works, builders for small structs.
10. Lifetimes: several explicit lifetimes or `'static` forced in? Usually the data should be owned.
11. Lint suppressions: `#[expect(..., reason)]` only, and the reason is true.
12. Stringly typed: ids, statuses or JSON as raw strings where a type belongs.
13. Postgres: missing transaction around things that commit together, N+1 queries, a connection held across a slow call, a migration that isn't safe on a non-empty table.
14. Tests: assert behaviour on inputs, cover the error path, don't restate the implementation.
15. Dependencies: new crate justified, maintained, minimal features.
