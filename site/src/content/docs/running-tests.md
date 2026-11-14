---
title: Running the tests
description: Run newt's test suite against a throwaway Postgres, and the checks CI runs.
---

```sh
docker compose up -d --wait
export DATABASE_URL=postgres://newt:newt@localhost:55432/newt
cargo test --workspace
```

- Tests in `newt-server`, `newt-demo` and `newt-loadtest` use Postgres: each `#[sqlx::test]` creates a throwaway database from `DATABASE_URL` and applies `migrations/`.
- The `newt-core` tests need no database.
- No test calls a real model provider.

CI also runs:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
cargo machete
```

See [Contributing](../contributing/) for setup and code rules.
