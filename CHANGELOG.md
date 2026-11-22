# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and newt follows [semantic versioning](https://semver.org/) from 1.0; what that covers is under Stability in the README.

## [Unreleased]

### Added

- **Cancel works from any instance.** `POST .../cancel` no longer needs to reach the instance running the turn. It looks up the session's running turn in Postgres and records the request on the session row (`cancel_turn`, `cancel_requested_at`; migration `0010`), notifies the `newt_cancel` channel, and answers 202 `{"turn": "<id>"}`; the instance that holds the lease cancels at once, or at its next lease heartbeat if the notification was lost, with no extra database read per model call. The body may name `{"turn": "<id>"}` so a late cancel can never hit the next turn. Repeating a request is 202 again and cancels once; an unknown session is 404 `session_not_found`; no running turn, or another one, is 409 `turn_not_running` (the old per-instance 404 `no_running_turn` is gone). The flag is cleared when the turn ends. See protocol 5.9.
- **`max_waiting` in `[runner]` (default `4 * max_sessions`).** Every wake used to spawn a task that waited for a slot, with no limit. Now at most `max_waiting` woken sessions wait; a wake past it is dropped, the session stays in the inbox for the next sweep, and `newt_wakes_dropped_total` counts it.
- **One error shape and a gap frame for clients.** Every 4xx and 5xx of the HTTP API, including the framework's own (unknown route, wrong method, body over 2 MiB), is now `{"error": {"code", "message"}}` with stable codes (listed in protocol section 5), and 429 and 503 carry `Retry-After`. The `error` field used to be a string. `GET .../live` resumed from a cursor older than the oldest kept frame now starts with a `gap` frame `{from, to}` instead of silently starting later. The SSE keep-alive is a comment line every 15 seconds, now documented.
- **`GET /metrics`** in Prometheus text format, with no new dependency: events accepted, duplicate, limited and conflicted; turns started, completed, failed and cancelled; fenced commits, dead-lettered events and retries; running and waiting turns, `max_sessions` and the oldest wait; and a histogram of how long woken sessions waited for a slot. See protocol 5.10.
- **`POST /v1/agents/{agent}/sessions/{key}/cancel`.** Cancels the session's running turn through its cancellation token. The turn emits `turn_failed` with the new `reason: "cancelled"` (also on the `TurnFailed` event in `newt-core`, `FailureReason::Cancelled`), and its events are dead-lettered with the reason `cancelled` when no outward call had run, so it is not retried. See protocol 5.9.
- **`reserved_interactive` in `[runner]`.** Turn slots (`max_sessions`) were one FIFO pool, so a burst of background turns could fill the process and make a live chat wait. Now `reserved_interactive` slots (default `max(1, max_sessions / 4)`, always less than `max_sessions`) are only for interactive turns; turns whose events are all `priority: "background"` share the rest. The signal is the event's existing `priority` field, so no new `lane` field was added.
- **Event id conflicts.** The inbox row now stores a sha256 of the event's content (`text` and `meta`, keys sorted; migration `0009`). A repeat id with the same content is still a `duplicate`; a repeat id with different content gets 409 and is not stored, instead of silently answering with the first event. Rows from before the migration have no hash and stay duplicates.
- **`max_pending_events` per agent (default 100).** The number of unconsumed events one session may hold. `POST .../events` past it returns 429 with `Retry-After`; a repeat of an event already held is still a duplicate, not a 429. The check and the insert are one statement under a per-session advisory lock (migration `0008`), so concurrent posts cannot go past the cap.
- **Load-aware `/healthz`.** The body is now JSON: `ready`, `running` (turns holding a slot), `waiting` (woken sessions waiting for one), `max_sessions` and `oldest_wait_ms`. The status is still 200 while the database check passes and 503 when it fails, now with the same body. See protocol 5.8.
- **The live feed carries what a chat UI needs.** Under `live_stream`, the `received` frame carries the user's text and a new `tool_output` frame carries each tool call's result (first 4,000 characters). The AI SDK `/chat` stream sends that result as `tool-output-available`, and `examples/eve-react` turns newt's live feed into eve's message stream, so a UI built on `eve/react` runs against newt; its check drives eve's own client and reducer.

- **`reply_ends_turn` in the home's `newt` settings.** A response with text and no tool calls ends the turn with that text as the reply, instead of costing one more model call just for `end_turn`. In the benchmark this took a call off every turn: 7.6 to 6.6 calls, turn p50 20.4 s to 18.7 s at real speed, 16% fewer input tokens.
- **`live_stream` per agent.** An agent with `live_stream = true` gets the model's output on its live feed as it is written: the answer text as `text_delta` frames (`session`, `turn`, `text`), and each tool call's arguments as `tool_input_delta` frames (`call_id`, `name`, `delta`), so an agent that answers through a tool such as `send_message` can be shown as it writes. Both are batched to about every 50 ms or a line or sentence end, per call, and always flushed before the model call ends and before the call's ready event. Reasoning is never streamed, and the frames stay out of history, the journal, the JSONL event file and OTLP. All three provider protocols pass text and arguments through (`StreamEvent::TextDelta`, `StreamEvent::ToolInputDelta`: OpenAI Responses `response.function_call_arguments.delta`, Anthropic `input_json_delta`, chat-completions `tool_calls[].function.arguments`).
- **`tool_input`, `model_call_started` and `model_call_finished` live frames** for agents with `live_stream`. `tool_input` (`call_id`, `name`, `input`) carries a call's complete, parsed arguments once the model has finished writing them, before the tool runs. The two model call frames carry only `call` and the model or stop reason, and mark the steps of a turn.
- **`POST /v1/agents/{agent}/sessions/{key}/chat`** for the Vercel AI SDK. It accepts the body `DefaultChatTransport` sends, posts the last user message as an event (the message `id` is the event id, so retries deduplicate), and streams that turn as a UI message stream (`x-vercel-ai-ui-message-stream: v1`, `data: [DONE]`), so `useChat` shows text and tool-call input as they are written. Needs `live_stream`; a message sent mid-turn joins the running turn. See protocol 5.7 and `examples/ai-sdk-chat`.
- **`first_sign_ms` in `newt-loadtest`.** Each turn's time to first sign of life is logged and shown in `decompose` next to turn start and end: the earliest of a `tool_started`, `text_delta` or `tool_input_delta` frame on newt, and of `actions.requested`, `action.input.appended` or `message.appended` on eve. With a replay, `decompose` compares it to production's earliest tool-call start or first model token. The fake model now streams a call's arguments in 6 to 10 chunks spread over the time leading to the call, in both schedule modes.

### Fixed

- **A panic in a turn no longer stalls the session.** The runner now releases the session's lease immediately, fenced by owner and epoch, and schedules a retry with the usual backoff, instead of leaving the lease to expire (about 30 s). The interrupted turn resumes from its journal on the next attempt.

## [2.0.0] - 2026-11-21

This is a breaking release. The HTTP context provider is gone, and an agent is now its home MCP server plus any servers the home adds.

### Changed

- **An agent is its home MCP server.** `[agents.<name>]` takes `mcp` (`https://…`, `http://…` or `stdio:<command>`), `token_env` and `live_meta`; `context_provider` and `model` are gone. The home's `instructions` are the system prompt, and the agent's settings move from `GET /bootstrap` into the `newt` extension the home declares on `server/discover`: the same fields and defaults (`model`, efforts, `max_output_tokens`, `max_model_calls`, `max_turn_tokens`, `recite`, `history_budget`, `turn_end`), without `version`, `skills` or `mcp_servers`. A change to the declaration takes effect on the next turn after newt reconnects.
- **`POST /turn` becomes the `newt/turn` request**, sent to the home before each turn when it declares `turn: true`. Same params (`session`, `events`, `state`) and same result fields (`pinned`, `instructions`, `constraints`, `turn_state`, `model`, efforts, `reset`, `state`), without `version` and `pages`.
- **`POST /turn_end` becomes the `newt/turn_end` request**, with the same payload and the same at-least-once delivery; an error result counts as a failed delivery.
- **Pages come only from resources.** The home's resources are the root of the workspace, read in the background at the start of each turn as in 1.x MCP-only agents; `POST /pages` and pages in `/turn` are gone. A `resources/read` to a server that speaks newt carries `newt/session` and `newt/state`, and its result may carry `newt/state_patch`.
- **Skills come only from MCP** (the skills extension). Inline bootstrap skills are gone. The home's skills are named `<agent>/<path>`.
- **newt's protocol is the `newt` MCP extension.** Only servers that declare it, and the home, receive `newt/session`, `newt/state`, `newt/turn_state`, `newt/pages_read` and `newt/skills_loaded`, and have their tool and result `newt/*` fields applied. Any other server gets the call with `newt/idempotency_key` and `traceparent` only; its tools are `durable` and its `newt/*` fields are ignored.
- `auth: "agent_token"` and static `headers` on servers are gone. The agent's token goes only to a home reached over HTTP; an added server's token comes from `auth_env`.

### Added

- **Servers the home adds.** `servers: [{ name, url, auth_env? }]` in the home's settings. An added server's tools are named `<name>__<tool>` and its resources become pages under `<name>/`, with page keys scoped to that server, so paths and tool names never depend on how many servers there are. All servers connect at once, each with its own 20 second limit; one that cannot be reached is left out, named to the model in the turn context, and tried again every 30 seconds while the rest of the agent runs.
- **The stdio transport.** `stdio:<command>` for the home or any added server: newt starts the command, speaks MCP over its stdin and stdout, sends its stderr to newt's, stops it when the connection is dropped and starts it again when it exits. An agent and newt can ship as one container with no network between them; the same server code runs over HTTP.
- `newt-demo --stdio` serves the demo agent over stdio, and the demo answers `newt/turn` with the time.
- `newt-loadtest mock-app` is a home MCP server too, over HTTP or with `--stdio true`, and `/stats` counts resource lists and reads.
- **Docs:** a Why is newt page, and guides for the home server and for adding servers, replace the context-provider and MCP-only pages.

## [1.7.0] - 2026-11-16

### Added

- **`headers_env` per route.** `[routes.<name>]` may name an environment variable holding extra request headers as `name=value,name=value`, the same format `[tracing]` uses. They are sent on every model call next to the route's key, for a gateway that also wants something like an internal token. A header that would replace the key's own header is refused at startup; values are marked sensitive and never appear in logs or `Debug` output. `ProviderClient::with_headers` in `newt-core` carries them.

## [1.6.1] - 2026-11-16

### Added

- **`live_meta` per agent.** With `live_meta = true` under `[agents.<name>]`, the live feed's `received` frame carries the event's `meta`, so an app that follows the feed can render who sent what as soon as the event is accepted instead of waiting for the turn to commit. Off by default; frames are unchanged without it.

## [1.6.0] - 2026-11-15

### Added

- **A model and efforts per turn.** A `/turn` response may carry `model` (a `[models]` alias, resolved through the routes like the bootstrap's), `effort` and `first_call_effort`. They replace the bootstrap's values for every model call of that turn, and for the compaction right after it, and nothing else. An alias `server.toml` does not define fails the turn like any other bad response. A different model is a prompt cache miss, so it suits turns that are worth it.
- **Starting a session over from `/turn`.** `"reset": true` runs the turn as if the session had no history: the model sees no earlier entries and no earlier checkpoint summary, while the session keeps its key, inbox, provider state and live feed. Nothing is deleted: `GET .../history` and `fork` still see every entry. The reset is saved by the statement that commits the turn, so a turn that does not commit resets nothing, and compaction afterwards only considers what came after it. It needs no migration and no extra statement: the reset is a checkpoint row with an empty summary, written by the commit.
- A turn resumed after a crash keeps the model, efforts and reset it had chosen once a model call was journaled, whatever `/turn` answers the second time.
- **`model` and `reset` on the live feed's `turn_started` frame.**

## [1.5.0] - 2026-11-14

### Added

- **`reply` in the turn-end callback.** What the agent said when it finished (the text of the message that called `end_turn`, or of the text-only message that just preceded it), so a provider can post it to a chat or a thread. `TurnOutcome` in `newt-core` carries it as `reply`.

## [1.4.0] - 2026-11-14

### Added

- **The session state on every provider call.** A `/pages` request to an HTTP context provider carries `state`, and each MCP tool call carries `_meta["newt/state"]`: the session's provider state as the turn holds it, with patches applied so far. A page render or a tool can now read what the last turn saw without a store of its own.

## [1.3.0] - 2026-11-14

### Added

- **Session state patches during a turn.** A `/pages` response (`state_patch`) and an MCP tool result (`_meta["newt/state_patch"]`) can change the session's provider state with a JSON merge patch (RFC 7396). Patches touch only the keys they name, so a lazy page load and a tool call in the same turn do not overwrite each other. The state is saved at commit only if `/turn` or a patch changed it.
- **Initial turn state.** A `/turn` response may carry `turn_state`, the value the turn's tool calls start with, for facts only `/turn` knows (the channel to reply on, why the agent woke).

## [1.2.0] - 2026-11-14

### Added

- **Stateless context providers.** Newt now keeps the state a provider needs, so a provider can run as many instances as it likes with no session store of its own:
  - `POST {base}/turn` receives the turn's `events` (each event's `id` and the `meta` the app sent with it) and the session's `state`.
  - A turn response may return `state`. Newt saves it in the same transaction that commits the turn and sends it back on the next turn, so it never runs ahead of the session's history.
  - Turn state for tools: each MCP call carries `newt/turn_state` in `_meta`, and a result's `newt/turn_state` replaces it for the rest of the turn, error results included.
  - `turn_end: true` in the bootstrap makes newt call `POST {base}/turn_end` after each committed turn, and when it dead-letters events, with the outcome, every event consumed (steered ones included), state and turn state. The callback is written in the same transaction and delivered at least once, retried with backoff for about two hours.
- Migration `0007` adds `sessions.provider_state` and the `turn_ends` table. Both are additive.

## [1.1.0] - 2026-11-14

### Added

- **`replay_reasoning` route option.** `openai-responses` routes send earlier reasoning back by default. Set `replay_reasoning = false` for a gateway that may answer two calls of one turn from different backends (a pool of accounts, or overflow to another provider), which reject each other's encrypted reasoning with `invalid_encrypted_content`. Rejected on other protocols.

### Changed

- **Fewer round trips to Postgres.** A turn now reaches its first model call in four statements (accept, claim, read the session's work, start the turn with its history) and commits in one fenced statement that writes all of the turn's history at once. Releasing an idle session, the compaction check and the history read are one statement each. About two thirds fewer runner statements per turn, which matters most when the database is not next to newt.
- **No ping before every connection.** Only a pooled connection that sat idle for a minute or more is pinged before use, and one that fails is dropped.
- **Steering reads the inbox only when a notification arrived** since the last read, instead of before every model call. A failed read retries on the next call, and the listener wakes every watch after it reconnects, so a lost notification delays a steer by at most one turn and never drops it.

## [1.0.0] - 2026-11-13

The first release.

### Added

- **Runtime.** Turn loop with the frozen prefix, history and per-turn scratch layers, and retention classes (`ephemeral`, `durable`, `outward`) that decide what reaches history. Steering of a running turn by new events, a batch tool, and a crash-safe turn journal with exactly-once outward calls through idempotency keys. A Postgres runner with fenced leases, `NOTIFY` wakeups, a periodic sweep, retry backoff and dead-lettering. Events API with read receipts and a live SSE feed.
- **Providers.** Streaming clients for the Anthropic Messages, OpenAI Responses and OpenAI Chat Completions protocols (the last for vLLM, Ollama, OpenRouter and other compatible hosts), with per-route concurrency limits that reserve a slot for interactive turns, one retry of a retryable error before anything has streamed, fallback along a model alias's route chain, and a 10 minute deadline per model call. Prompt cache keys shared across an agent's sessions (`runner.cache_key_buckets`). By default (`"auto"`) newt measures each agent's model calls a minute and spreads its sessions over just enough keys to keep each under 12 a minute, with jump consistent hashing so a new key moves few sessions.
- **Workspace.** A reactive workspace of pages fed by a context provider: invalidation by key prefix, refetch on read, page data returned by tools, and lazy loading that runs the first model call while pages load in the background. Pinned turn context that joins the cached prefix.
- **Context provider.** Bootstrap per version, per-turn context (`instructions`, `constraints`, `pinned`) and page refetch over HTTP. Skills as a first-class feature, loaded on demand with `load_skill`, and visible to tools through `newt/skills_loaded`.
- **MCP.** Tools from MCP servers over Streamable HTTP, with the `newt/*` `_meta` fields for retention, invalidation, idempotency, summaries and page data. Speaks MCP `2026-07-28` with a fallback to `2025-11-25`. Tools sorted by name per server so the cached prefix stays stable. Relists tools after `notifications/tools/list_changed` over `subscriptions/listen`. A call stays alive while it reports progress (90 seconds quiet, 10 minutes total) and is cancelled when abandoned. Each call carries the `traceparent` of its tool span, and its progress notifications reach the live feed as `tool_progress`. Skills served over MCP (SEP-2640): listed on connect as `{server}/{skill path}`, fetched only when loaded, checked against the listing's digest, size and frontmatter, cached, with their supporting files readable through `load_skill`'s `file`. An agent can be one MCP server and nothing else (`[agents.x] mcp = ...`): its instructions are the server's, its workspace pages are the session's resources, and `notifications/resources/updated` over `subscriptions/listen` marks a page stale.
- **Approvals and tasks.** A tool that answers `input_required` with elicitations becomes a question to the person: the model asks, then calls the tool again with `_input` answers that newt sends as MCP input responses, with no state stored in between. Tools that run as MCP tasks are polled for up to an hour.
- **Turn guards.** `max_turn_tokens` in the bootstrap: past it the model is told to finish, and one more call that does not end the turn fails it. Recitation, experimental and off by default: with `recite` in the bootstrap, a `<goal>` note restates the turn's events on long turns, in scratch only. An eval (`examples/recitation_eval.rs`) found no gain on a strong model and 1 to 12% more input tokens.
- **Replay and fork.** `GET .../history` and `POST .../fork` (a new session, under the same or another agent, with a copy of the history up to a turn). `newt-replay` sends a session's events to a fork turn by turn and prints what each turn did then and now.
- **Compaction.** Checkpoints written by the agent's own model, with commitments that cannot be dropped silently, a kept tail of recent turns, and raw history left in place. The built-in `remember` tool for durable facts.
- **Observability.** One typed event stream feeding a JSONL file, the live feed and an OTLP/HTTP sink (one trace per turn, with a Langfuse profile, an enricher and a redactor). `newt-report` summarises timing and flags from the JSONL file, and prints a cache report card: cached share, cost at `--prices`, and for every miss the cause (first call, model change, the first changed piece of the prefix with its offset, history rewritten from message n, compaction, idle past `--ttl`, or routing). Prefix and history fingerprints carry a hash per tool and per message for it.
- **Load testing.** `newt-loadtest` with a fake model that speaks the OpenAI Responses stream and simulates prompt caching, a mock context provider and MCP server, and a driver that reports read receipt, turn latency, model calls, cache rate and tool calls.
- **Embedded Postgres.** `database = { embedded = "<dir>" }` runs newt's own Postgres (downloaded once, data in `<dir>`), so newt needs no database setup on one machine. On by default through the `embedded-postgres` feature.
- **Profiling.** Model call events carry their prompt cache key, and the cache report card names a session that moved to another key. Accepting an event, claiming a session and loading history report how long they waited for a database connection; history and compaction state each use one connection instead of two or three. The fake model writes a cache log (`--cache-log`) of where each request went and which of its blocks the machine held or could read.
- **Turn start and cache keys.** Starting a turn takes three connections from the pool instead of five: the next turn's events are read on one, and the turn's start is written and its history read on another. A session keeps its cache key while its prefix is still cached and the key is not crowded, so adding keys moves no one who is fine where they are.
- **Benchmark.** `scripts/bench.sh` runs the fake model, the mock app and newt and reports load, CPU, memory and the cache report card, with no API key, in a `steady` scenario paced like people and a `burst` worst case; method in `docs/benchmark.md`.
- **Demo.** `newt-demo`, a personal assistant that serves a context provider and MCP tools in one process. Its MCP server is also a complete agent on its own (instructions, resources with update notifications, a skill served over MCP), run as `assistant-mcp`. Its MCP server also dogfoods approvals (`clear_tasks` asks first) and tasks (`weekly_review`, polled until done). A Dockerfile with every binary and `deploy/compose.yaml`, which runs Postgres, the demo, the fake model and newt with no API key.
- **Retention.** `[retention]` prunes consumed events after `inbox_days` (30) and live feed frames after `live_days` (7), hourly and in batches. `DELETE /v1/agents/{agent}/sessions/{key}` deletes a session and everything stored for it, and answers 409 while a turn holds its lease. History is never deleted on its own.
- **Several instances.** A test runs two runners against one Postgres and checks that every event reaches history exactly once and every model call belongs to one committed turn.
- **Releases.** A `v*` tag builds `newt`, `newt-report` and `newt-replay` for Linux x86_64 and macOS arm64, pushes the image to `ghcr.io/julianbouchard/newt`, and publishes a GitHub release with this changelog's section as notes.
- **Workspace setup.** Cargo workspace with `newt-core`, `newt-server`, `newt-demo` and `newt-loadtest`; dual `Apache-2.0 OR MIT` licensing; CI running format, clippy, tests, `cargo deny`, `cargo machete` and a boot smoke test; the newt logo.
