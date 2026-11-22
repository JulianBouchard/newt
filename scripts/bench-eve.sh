#!/usr/bin/env bash
# The eve side of the benchmark: the same fake model, mock app and driver as
# scripts/bench.sh, against eve 0.83.3 built and started in production mode.
#
#   scripts/bench-eve.sh [sessions] [turns] [steady|burst] [speed]   (defaults: 200, 3, steady, 1)
#
# eve runs `eve build` then `eve start` with the Postgres Workflow world
# (@workflow/world-postgres, database eve_bench in the repo's docker-compose
# Postgres) and the Docker sandbox provider. Needs Docker and Node. See
# docs/benchmark.md for the metrics and bench/eve for the agent.
set -euo pipefail

sessions=${1:-200}
turns=${2:-3}
scenario=${3:-steady}
speed=${4:-1}
case "$scenario" in
  steady) pacing=(--think-ms 20000 --think-max-ms 90000 --ramp-s 60) ;;
  burst) pacing=(--think-ms 2000) ;;
  *) echo "scenario must be steady or burst" >&2; exit 2 ;;
esac
root=$(cd "$(dirname "$0")/.." && pwd)
agent="$root/bench/eve"
work=$(mktemp -d)
export BENCH_TOKEN=bench-token
export BENCH_MODEL_URL=http://127.0.0.1:9100/v1
export BENCH_MODEL_KEY=bench-token
export BENCH_MOCK_URL=http://127.0.0.1:9200
export WORKFLOW_TARGET_WORLD=@workflow/world-postgres
export WORKFLOW_POSTGRES_URL=${WORKFLOW_POSTGRES_URL:-postgres://newt:newt@localhost:55432/eve_bench}
psql_admin() { docker compose -f "$root/docker-compose.yml" exec -T postgres psql -U newt -d postgres "$@"; }
ulimit -n 10240 2>/dev/null || true

docker compose -f "$root/docker-compose.yml" up -d --wait postgres > /dev/null
psql_admin -tc "select 1 from pg_database where datname = 'eve_bench'" | grep -q 1 \
  || psql_admin -c "create database eve_bench" > /dev/null
(cd "$agent" && npm install --no-audit --no-fund --silent)
(cd "$agent" && npx --package=@workflow/world-postgres bootstrap > "$work/world-setup.log" 2>&1)
(cd "$agent" && npx eve build > "$work/eve-build.log" 2>&1)
cargo build --release --quiet --manifest-path "$root/Cargo.toml" -p newt-loadtest
bin="$root/target/release"

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

"$bin/newt-loadtest" fake-model --listen 127.0.0.1:9100 --speed "$speed" --cache-scope routed --machines 4 \
  --turn-marker "load message" --profile "$agent/profile.json" > "$work/fake.log" 2>&1 &
pids+=($!)
"$bin/newt-loadtest" mock-app --listen 127.0.0.1:9200 --speed "$speed" --profile "$agent/profile.json" > "$work/mock.log" 2>&1 &
pids+=($!)
(cd "$agent" && PORT=3100 exec npx eve start --host 127.0.0.1 > "$work/eve.log" 2>&1) &
eve=$!
pids+=($eve)
for _ in $(seq 1 200); do curl -sf localhost:3100/eve/v1/health > /dev/null && break; sleep 0.2; done
curl -sf localhost:3100/eve/v1/health > /dev/null

"$bin/newt-loadtest" run --runtime eve --base http://127.0.0.1:3100 --token-env BENCH_TOKEN \
  --sessions "$sessions" --turns "$turns" --concurrency "$sessions" "${pacing[@]}" --timeout-s 300 \
  --fake http://127.0.0.1:9100 --mock http://127.0.0.1:9200

echo
node_pid=$(lsof -ti tcp:3100 -sTCP:LISTEN | head -1 || true)
echo "eve: cpu $(ps -o time= -p "${node_pid:-$eve}" | tr -d ' '), rss $(( $(ps -o rss= -p "${node_pid:-$eve}") / 1024 )) MiB"
echo "logs: $work"
