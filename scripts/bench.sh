#!/usr/bin/env bash
# The newt benchmark: one command, no model provider, no API key.
#
#   scripts/bench.sh [sessions] [turns]        (defaults: 200 sessions, 3 turns)
#
# Builds the release binaries, starts the loadtest's fake model (prompt caching
# simulated per key and spread over 4 machines, as OpenAI routes), the mock app
# (a production-sized prompt, workspace and tool latencies) and newt on its own
# ports, runs every session at once, and prints the load report, newt's CPU and
# memory, and the cache report card. Needs Postgres: NEWT_DATABASE_URL, or the
# repo's docker-compose.yml database. See docs/benchmark.md.
set -euo pipefail

sessions=${1:-200}
turns=${2:-3}
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
export NEWT_DATABASE_URL=${NEWT_DATABASE_URL:-postgres://newt:newt@localhost:55432/newt}
export BENCH_TOKEN=bench-token
ulimit -n 10240 2>/dev/null || true

cargo build --release --quiet --manifest-path "$root/Cargo.toml" -p newt-server -p newt-loadtest
bin="$root/target/release"

cat > "$work/server.toml" <<EOF
server = { listen = "127.0.0.1:7979" }
database = { url_env = "NEWT_DATABASE_URL", max_connections = 20 }
events = { jsonl = "$work/events.jsonl" }
runner = { max_sessions = 1000 }

[routes.fake]
protocol = "openai-responses"
model = "fake-model"
base_url = "http://127.0.0.1:9100/v1"
key_env = "BENCH_TOKEN"
max_concurrent = 1000

[models.default]
chain = ["fake"]

[agents.bench]
context_provider = "http://127.0.0.1:9200/agent"
token_env = "BENCH_TOKEN"
EOF

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

"$bin/newt-loadtest" fake-model --listen 127.0.0.1:9100 --cache-scope routed --machines 4 > "$work/fake.log" 2>&1 &
pids+=($!)
"$bin/newt-loadtest" mock-app --listen 127.0.0.1:9200 > "$work/mock.log" 2>&1 &
pids+=($!)
NEWT_CONFIG="$work/server.toml" RUST_LOG=warn "$bin/newt" > "$work/newt.log" 2>&1 &
newt=$!
pids+=($newt)
for _ in $(seq 1 100); do curl -sf localhost:7979/healthz > /dev/null && break; sleep 0.1; done
curl -sf localhost:7979/healthz > /dev/null

"$bin/newt-loadtest" run --base http://127.0.0.1:7979 --agent bench --token-env BENCH_TOKEN \
  --sessions "$sessions" --turns "$turns" --concurrency "$sessions" --think-ms 2000 --timeout-s 300 \
  --fake http://127.0.0.1:9100 --mock http://127.0.0.1:9200

echo
echo "newt: cpu $(ps -o time= -p "$newt" | tr -d ' '), rss $(( $(ps -o rss= -p "$newt") / 1024 )) MiB, warnings $(grep -ciE 'warn|error' "$work/newt.log" || true)"
"$bin/newt-report" --summary "$work/events.jsonl" | sed -n '/cache report card/,/^$/p'
echo "logs and events: $work"
