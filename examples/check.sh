#!/usr/bin/env bash
# Starts newt on the repo's fake model and mock app, then runs one example's check.mjs
# against it: ai-sdk-chat drives /chat with the AI SDK, eve-react the eve routes with
# eve's own client.
#   examples/check.sh ai-sdk-chat|eve-react [speed]
set -euo pipefail

example=${1:?usage: examples/check.sh ai-sdk-chat|eve-react [speed]}
speed=${2:-5}
root=$(cd "$(dirname "$0")/.." && pwd)
dir="$root/examples/$example"
[ -f "$dir/check.mjs" ] || { echo "no such example: $example" >&2; exit 1; }
case $example in
  ai-sdk-chat) default_offset=3000 ;;
  *) default_offset=3100 ;;
esac
offset=${PORT_OFFSET:-$default_offset}
newt_port=$((7979 + offset))
model_port=$((9100 + offset))
app_port=$((9200 + offset))
work=$(mktemp -d)
db=newt_example_${offset}
db_container=$(docker ps -q --filter publish=55432 | head -1)
[ -n "$db_container" ] || { echo "no Postgres container publishes port 55432: run docker compose up -d --wait" >&2; exit 1; }
docker exec "$db_container" psql -U newt -d postgres -qc "drop database if exists $db with (force)" > /dev/null
docker exec "$db_container" psql -U newt -d postgres -qc "create database $db" > /dev/null
export NEWT_DATABASE_URL="postgres://newt:newt@localhost:55432/$db"
export BENCH_TOKEN=bench-token

cargo build --release --quiet --manifest-path "$root/Cargo.toml" -p newt-server -p newt-loadtest
bin="$root/target/release"
[ -d "$dir/node_modules" ] || npm install --no-audit --no-fund --prefix "$dir" > /dev/null

pids=()
cleanup() { kill ${pids[@]+"${pids[@]}"} 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

wait_for() {
  for _ in $(seq 1 300); do curl -sf "$1" > /dev/null && return 0; sleep 0.1; done
  echo "not ready: $1" >&2
  return 1
}

"$bin/newt-loadtest" fake-model --listen 127.0.0.1:${model_port} --speed "$speed" --cache-scope routed --machines 1 --finish reply-then-end > "$work/fake.log" 2>&1 &
pids+=($!)
"$bin/newt-loadtest" mock-app --listen 127.0.0.1:${app_port} --speed "$speed" > "$work/mock.log" 2>&1 &
pids+=($!)
wait_for http://127.0.0.1:${model_port}/stats
wait_for http://127.0.0.1:${app_port}/stats

cat > "$work/server.toml" <<EOF
server = { listen = "127.0.0.1:${newt_port}" }
database = { url_env = "NEWT_DATABASE_URL", max_connections = 5 }

[routes.fake]
protocol = "openai-responses"
model = "fake-model"
base_url = "http://127.0.0.1:${model_port}/v1"
key_env = "BENCH_TOKEN"
max_concurrent = 100

[models.default]
chain = ["fake"]

[agents.bench]
mcp = "http://127.0.0.1:${app_port}/mcp"
live_stream = true
token_env = "BENCH_TOKEN"
EOF
NEWT_CONFIG="$work/server.toml" RUST_LOG=warn "$bin/newt" > "$work/newt.log" 2>&1 &
pids+=($!)
wait_for http://127.0.0.1:${newt_port}/healthz

NEWT_URL=http://127.0.0.1:${newt_port} NEWT_AGENT=bench NEWT_TOKEN=$BENCH_TOKEN \
  node "$dir/check.mjs"
