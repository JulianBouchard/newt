#!/usr/bin/env bash
# Runs the benchmark workload against one runtime, sped up, and splits every turn into
# the runtime's own time and the time spent waiting on the simulated model and tools.
# The waiting scales back to real speed, so the report gives turns as they would take
# in production from a run that takes seconds. See docs/benchmark.md.
#
#   scripts/bench-compare.sh newt|eve [sessions] [turns] [speed]
#
# Defaults: 20 sessions, 3 turns, speed 50. Production settings: a pool of 5
# database connections, sessions pausing 20 to 90 simulated seconds between messages.
set -euo pipefail

runtime=${1:?usage: bench-compare.sh newt|eve [sessions] [turns] [speed]}
sessions=${2:-20}
turns=${3:-3}
speed=${4:-50}
root=$(cd "$(dirname "$0")/.." && pwd)
offset=${BENCH_PORT_OFFSET:-0}
newt_port=$((7979 + offset))
model_port=$((9100 + offset))
app_port=$((9200 + offset))
eve_port=$((3100 + offset))
work=$(mktemp -d)
bench_db=newt_bench_${BENCH_PORT_OFFSET:-0}
db_container=$(docker ps -q --filter publish=55432 | head -1)
docker exec "$db_container" psql -U newt -d postgres -qc "drop database if exists $bench_db with (force)" > /dev/null
docker exec "$db_container" psql -U newt -d postgres -qc "create database $bench_db" > /dev/null
export NEWT_DATABASE_URL=${NEWT_DATABASE_URL:-postgres://newt:newt@localhost:55432/newt}
export NEWT_DATABASE_URL="${NEWT_DATABASE_URL%/*}/$bench_db"
export BENCH_TOKEN=bench-token
ulimit -n 10240 2>/dev/null || true

cargo build --release --quiet --manifest-path "$root/Cargo.toml" -p newt-server -p newt-loadtest
bin="$root/target/release"

pids=()
cleanup() { kill ${pids[@]+"${pids[@]}"} 2>/dev/null || true; wait 2>/dev/null || true; pids=(); }
trap cleanup EXIT

wait_for() {
  for _ in $(seq 1 300); do curl -sf "$1" > /dev/null && return 0; sleep 0.1; done
  echo "not ready: $1" >&2
  return 1
}

start_newt() {
  local live_stream_line=""
  [ "${BENCH_LIVE_TEXT:-true}" = true ] && live_stream_line="live_stream = true"
  cat > "$work/server.toml" <<EOF
server = { listen = "127.0.0.1:${newt_port}" }
database = { url_env = "NEWT_DATABASE_URL", max_connections = 5 }
events = { jsonl = "$work/events-$1.jsonl" }

[routes.fake]
protocol = "openai-responses"
model = "fake-model"
base_url = "http://127.0.0.1:${model_port}/v1"
key_env = "BENCH_TOKEN"
max_concurrent = 1000

[models.default]
chain = ["fake"]

[agents.bench]
mcp = "http://127.0.0.1:${app_port}/mcp"
$live_stream_line
token_env = "BENCH_TOKEN"
EOF
  NEWT_CONFIG="$work/server.toml" RUST_LOG=warn "${BENCH_NEWT_BIN:-$bin/newt}" > "$work/newt-$1.log" 2>&1 &
  runtime_pid=$!
  pids+=("$runtime_pid")
  wait_for http://127.0.0.1:${newt_port}/healthz
  target=(--base http://127.0.0.1:${newt_port} --agent bench)
}

prepare_newt() { profile=(); [ -n "${BENCH_PROFILE:-}" ] && profile=(--profile "$BENCH_PROFILE"); return 0; }
stop_newt_sandboxes() { :; }

prepare_eve() {
  agent="$root/bench/eve"
  export BENCH_MODEL_URL=http://127.0.0.1:${model_port}/v1 BENCH_MODEL_KEY=$BENCH_TOKEN BENCH_MOCK_URL=http://127.0.0.1:${app_port}
  export WORKFLOW_TARGET_WORLD=@workflow/world-postgres
  export WORKFLOW_POSTGRES_URL=${WORKFLOW_POSTGRES_URL:-postgres://newt:newt@localhost:55432/eve_bench}
  export WORKFLOW_POSTGRES_URL="${WORKFLOW_POSTGRES_URL%/*}/eve_bench_${BENCH_PORT_OFFSET:-0}"
  local db
  db=$(docker ps -q --filter publish=55432 | head -1)
  docker exec "$db" psql -U newt -d postgres -qc "drop database if exists eve_bench_${BENCH_PORT_OFFSET:-0} with (force)" > /dev/null
  docker exec "$db" psql -U newt -d postgres -qc "create database eve_bench_${BENCH_PORT_OFFSET:-0}" > /dev/null
  (cd "$agent" && npm install --no-audit --no-fund --silent)
  export BENCH_PROFILE_FILE=${BENCH_PROFILE_EVE:-$agent/profile.json}
  if [ -n "${BENCH_PROFILE_EVE:-}" ]; then
    local copy="$work/eve-agent"
    rsync -a --exclude node_modules --exclude .output --exclude .eve "$agent/" "$copy/"
    ln -s "$agent/node_modules" "$copy/node_modules"
    find "$copy/agent/tools" -name '*.ts' ! -name read_file.ts ! -name grep.ts ! -name glob.ts -delete
    node -e '
      const [profile, dir] = process.argv.slice(1);
      const fs = require("fs");
      for (const tool of JSON.parse(fs.readFileSync(profile, "utf8")).tools) {
        if (tool.kind === "end") {
          fs.writeFileSync(`${dir}/${tool.name}.ts`, `import { noopTool } from "../../lib/mock";\n\nexport default noopTool();\n`);
          continue;
        }
        if (!["read", "write", "send"].includes(tool.kind)) continue;
        fs.writeFileSync(`${dir}/${tool.name}.ts`, `import { mockTool } from "../../lib/mock";\n\nexport default mockTool(${JSON.stringify(tool.name)});\n`);
      }' "$BENCH_PROFILE_EVE" "$copy/agent/tools"
    agent="$copy"
  fi
  (cd "$agent" && npx --package=@workflow/world-postgres bootstrap > "$work/world-setup.log" 2>&1)
  (cd "$agent" && npx eve build > "$work/eve-build.log" 2>&1)
  profile=(--profile "${BENCH_PROFILE_EVE:-$agent/profile.json}")
}

start_eve() {
  (cd "$agent" && PORT=${eve_port} exec npx eve start --host 127.0.0.1 > "$work/eve-$1.log" 2>&1) &
  pids+=($!)
  wait_for http://127.0.0.1:${eve_port}/eve/v1/health
  runtime_pid=$(lsof -ti tcp:${eve_port} -sTCP:LISTEN | head -1)
  target=(--runtime eve --base http://127.0.0.1:${eve_port})
}

stop_eve_sandboxes() {
  docker ps -aq --filter name=eve-sbx- | xargs -r docker rm -f > /dev/null 2>&1 || true
}

one_run() {
  local speed=$1
  "$bin/newt-loadtest" fake-model --listen 127.0.0.1:${model_port} --speed "$speed" --cache-scope routed --machines 4 --turn-marker "load message" --finish "${BENCH_FINISH:-reply-then-end}" ${profile[@]+"${profile[@]}"} ${BENCH_REPLAY:+--replay "$BENCH_REPLAY"} ${BENCH_DUMP_DIR:+--dump-dir "$BENCH_DUMP_DIR"} \
    --interval-log "$work/model-intervals.jsonl" --cache-log "$work/cache-$speed.jsonl" > "$work/fake-$speed.log" 2>&1 &
  pids+=($!)
  "$bin/newt-loadtest" mock-app --listen 127.0.0.1:${app_port} --speed "$speed" --interval-log "$work/tool-intervals.jsonl" --require-end-turn "${BENCH_REQUIRE_END_TURN:-false}" --prompt-bytes "${BENCH_PROMPT_BYTES:-30000}" --render-ms "${BENCH_RENDER_MS:-0}" ${profile[@]+"${profile[@]}"} > "$work/mock-$speed.log" 2>&1 &
  pids+=($!)
  wait_for http://127.0.0.1:${model_port}/stats
  wait_for http://127.0.0.1:${app_port}/stats
  "start_$runtime" "$speed"
  echo "== $runtime at speed $speed"
  "$bin/newt-loadtest" run "${target[@]}" --token-env BENCH_TOKEN \
    --sessions "$sessions" --turns "$turns" --concurrency "$sessions" \
    --think-ms 20000 --think-max-ms 90000 --ramp-s 60 --timeout-s 600 --speed "$speed" \
    --fake http://127.0.0.1:${model_port} --mock http://127.0.0.1:${app_port} --turns-log "$work/turns-$speed.jsonl"
  echo "$runtime: cpu $(ps -o time= -p "$runtime_pid" | tr -d ' '), rss $(( $(ps -o rss= -p "$runtime_pid") / 1024 )) MiB"
  cleanup
  "stop_${runtime}_sandboxes"
}

"prepare_$runtime"
started=$(date +%s)
one_run "$speed"
echo
echo "== $runtime at real speed"
"$bin/newt-loadtest" decompose --turns "$work/turns-$speed.jsonl" --speed "$speed" \
  --intervals "$work/model-intervals.jsonl,$work/tool-intervals.jsonl" ${BENCH_REPLAY:+--replay "$BENCH_REPLAY"}
echo "wall clock $(( $(date +%s) - started )) s, logs in $work"
