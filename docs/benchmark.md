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

## Results

200 sessions, 3 turns each, on an Apple M5 Pro (15 cores, 24 GB) with Postgres 18 in OrbStack, on 2026-11-12. The same machine ran the fake model, the mock app and the driver.

| | `69f29b5` | `0152f99` |
|---|---|---|
| Turns completed | 600 of 600 | 600 of 600 |
| Model calls | 3,879 | 3,887 |
| newt CPU time | 1:56 | 0:12 |
| newt resident memory | 25 MiB | 26 MiB |
| Read receipt p50 / p90 | 3 / 1,080 ms | 4 / 285 ms |
| Event to turn start p50 / p90 | 12 / 2,044 ms | 13 / 586 ms |
| Turn duration p50 / p90 | 16.7 / 28.2 s | 16.3 / 27.4 s |
| Cached share of input | 76.0% | 77.9% |
| Misses: first call of a session | 200 | 200 |
| Misses: routing | 117 | 117 |
| Warnings in newt's log | 0 | 0 |

What changed between the two:

- **CPU.** At `69f29b5` every live feed and running turn woke for every notification on the instance; a profile showed most of newt's time waiting on that one channel's lock. `0673c06` wakes only the session a notification is for.
- **The p90 read receipt and turn start** come from the first turn, when all 200 sessions post in the same moment. After the first turn both are under 15 ms in both runs. `0152f99` opens every database connection at boot instead of growing the pool during that burst.
- **Routing misses** happen in the first minute, while auto cache keys grow from one key to about 130 and sessions move to new keys, each of which the fake model routes to another of its four machines. After the first 80 seconds there are almost none. `18cc08f` raised the key cap from 64 to 256 so that at this rate (about 1,600 calls a minute) each key stays under OpenAI's ~15 a minute; the fake model's four machines cannot show that difference.

Turn duration is the fake model's and the mock tools' simulated latency, as above. One MCP call timed out after 90 seconds in a run before `0152f99`: the mock app's latency tail, not newt.
