# newt

An agent runtime for products: a long-lived agent per customer, your backend as its world, and a database's discipline about context.

Newts regrow what they lose. So does this: every turn strips the context back and grows it from the workspace and the history, so stale reads never pile up and the prompt cache stays warm.

Early days. In so far: streaming Anthropic and OpenAI clients, the turn loop with retention classes, steering, a crash-safe journal, the Postgres runtime, and the reactive workspace fed by a context provider.

## Run it

```sh
docker compose up -d --wait
export NEWT_DATABASE_URL=postgres://newt:newt@localhost:55432/newt
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=... ASSISTANT_TOKEN=...
NEWT_CONFIG=server.example.toml cargo run -p newt-server
curl localhost:7878/healthz
```

## Context provider

Each agent in `server.toml` names a `context_provider` base URL and a `token_env`. That one token authenticates both directions: callers send it to newt as `Authorization: Bearer`, and newt sends it to the provider. The provider serves three JSON endpoints:

| Call | When | Request | Response |
|---|---|---|---|
| `GET {base}/bootstrap` | first turn, then whenever the version changes | none | `{ version, instructions, model, first_call_effort?, effort?, max_output_tokens?, max_model_calls?, history_budget?, mcp_servers? }` |
| `POST {base}/turn` | start of every turn | `{ session }` | `{ version, pages: [Page], instructions?, constraints? }` |
| `POST {base}/pages` | a read of a page a tool invalidated | `{ session, keys: [PageKey] }` | `{ pages: [Page] }` |

A `Page` is `{ key: ["customer", "profile"], path: "profile.md", version, content }`. Tools invalidate pages by key prefix; the next read refetches them, and a page missing from the response is gone. `instructions` and `constraints` reach the model at the start of that turn only and never enter history; put anything that must hold until it changes (a hold, a dealbreaker) in `constraints`, and enforce it in your tool handlers too, since the model can still get it wrong. `model` must be a `[models]` alias from `server.toml`. Unknown fields, an unknown model or a version mismatch fail the turn loudly.

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
| result | `newt/summary` | what durable history keeps instead of the full result |
| result | `newt/invalidate` | more page key prefixes to mark stale |
| result | `newt/pages` | fresh page data, used without a refetch |

Invalidation only reaches pages newt already has, so a tool that creates a page returns it in `newt/pages`. A result whose newt metadata is malformed marks every page stale. When a call times out or loses its connection, the model is told the outcome is unknown and not to repeat it, and the connections are rebuilt on the next turn.

## Memory and compaction

The agent saves durable facts with the built-in `remember` tool (one fact per call, kept in history as an append). When a session's live history passes `history_budget` tokens (60,000 by default), newt compacts it right after a turn commits: the agent's own model gets the same prompt plus one instruction, saves anything not yet recorded, then calls `checkpoint` with its memory, open threads, commitments, decisions and tone. Newt refuses a checkpoint that drops a commitment from the previous one unless it is listed as resolved, and nothing is sent while compacting. Later turns see the checkpoint followed by the most recent whole turns that fit in a quarter of the budget. Raw history is never deleted.

## Tracing

Every turn emits one typed event stream; sinks read it. The JSONL sink writes it to a file for local runs and sims (`newt-report <file>` summarises timing, cache hits and flags). The OTLP sink exports one trace per turn over OTLP/HTTP JSON: an `invoke_agent` root, a `chat` span per model call with token usage, timings and (unless `capture_content = false`) the messages new to that call and its response, and an `execute_tool` span per tool call. A session's turns share `gen_ai.conversation.id`; the `langfuse` profile also sets the Langfuse session, observation types and usage details. Span ids derive from session, turn and call, so a re-export overwrites rather than duplicates. Library users can add an `Enricher` (extra attributes per session) and a `Redactor` (runs over every captured message before it leaves the process).

## License

Apache-2.0 or MIT, at your option.
