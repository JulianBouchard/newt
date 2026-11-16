<h1 align="center"><img src="assets/logo/newt-lockup.svg" alt="newt" width="260"></h1>

An agent runtime for products: a long-lived agent per customer, your backend as its world, and a database's discipline about context.

Newts regrow what they lose. So does this: every turn strips the context back and grows it from the workspace and the history, so stale reads never pile up and the prompt cache stays warm.

In it: streaming clients for Anthropic, OpenAI and any Chat Completions host (vLLM, Ollama, OpenRouter), the turn loop with retention classes, steering, a crash-safe journal, the Postgres runtime, a reactive workspace fed by a context provider, MCP tools, approvals and tasks over MCP, compaction and OTLP tracing.

**Status:** stable since 1.0. Each release ships `newt` binaries for Linux (x86_64) and macOS (arm64) and a Docker image at `ghcr.io/julianbouchard/newt`. To build from source you need Rust 1.99. newt runs its own Postgres 18 or connects to yours.

| Maturity | What |
|---|---|
| Stable | The turn loop and retention classes, steering, the crash-safe journal, fenced leases and several instances on one Postgres, the events API and live feed, deleting sessions and pruning old rows, the context provider, MCP tools, approvals through MCP `input_required`, MCP tasks, agents made of one MCP server, compaction, prompt cache keys, OTLP tracing, embedded Postgres |
| Experimental | Skills over MCP (SEP-2640, a draft spec); replay and fork; the `openai-chat` protocol; the turn token budget; recitation (`recite` in the bootstrap, off by default) |

Experimental features work and are tested, including end to end against `newt-demo`, but the specs they follow or the shape of their settings may still change.

### Stability

From 1.0 newt follows semantic versioning. Within 1.x, nothing below changes in a way that breaks a working setup:

- the event API (`docs/protocol.md`, section 5): paths, fields, status codes;
- the context-provider protocol and the `newt/*` MCP `_meta` fields (sections 1 to 4);
- the `server.toml` keys and their defaults (`docs/config.md`);
- the database: newt applies its own migrations on start, and within 1.x they only add tables, columns and indexes. Upgrading is replacing the binary. Downgrading is not supported.

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
  Model providers      Context provider  MCP servers             Postgres
  (Anthropic, OpenAI)  (bootstrap, turn, (tools with              (inbox, history,
                        pages)            newt _meta)              journal, leases)
```

Read on for a demo. The details live in [docs/design.md](docs/design.md) (why it works this way), [docs/protocol.md](docs/protocol.md) (the context-provider and MCP contract), [docs/config.md](docs/config.md) (every `server.toml` key), [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## Try it: a personal assistant

`newt-demo` is a small assistant that keeps a person's notes and tasks and replies by chat. One process serves both halves an app gives newt: the context provider (`/assistant`) and the tools, over MCP (`/mcp`). Its MCP server is also a complete agent on its own: instructions, tools, the person's notes and tasks as resources that announce their updates, and a skill served over MCP. Its state lives in memory. Copy `crates/newt-demo` to start your own app.

With nothing but Docker, and no API key (the loadtest's fake model answers):

```sh
docker compose -f deploy/compose.yaml up --build -d
curl -X POST localhost:7878/v1/agents/assistant-mcp/sessions/ana/events \
  -H "Authorization: Bearer demo-token" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow."}'
curl -N localhost:7878/v1/agents/assistant-mcp/sessions/ana/live -H "Authorization: Bearer demo-token"
```

`assistant` is the demo through its context provider, `assistant-mcp` the same demo as nothing but an MCP server. Its MCP server also shows the experimental flows: `clear_tasks` asks the person to approve before it deletes anything (MCP `input_required`), and `weekly_review` runs as an MCP task that newt polls until it is done. For a real model, set `OPENAI_API_KEY` and point `[models.default]` in `deploy/server.toml` at `openai`.

From source, with no database to set up (newt runs its own Postgres in `.newt/postgres`, downloading the binaries once):

```sh
export ASSISTANT_TOKEN=$(openssl rand -hex 16)

# terminal 1: the app side
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

curl localhost:4000/assistant/outbox/ana -H "Authorization: Bearer $ASSISTANT_TOKEN"
```

The event is accepted in a few milliseconds; the turn reads the workspace, adds the task and the note through MCP, sends a reply and ends. `newt-report newt-events.jsonl` breaks each turn down by model call. On a live run against gpt-6.1-terra, each turn took 6 to 8 seconds, of which newt itself spent about 20 ms.

## Load testing without paying for tokens

`newt-loadtest` runs load against newt with no real model behind it:

- `fake-model` speaks the OpenAI Responses stream with random text. Its timings (response headers, thinking, token rate, prefill per uncached token) and its tool use come from the profile of a production assistant: about 16 tool calls per turn, picked with that assistant's real tool mix. Prompt caching is simulated per prefix: a request reuses the longest run of 128-token blocks an earlier request with the same `prompt_cache_key` wrote (at least 1,024 tokens, readable 2 seconds after the write, five-minute expiry), so a rewritten history misses just as it would against a real model.
- `mock-app` is a context provider with a production-sized prompt and workspace, and an MCP server whose tools have the latencies and error rates of that assistant's tools.
- `run` drives sessions against newt's events API and live feed and reports read receipt, time to turn start, turn duration, model calls, cache rate and tool calls.

```sh
newt-loadtest fake-model --speed 1 &   # --speed 10 compresses every delay tenfold
newt-loadtest mock-app --speed 1 &
LOADTEST_TOKEN=... newt-loadtest run --base http://127.0.0.1:7878 --agent assistant \
  --sessions 200 --turns 5 --concurrency 200 --think-ms 5000 \
  --fake http://127.0.0.1:9100 --mock http://127.0.0.1:9200
```

Point a route's `base_url` at `http://127.0.0.1:9100/v1` and the agent's `context_provider` at `http://127.0.0.1:9200/agent`. `scripts/bench.sh` does all of this in one command and adds newt's CPU, memory and cache report card; see [docs/benchmark.md](docs/benchmark.md).

## Running the tests

```sh
docker compose up -d --wait
export DATABASE_URL=postgres://newt:newt@localhost:55432/newt
cargo test --workspace
```

The tests in `newt-server`, `newt-demo` and `newt-loadtest` use Postgres: each `#[sqlx::test]` creates a throwaway database from `DATABASE_URL` and applies `migrations/`. The `newt-core` tests need no database. No test calls a real model provider. CI also runs `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check` and `cargo machete`; see [CONTRIBUTING.md](CONTRIBUTING.md).

## Context provider

The full contract is in [docs/protocol.md](docs/protocol.md). Each agent in `server.toml` names a `context_provider` base URL and a `token_env`. That one token authenticates both directions: callers send it to newt as `Authorization: Bearer`, and newt sends it to the provider. The provider serves three JSON endpoints:

| Call | When | Request | Response |
|---|---|---|---|
| `GET {base}/bootstrap` | first turn, then whenever the version changes | none | `{ version, instructions, model, first_call_effort?, effort?, max_output_tokens?, max_model_calls?, history_budget?, skills?, mcp_servers? }` |
| `POST {base}/turn` | start of every turn | `{ session }` | `{ version, pages?: [Page], pinned?, instructions?, constraints? }` |
| `POST {base}/pages` | a read of a page a tool invalidated, or the first read when `turn` returned no pages | `{ session, keys: [PageKey] }` (an empty key means every page) | `{ pages: [Page] }` |

Leave `pages` out of the turn response when rendering them is slow: newt starts the first model call at once and loads every page in the background, and the first read, glob or grep waits for that load. A `Page` is `{ key: ["customer", "profile"], path: "profile.md", version, content }`. Tools invalidate pages by key prefix; the next read refetches them, and a page missing from the response is gone. `pinned` is text that stays the same from turn to turn for this session, such as the writing guide for the person's channel: newt places it after the bootstrap instructions, in the cached prefix, so it costs nothing while it is unchanged and one cache miss when it changes. `instructions` and `constraints` reach the model at the start of that turn only and never enter history; put anything that must hold until it changes (a hold, a hard requirement) in `constraints`, and enforce it in your tool handlers too, since the model can still get it wrong. `model` must be a `[models]` alias from `server.toml`. Unknown fields, an unknown model or a version mismatch fail the turn loudly.

## Skills

A skill is instructions for one kind of task, loaded only when the agent needs it. The bootstrap lists them as `skills: [{ name, description, content }]`; newt adds an index of names and descriptions to the instructions (it changes only with the bootstrap version, so it stays in the cached prefix) and gives the agent a built-in `load_skill` tool. A loaded skill's text stays in that turn's scratch and never enters history.

## Replay and fork

`POST .../sessions/{key}/fork` starts a new session with a copy of another's history, up to a turn, under the same or another agent. `newt-replay assistant/ana assistant-canary/ana-replay` uses it to send a real conversation's events to a canary agent with a new prompt or model, turn by turn, and prints what each turn did then and now. Use the loadtest fake model to replay for free. See [docs/protocol.md](docs/protocol.md) 5.4 and 5.5.

## Tools over MCP

Tools come from the MCP servers the bootstrap lists: `{ name, url, headers?, auth? }`, where `auth: "agent_token"` sends the agent's token as the bearer and the default sends none. Headers are static and must not hold secrets. Newt connects once per agent and bootstrap version over Streamable HTTP (MCP `2026-07-28`, falling back to `2025-11-25`), lists the tools, and keeps those connections for every session of that version, relisting when a server announces its tools changed. A server can also serve skills over MCP (the `io.modelcontextprotocol/skills` extension); newt lists them, loads them on demand and verifies every file against the listing.

An agent can also be nothing but an MCP server: `[agents.x] mcp = "https://app.example.com/mcp"` with a `model`. The server's instructions become the system prompt, its tools and skills are the agent's, and its resources, listed and read per session, are the workspace. A `notifications/resources/updated` marks a page stale. Build an MCP server and the agent comes with it ([docs/protocol.md](docs/protocol.md) 2.7).

The full contract is in [docs/protocol.md](docs/protocol.md). Newt reads and writes a few `_meta` fields; a server that ignores them still works, and its tools count as durable with no invalidation.

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

Invalidation only reaches pages newt already has, so a tool that creates a page returns it in `newt/pages`. A result whose newt metadata is malformed marks every page stale. When a call times out or loses its connection, the model is told the outcome is unknown and not to repeat it, and the connections are rebuilt on the next turn.

## Memory and compaction

The agent saves durable facts with the built-in `remember` tool (one fact per call, kept in history as an append). When a session's live history passes `history_budget` tokens (60,000 by default), newt compacts it right after a turn commits: the agent's own model gets the same prompt plus one instruction, saves anything not yet recorded, then calls `checkpoint` with its memory, open threads, commitments, decisions and tone. Newt refuses a checkpoint that drops a commitment from the previous one unless it is listed as resolved, and nothing is sent while compacting. Later turns see the checkpoint followed by the most recent whole turns that fit in a quarter of the budget, plus anything saved during the flush. An empty checkpoint, or one longer than a quarter of the budget, is refused. A compaction that fails waits before the next try (ten minutes, doubling, at most a day). Raw history is never deleted.

## Tracing

Every turn emits one typed event stream; sinks read it. The JSONL sink writes it to a file for local runs and sims, without message content unless `[events] capture_content = true` (`newt-report <file>` summarises timing and flags, and prints a cache report card that names what broke the cache on every miss: a tool whose description changed, history rewritten by compaction, an idle gap past the cache lifetime, or routing; `--prices input,cached,output` adds the cost). The OTLP sink exports one trace per turn over OTLP/HTTP JSON: an `invoke_agent` root, a `chat` span per model call with token usage, timings and (unless `capture_content = false`, which also hashes the session key and replaces error text with `failed`) the messages new to that call and its response, and an `execute_tool` span per tool call. A session's turns share `gen_ai.conversation.id`; the `langfuse` profile also sets the Langfuse session, observation types and usage details. Span ids derive from session, turn and call, so a re-export overwrites rather than duplicates. Library users can add an `Enricher` (extra attributes per session) and a `Redactor` (runs over every captured message before it leaves the process).

## License

Apache-2.0 or MIT, at your option. See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
