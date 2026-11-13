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
- The cache report card from `newt-report`: the cached share of all input, and every cache miss with its cause, including a session moved to another cache key.
- The fake model's cache log (`cache.jsonl` in the run directory): one line per request with its cache key, the machine it went to and the key's home machine, the key's requests in the last minute, and how many leading prompt blocks that machine held, could read, or did not have.
- Time spent waiting for a database connection when accepting an event, claiming the session and loading history, in `newt-report --summary`.

Turn duration is dominated by the fake model's simulated latency and the mock tools, so it says whether newt adds waiting, not how fast a real model is. The numbers that are newt's own are time to read receipt, time to turn start, the cached share and the miss causes, and CPU and memory per session.

## Reading a result

Run it on a quiet machine: other load shows up as newt CPU time and as slower Postgres. Use a fresh database: the same code read a p90 receipt of 1,080 ms on a long-used dev database and 307 ms on a new one. Compare runs of one commit against another at the same session count, not against numbers from another machine. A failed turn, a miss labelled anything but `first call of the session`, or warnings in newt's log are worth a look before any timing.

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

## Per milestone

The same harness (HEAD's fake model, mock app and driver), newt built at each milestone commit, a fresh database per run, on 2026-11-12. The oldest and newest commits ran twice to show the noise. Score: the mean of four parts, each 0 to 100: turns completed; read receipt p90 (100 ms to 3 s) and turn start p90 (250 ms to 5 s); cached share of input (50% to 90%); CPU per model call (2 ms to 50 ms) and memory per session (0.25 MiB to 5 MiB). Latency, CPU and memory are scored on a log scale between those ends.

| Commit | Milestone | Score | Read receipt p90 | Turn start p90 | Cached | newt CPU per model call |
|---|---|---|---|---|---|---|
| `d7e6e89` | Runtime complete | 74.9 / 71.0 | 280 / 330 ms | 864 / 2,243 ms | 80.5 / 80.9% | 28.0 / 24.5 ms |
| `4a77802` | Hot path | 77.0 | 227 ms | 576 ms | 80.3% | 29.3 ms |
| `81e6a76` | Shared cache keys | 73.9 | 250 ms | 569 ms | 75.9% | 29.7 ms |
| `2c7c472` | Hardening | 70.7 | 410 ms | 789 ms | 75.9% | 29.9 ms |
| `0f992bb` | MCP 2026-07-28 | 75.0 | 225 ms | 481 ms | 75.9% | 29.7 ms |
| `69f29b5` | MCP-native agent | 72.2 | 307 ms | 716 ms | 75.7% | 28.9 ms |
| `80d8a89` | Wakeup fix, embedded Postgres | 84.9 / 85.5 | 217 / 195 ms | 499 / 519 ms | 77.7 / 78.4% | 3.1 / 3.1 ms |

All 600 turns completed in every run. The features of six milestones left the score between 71 and 77; the wakeup fix in `0673c06` is the one change that moved it. A move of 3 points or less is within the noise. In this simulator sending no cache key (before `81e6a76`) caches more than shared keys; a replay against the real provider showed the reverse, so the cached share here reflects the fake model's four machines.
