#!/usr/bin/env bash
set -uo pipefail

input=$(cat)
if printf '%s' "$input" | grep -q '"stop_hook_active"[[:space:]]*:[[:space:]]*true'; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}" || exit 0
if [ -z "$(git status --porcelain -- crates migrations Cargo.toml Cargo.lock 2>/dev/null)" ]; then
  exit 0
fi

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export DATABASE_URL="${DATABASE_URL:-postgres://newt:newt@localhost:55432/newt}"

fail() {
  printf '%s\n\n%s\n' "$1" "$2" | tail -n 60 >&2
  exit 2
}

out=$(cargo fmt --check 2>&1) || fail "cargo fmt --check failed. Run cargo fmt." "$out"
out=$(cargo clippy --workspace --all-targets --quiet -- -D warnings 2>&1) || fail "clippy failed. Fix every warning." "$out"
out=$(cargo test --workspace --quiet 2>&1) || fail "tests failed." "$out"
exit 0
