<h1 align="center"><img src="assets/logo/newt-lockup.svg" alt="newt" width="260"></h1>

An agent runtime for products: a long-lived agent per customer, your backend as its world, and a database's discipline about context.

Newts regrow what they lose. So does this: every turn strips the context back and grows it from the workspace and the history, so stale reads never pile up and the prompt cache stays warm.

In it: streaming clients for Anthropic, OpenAI and any Chat Completions host (vLLM, Ollama, OpenRouter), the turn loop with retention classes, steering, a crash-safe journal, the Postgres runtime, agents made of MCP servers (reached over HTTP or run by newt over stdio), a reactive workspace fed by their resources, approvals and tasks over MCP, compaction and OTLP tracing.

**Status:** stable since 1.0. Each release ships `newt` binaries for Linux (x86_64) and macOS (arm64) and a Docker image at `ghcr.io/julianbouchard/newt`. To build from source you need Rust 1.99. newt runs its own Postgres 18 or connects to yours.

| Maturity | What |
|---|---|
| Stable | The turn loop and retention classes, steering, the crash-safe journal, fenced leases and several instances on one Postgres, the events API and live feed, deleting sessions and pruning old rows, the home server and the `newt` MCP extension, servers the home adds, the stdio transport, approvals through MCP `input_required`, MCP tasks, compaction, prompt cache keys, OTLP tracing, embedded Postgres |
| Experimental | Skills over MCP (SEP-2640, a draft spec); replay and fork; the `openai-chat` protocol; the turn token budget; recitation (`recite` in the home's settings, off by default) |

Experimental features work and are tested, including end to end against `newt-demo`, but the specs they follow or the shape of their settings may still change.

### Stability

Newt follows semantic versioning. 2.0 replaced the HTTP context provider with the home MCP server, and 2.1 settled a few of its defaults and responses (see [CHANGELOG.md](CHANGELOG.md)). From 2.1 on, nothing below changes within 2.x in a way that breaks a working setup:

- the event API (`docs/protocol.md`, section 5): paths, fields, status codes;
- the `newt` MCP extension: the home's declaration, `newt/turn`, `newt/turn_end` and the `newt/*` `_meta` fields (sections 1 to 4);
- the `server.toml` keys and their defaults (`docs/config.md`);
- the database: newt applies its own migrations on start, and within 2.x they only add tables, columns and indexes. Upgrading is replacing the binary. Downgrading is not supported.

Minor releases may add things: optional fields, endpoints, live feed event types, config keys. Clients should ignore fields they do not know. Experimental features are outside the promise: a minor release may change them, and the changelog says so. The Rust crates are not published, and their APIs, the JSONL event file and the OTLP span attributes are not covered.

```
              events (POST)                        live feed (SSE)
  your app ----------------------+   +--------------------------> your app
                                 v   |
                         +---------------------+
                         |   newt-server       |
                         |   event API         |
                         |   runner: leases,   |
                         |   sweeps, turn loop |
                         +---------------------+
                           |     |       |     |
         model calls       |     |       |     |   state, journal, wakeups
        +------------------+     |       |     +----------------------+
        v                        v       v                            v
  Model providers      Home MCP server   Added MCP servers       Postgres
  (Anthropic, OpenAI)  (the agent:       (more tools and          (inbox, history,
                        prompt, tools,    pages)                   journal, leases)
                        pages)
```

Read on for a demo. The details live in [docs/design.md](docs/design.md) (why it works this way), [docs/protocol.md](docs/protocol.md) (the home server and MCP contract), [docs/config.md](docs/config.md) (every `server.toml` key), [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## Try it: a personal assistant

`newt-demo` is a small assistant that keeps a person's notes and tasks and replies by chat. It is one MCP server, and that server is the whole agent: its instructions, its model and settings in the `newt` extension, its tools, the person's notes and tasks as resources that announce their updates, a skill, and a `newt/turn` answer that tells the model the time. Its state lives in memory. Copy `crates/newt-demo` to start your own agent.

With nothing but Docker, and no API key (the loadtest's fake model answers):

```sh
docker compose -f deploy/compose.yaml up --build -d
curl -X POST localhost:7878/v1/agents/assistant/sessions/ana/events \
  -H "Authorization: Bearer demo-token" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow."}'
curl -N localhost:7878/v1/agents/assistant/sessions/ana/live -H "Authorization: Bearer demo-token"
```

The demo also shows the experimental flows: `clear_tasks` asks the person to approve before it deletes anything (MCP `input_required`), and `weekly_review` runs as an MCP task that newt polls until it is done. For a real model, set `OPENAI_API_KEY` and point `[models.default]` in `deploy/server.toml` at `openai`.

From source, with no database to set up (newt runs its own Postgres in `.newt/postgres`, downloading the binaries once):

```sh
export ASSISTANT_TOKEN=$(openssl rand -hex 16)

# terminal 1: the agent, an MCP server at localhost:4000/mcp
cargo run -p newt-demo

# terminal 2: newt, with the route keys server.example.toml names
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=...
NEWT_CONFIG=server.example.toml cargo run -p newt-server
```

Talk to it, watch the turn live, and read what it sent:

```sh
curl -X POST localhost:7878/v1/agents/assistant/sessions/ana/events \
  -H "Authorization: Bearer $ASSISTANT_TOKEN" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow. Also, I moved to Lisbon."}'

curl -N localhost:7878/v1/agents/assistant/sessions/ana/live -H "Authorization: Bearer $ASSISTANT_TOKEN"

curl localhost:4000/outbox/ana -H "Authorization: Bearer $ASSISTANT_TOKEN"
```

To skip terminal 1, let newt run the demo itself: build it (`cargo build -p newt-demo`) and set `mcp = "stdio:target/debug/newt-demo --stdio"` for the agent. Newt starts it, talks to it over stdin and stdout, and stops it on exit; the code is the same.

The event is accepted in a few milliseconds; the turn reads the workspace, adds the task and the note through MCP, sends a reply and ends. `newt-report newt-events.jsonl` breaks each turn down by model call. On a live run against a hosted model, each turn took 6 to 8 seconds, of which newt itself spent about 20 ms.

## Load testing without paying for tokens

`newt-loadtest` runs load against newt with no real model behind it:

- `fake-model` speaks the OpenAI Responses stream with random text. Its timings (response headers, thinking, token rate, prefill per uncached token) and its tool use come from the profile of a real assistant: about 16 tool calls per turn, picked with that assistant's tool mix. Prompt caching is simulated per prefix: a request reuses the longest run of 128-token blocks an earlier request with the same `prompt_cache_key` wrote (at least 1,024 tokens, readable 1 second after the write, five-minute expiry), so a rewritten history misses just as it would against a real model.
- `mock-app` is the agent's home MCP server: a production-sized prompt, workspace and skills, and tools with the latencies and error rates of that assistant's tools. `--stdio true` serves it over stdin and stdout, for newt to run itself.
- `run` drives sessions against newt's events API and live feed and reports read receipt, time to turn start, turn duration, model calls, cache rate and tool calls.

```sh
newt-loadtest fake-model --speed 1 &   # --speed 10 compresses every delay tenfold
newt-loadtest mock-app --speed 1 &
LOADTEST_TOKEN=... newt-loadtest run --base http://127.0.0.1:7878 --agent assistant \
  --sessions 200 --turns 5 --concurrency 200 --think-ms 5000 \
  --fake http://127.0.0.1:9100 --mock http://127.0.0.1:9200
```

Point a route's `base_url` at `http://127.0.0.1:9100/v1` and the agent's `mcp` at `http://127.0.0.1:9200/mcp`, or at `stdio:newt-loadtest mock-app --stdio true`. `scripts/bench.sh` does all of this in one command and adds newt's CPU, memory and cache report card. `scripts/bench-compare.sh newt|eve` runs the same workload against newt or eve, sped up, and reports turns at real speed; see [docs/benchmark.md](docs/benchmark.md).

## Running the tests

```sh
docker compose up -d --wait
export DATABASE_URL=postgres://newt:newt@localhost:55432/newt
cargo test --workspace
```

The tests in `newt-server`, `newt-demo` and `newt-loadtest` use Postgres: each `#[sqlx::test]` creates a throwaway database from `DATABASE_URL` and applies `migrations/`. The `newt-core` tests need no database. No test calls a real model provider. CI also runs `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check` and `cargo machete`; see [CONTRIBUTING.md](CONTRIBUTING.md).

## The home server

The full contract is in [docs/protocol.md](docs/protocol.md). An agent is one MCP server, its home, named in `server.toml` with its token:

```toml
[agents.assistant]
mcp = "https://agent.example.com/mcp"   # or http://localhost:4000/mcp, or stdio:<command>
token_env = "ASSISTANT_TOKEN"
```

The token authenticates both directions: callers send it to newt as `Authorization: Bearer`, and newt sends it to a home reached over HTTP. Everything else comes from the home:

| From the home | Becomes |
|---|---|
| `instructions` | the system prompt |
| `capabilities.extensions.newt` | the agent's settings: `{ model, first_call_effort?, effort?, max_output_tokens?, max_model_calls?, max_turn_tokens?, history_budget?, recite?, turn?, turn_end?, servers? }` |
| tools, skills | the agent's tools and skills |
| resources, read per session | the workspace pages; `notifications/resources/updated` marks a page stale |
| `newt/turn` (when `turn: true`) | per-turn `pinned`, `instructions`, `constraints`, `model`, efforts, `reset`, `state` and `turn_state` |
| `newt/turn_end` (when `turn_end: true`) | called after each committed turn, with the outcome and the reply, at least once |

`pinned` is text that stays the same from turn to turn for this session, such as the writing guide for the person's channel: newt places it after the instructions, in the cached prefix, so it costs nothing while it is unchanged and one cache miss when it changes. `instructions` and `constraints` reach the model at the start of that turn only and never enter history; put anything that must hold until it changes (a hold, a hard requirement) in `constraints`, and enforce it in your tool handlers too, since the model can still get it wrong. `model` must be a `[models]` alias from `server.toml`. Unknown fields or an unknown model fail the turn loudly.

The same server code runs hosted (`https://`), next to newt in the same deploy (`http://localhost`), or as a child process of newt (`stdio:`), just as newt's database can be someone else's, yours next to it, or newt's own.

## Skills

A skill is instructions for one kind of task, loaded only when the agent needs it. Servers serve them over MCP (the `io.modelcontextprotocol/skills` extension); newt adds an index of names and descriptions to the instructions (it stays in the cached prefix) and gives the agent a built-in `load_skill` tool that fetches a skill on demand and verifies every file against the listing. A loaded skill's text stays in that turn's scratch and never enters history.

## Replay and fork

`POST .../sessions/{key}/fork` starts a new session with a copy of another's history, up to a turn, under the same or another agent. `newt-replay assistant/ana assistant-canary/ana-replay` uses it to send a real conversation's events to a canary agent with a new prompt or model, turn by turn, and prints what each turn did then and now. Use the loadtest fake model to replay for free. See [docs/protocol.md](docs/protocol.md) 5.4 and 5.5.

## Tools over MCP

The home's tools are the agent's, and the home can add more servers in its settings: `servers: [{ name, url, auth_env? }]`, where `auth_env` names the environment variable whose value newt sends as the bearer. An added server's tools are named `<name>__<tool>` and its resources become pages under `<name>/`, so nothing clashes and paths never move. Newt connects to all of them at once over Streamable HTTP (MCP `2026-07-28`, falling back to `2025-11-25`) or stdio, keeps one connection per server for every session, and relists when a server announces its tools changed. A server that cannot be reached is left out of the turn, and the model is told so; the rest of the agent runs.

A server that declares the `newt` extension speaks newt's protocol: it receives the session and its state, and newt reads its `_meta` below. Any other server is plain MCP: newt sends it the call with its idempotency key and nothing of the session, its tools count as durable with no invalidation, and whatever it says about pages or state is ignored. Choosing a server is choosing to trust what it tells the model.

| Where | Field | Meaning |
|---|---|---|
| tool | `newt/retention` | `ephemeral` (result dropped at the end of the turn), `durable` (call kept, result cut to its summary) or `outward` (an effect outside your system, journaled before it runs) |
| tool | `newt/invalidates` | page key prefixes every successful call makes stale |
| tool | `newt/idempotent` | must be `true` on outward tools, or newt refuses to load them |
| call | `newt/session` | the session key |
| call | `newt/idempotency_key` | stable across retries of the same call; outward handlers must honour it |
| call | `newt/skills_loaded` | the skills the agent loaded this turn, so a handler can refuse an action until the right skill was read |
| call | `newt/pages_read` | `[{ path, version }]` of the workspace pages the agent read this turn |
| result | `newt/summary` | what durable history keeps instead of the full result |
| result | `newt/invalidate` | more page key prefixes to mark stale |
| result | `newt/pages` | fresh page data, used without a refetch |

Invalidation only reaches pages newt already has, and only the pages of the server whose tool made the call, so a tool that creates a page returns it in `newt/pages`. A result whose newt metadata is malformed marks all of that server's pages stale. When a call times out or loses its connection, the model is told the outcome is unknown and not to repeat it, and the connections are rebuilt on the next turn.

## Memory and compaction

The agent saves durable facts with the built-in `remember` tool (one fact per call, kept in history as an append). When a session's live history passes `history_budget` tokens (60,000 by default), newt compacts it right after a turn commits: the agent's own model gets the same prompt plus one instruction, saves anything not yet recorded, then calls `checkpoint` with its memory, open threads, commitments, decisions and tone. Newt refuses a checkpoint that drops a commitment from the previous one unless it is listed as resolved, and nothing is sent while compacting. Later turns see the checkpoint followed by the most recent whole turns that fit in a quarter of the budget, plus anything saved during the flush. An empty checkpoint, or one longer than a quarter of the budget, is refused. A compaction that fails waits before the next try (ten minutes, doubling, at most a day). Raw history is never deleted.

## Tracing

Every turn emits one typed event stream; sinks read it. The JSONL sink writes it to a file for local runs and sims, without message content unless `[events] capture_content = true` (`newt-report <file>` summarises timing and flags, and prints a cache report card that names what broke the cache on every miss: a tool whose description changed, history rewritten by compaction, an idle gap past the cache lifetime, or routing; `--prices input,cached,output` adds the cost). The OTLP sink exports one trace per turn over OTLP/HTTP JSON: an `invoke_agent` root, a `chat` span per model call with token usage, timings and (unless `capture_content = false`, which also hashes the session key and replaces error text with `failed`) the messages new to that call and its response, and an `execute_tool` span per tool call. A session's turns share `gen_ai.conversation.id`; the `langfuse` profile also sets the Langfuse session, observation types and usage details. Span ids derive from session, turn and call, so a re-export overwrites rather than duplicates. Library users can add an `Enricher` (extra attributes per session) and a `Redactor` (runs over every captured message before it leaves the process).

## License

Apache-2.0 or MIT, at your option. See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
