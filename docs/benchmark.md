# Benchmark

`scripts/bench.sh [sessions] [turns]` measures newt under load with no model provider and no API key. It needs Postgres (`NEWT_DATABASE_URL`, or `docker compose up -d --wait` for the repo's dev database) and runs everything else itself.

## What runs

- **The fake model** (`newt-loadtest fake-model --cache-scope routed --machines 4`) speaks the OpenAI Responses stream. Its timings and tool use come from the profile of a production assistant, about 16 tool calls a turn. Prompt caching is simulated per `prompt_cache_key` and spread over four machines once a key passes 15 requests a minute, as OpenAI routes, so a key that is too busy or a rewritten history misses as it would against the real thing.
- **The mock app** (`newt-loadtest mock-app`) is a context provider with a production-sized prompt and workspace, and an MCP server whose tools have that assistant's latencies and error rates.
- **newt**, built in release mode, with default runner settings (`cache_key_buckets = "auto"`).
- **The driver** (`newt-loadtest run`) starts every session at once, waits 2 seconds between a session's turns, and follows each turn on the live feed.

## What it reports

- From the driver: time to the read receipt, time until the turn starts, turn duration, model calls and tool calls per turn, the cached share of input, and failed turns.
- newt's CPU time and resident memory at the end of the run, and how many warnings it logged.
- The cache report card from `newt-report`: the cached share of all input, and every cache miss with its cause.

Turn duration is dominated by the fake model's simulated latency and the mock tools, so it says whether newt adds waiting, not how fast a real model is. The numbers that are newt's own are time to read receipt, time to turn start, the cached share and the miss causes, and CPU and memory per session.

## Reading a result

Run it on a quiet machine: other load shows up as newt CPU time and as slower Postgres. Compare runs of one commit against another at the same session count, not against numbers from another machine. A failed turn, a miss labelled anything but `first call of the session`, or warnings in newt's log are worth a look before any timing.

No numbers are published here yet; the first run on a dedicated machine will add them, with the commit and the machine.
