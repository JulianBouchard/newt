# newt

An agent runtime for products: a long-lived agent per customer, your backend as its world, and a database's discipline about context.

Newts regrow what they lose. So does this: every turn strips the context back and grows it from the workspace and the history, so stale reads never pile up and the prompt cache stays warm.

Early days, not yet in production. In so far: streaming Anthropic and OpenAI clients, the turn loop with retention classes, steering, a crash-safe journal, the Postgres runtime, a reactive workspace fed by a context provider, MCP tools, compaction and OTLP tracing.

## Try it: a personal assistant

`newt-demo` is a small assistant that keeps a person's notes and tasks and replies by chat. One process serves both halves an app gives newt: the context provider (`/assistant`) and the tools, over MCP (`/mcp`). Its state lives in memory.

```sh
docker compose up -d --wait
export NEWT_DATABASE_URL=postgres://newt:newt@localhost:55432/newt
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

- `fake-model` speaks the OpenAI Responses stream with random text. Its timings (response headers, thinking, token rate, prefill per uncached token) and its tool use come from a production agent's production profile: about 16 tool calls per turn, picked with a production agent's real tool mix. Prompt caching is simulated per prefix: a request reuses the longest run of 128-token blocks an earlier request with the same `prompt_cache_key` wrote (at least 1,024 tokens, readable 2 seconds after the write, five-minute expiry), so a rewritten history misses just as it would against a real model.
- `mock-app` is a context provider with a production-sized prompt and workspace, and an MCP server whose tools have a production agent's latencies and error rates.
- `run` drives sessions against newt's events API and live feed and reports read receipt, time to turn start, turn duration, model calls, cache rate and tool calls.

```sh
newt-loadtest fake-model --speed 1 &   # --speed 10 compresses every delay tenfold
newt-loadtest mock-app --speed 1 &
LOADTEST_TOKEN=... newt-loadtest run --base http://127.0.0.1:7878 --agent assistant \
  --sessions 200 --turns 5 --concurrency 200 --think-ms 5000 \
  --fake http://127.0.0.1:9100 --mock http://127.0.0.1:9200
```

Point a route's `base_url` at `http://127.0.0.1:9100/v1` and the agent's `context_provider` at `http://127.0.0.1:9200/agent`.

## Context provider

Each agent in `server.toml` names a `context_provider` base URL and a `token_env`. That one token authenticates both directions: callers send it to newt as `Authorization: Bearer`, and newt sends it to the provider. The provider serves three JSON endpoints:

| Call | When | Request | Response |
|---|---|---|---|
| `GET {base}/bootstrap` | first turn, then whenever the version changes | none | `{ version, instructions, model, first_call_effort?, effort?, max_output_tokens?, max_model_calls?, history_budget?, skills?, mcp_servers? }` |
| `POST {base}/turn` | start of every turn | `{ session }` | `{ version, pages: [Page], instructions?, constraints? }` |
| `POST {base}/pages` | a read of a page a tool invalidated | `{ session, keys: [PageKey] }` | `{ pages: [Page] }` |

A `Page` is `{ key: ["customer", "profile"], path: "profile.md", version, content }`. Tools invalidate pages by key prefix; the next read refetches them, and a page missing from the response is gone. `instructions` and `constraints` reach the model at the start of that turn only and never enter history; put anything that must hold until it changes (a hold, a dealbreaker) in `constraints`, and enforce it in your tool handlers too, since the model can still get it wrong. `model` must be a `[models]` alias from `server.toml`. Unknown fields, an unknown model or a version mismatch fail the turn loudly.

## Skills

A skill is instructions for one kind of task, loaded only when the agent needs it. The bootstrap lists them as `skills: [{ name, description, content }]`; newt adds an index of names and descriptions to the instructions (it changes only with the bootstrap version, so it stays in the cached prefix) and gives the agent a built-in `load_skill` tool. A loaded skill's text stays in that turn's scratch and never enters history.

## Tools over MCP

Tools come from the MCP servers the bootstrap lists: `{ name, url, headers?, auth? }`, where `auth: "agent_token"` sends the agent's token as the bearer and the default sends none. Headers are static and must not hold secrets. Newt connects once per agent and bootstrap version over Streamable HTTP, lists the tools, and keeps those connections for every session of that version.

Newt reads and writes a few `_meta` fields; a server that ignores them still works, and its tools count as durable with no invalidation.

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

Every turn emits one typed event stream; sinks read it. The JSONL sink writes it to a file for local runs and sims, without message content unless `[events] capture_content = true` (`newt-report <file>` summarises timing, cache hits and flags). The OTLP sink exports one trace per turn over OTLP/HTTP JSON: an `invoke_agent` root, a `chat` span per model call with token usage, timings and (unless `capture_content = false`, which also hashes the session key and replaces error text with `failed`) the messages new to that call and its response, and an `execute_tool` span per tool call. A session's turns share `gen_ai.conversation.id`; the `langfuse` profile also sets the Langfuse session, observation types and usage details. Span ids derive from session, turn and call, so a re-export overwrites rather than duplicates. Library users can add an `Enricher` (extra attributes per session) and a `Redactor` (runs over every captured message before it leaves the process).

## License

Apache-2.0 or MIT, at your option.
