# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and newt follows [semantic versioning](https://semver.org/) from 1.0; what that covers is under Stability in the README.

## [Unreleased]

## [2.5.1] - 2026-11-27

Two fixes from an architecture review. The config, the event API and the database are unchanged.

### Fixed

- **A turn that crashes newt every time it runs is given up.** A crash mid-turn leaves the session's lease to expire, and the next instance picks the turn up again; that never counted as an attempt, so a turn that crashed newt each time was picked up forever. Taking over an expired lease now counts as a failed attempt (a commit resets the count), and the failure policy decides before such a turn runs again: by default, after five interrupted runs its events are dead-lettered with the reason `the turn was interrupted before it finished`, and no `newt/turn_end` is sent for it. A turn that already made an outward call is never given up this way; like every turn with effects, it runs again.
- **A failed journal write no longer leaves a hole in the journal.** After one write failed, later entries of the same turn were still written, so a crash before the turn ended could replay its steps out of line. Once a write fails, nothing more of that turn is journaled: a call that must be journaled before it runs fails the turn, and a resume replays only what was saved.

## [2.5.0] - 2026-11-27

The workspace reads a page only when the turn asks for it, newt-core keeps only the turn loop and the traits around it, and three hooks let an app change what the server does without forking it.

### Added

- **The turn opens with an index of the workspace's pages**: path, size and description, priority first, a large folder as one line with the glob that lists it. `page_index_wait_ms` in the agent's `newt` settings (default 2000) bounds how long the first model call waits for it; `0` leaves it out.
- **Pages are read ahead without counting as read**: those whose `annotations.priority` is at least `preload_priority` (default 1.0) as the turn starts, and those a tool result links to with `resource_link`.
- **MCP resource templates**: a template named like `orders/{id}.md` with a matching `uriTemplate` serves the pages its pattern matches, keyed by its `_meta["newt/key"]` with the parts filled in, or by the filled URI.
- **Hooks on `Runner::builder`**: `.observe(...)` adds an `Observer` (an `EventSink` that can also hear server events), `.history(...)` sets the `HistoryPolicy` that decides what of a turn becomes history (`Retained` by default, `KeepAllToolCalls` as an alternative), and `.failures(...)` sets the `FailurePolicy` that decides when a failed turn is dead-lettered (`GiveUpAfter { attempts: 5 }` by default). A turn with effects outside is always retried.

### Changed

- **The workspace lists pages at turn start and reads a page only when asked.** A read waits for the pages it names, never for the slowest one, and a page that fails fails only its own read. `grep` searches the pages that read and names the ones it skipped.
- **MCP requests reuse pooled connections** instead of opening one per request.
- **Provider clients move to their own crate, `newt-providers`.** For Rust users: `ProviderClient`, `Protocol` and `sse` are now under `newt_providers`; newt-core keeps `Model` and its types and no longer depends on an HTTP client. `ProviderError::Http` and `Timeout` box their error, and `Sse` carries its message.
- **`Runner::new` is replaced by `Runner::builder(store, backend, model)`**, and `RunnerConfig::max_attempts` by the failure policy. `TurnDeps` takes a `history` policy.
- **Built-in tools are one table, `BUILTINS`.** `BUILTIN_TOOL_NAMES` is gone; the names come from the table.

The `newt` binary's config, event log, journal and HTTP API are unchanged; `server.toml` gains no keys.

## [2.4.0] - 2026-11-26

newt-core now holds only the turn loop: tracing is its own crate and the server's own concerns live in the server. The `newt` binary, its config, the event log and the journal are unchanged. Two fixes for several instances sharing one Postgres.

### Fixed

- **A cancelled turn's events can no longer be answered by another instance.** A cancelled turn without effects released its session before it dead-lettered its events, so for a moment another instance could claim the session and run them again. The late dead-letter then marked those events consumed under the new owner's running turn and deleted its journal, so a crash in that turn could not be resumed. Dead-lettering now happens while the turn still holds the session, checks the lease, and releases the session in the same transaction.
- **Library users of the OTLP exporter: an unfinished turn is exported after 30 minutes even if no other event arrives.** Eviction ran only when an event came in; it now runs on its own one-minute timer. The server always had events arriving, so its traces are unchanged.

### Changed

- **OTLP export is its own crate, `newt-otlp`.** For Rust users of the library: `newt_core::otlp` is now `newt_otlp`, and `tool_traceparent` with the other trace and span ids is `newt_core::trace`. The `newt` binary, its config and the traces it exports are unchanged.
- **The server's own events leave `newt_core::event`.** `EventKind` keeps the events of a turn. `server_ready`, `event_accepted`, `session_woken`, `session_claimed`, `turn_prepared`, `turn_committed`, `compaction_completed` and `runner_sample` move to `newt_server::events`, and `JsonlSink` takes the line type it writes. The JSONL event log is unchanged, line for line.
- **`TurnChoice` moves to `newt_server::backend`.** In newt-core, `JournalEntry::Choice` is now `JournalEntry::Host`, an object the loop replays untouched. Journal rows keep their shape, so turns resume across versions.

## [2.3.0] - 2026-11-26

A write a crash interrupted is reported to the model instead of made again. In the agent-runtime bench (one session, four turns, 21 model calls and 73 tool calls), a turn waits on about four more database round trips: +195 ms per turn at 50 ms per round trip, nothing measurable at 0 ms; statements and CPU per turn are unchanged.

### Changed

- **A write a crash interrupted is no longer made again by default.** The `durable` calls of a model step run only once Postgres has acknowledged the model response's journal write (not its disk flush). On recovery, a `durable` call from a journaled response with no recorded output is not made again: the model gets an `Interrupted:` error result saying it may or may not have taken effect. Until now it was made again, and if the response had not reached the journal, newt asked the model again, which gave the calls new ids and new idempotency keys, so a handler could not tell the repeat apart. Tools that set `newt/idempotent: true` are still made again, now always with the same key. `ephemeral` calls don't wait and still run again. The journal format is unchanged; a turn journaled by an older version that crashed mid-write is reported `Interrupted:` too. Protocol section 4.1.
- **`newt/idempotent` now applies to `durable` tools too** (protocol section 2.2). It is still required on `outward` tools.

## [2.2.2] - 2026-11-22

### Security

- **`url` 2.5.9** for [RUSTSEC-2026-0592](https://rustsec.org/advisories/RUSTSEC-2026-0592) (stack overflow in `Url::origin()` on nested `blob:` URLs). newt's own code never calls it; the crate comes in through its HTTP dependencies.

## [2.2.1] - 2026-11-22

Fewer database round trips for the live feed. In the agent-runtime bench (one session, four turns, 21 model calls and 73 tool calls), statements per turn drop from 536 to 315 with `live_stream` and from 196 to 131 without; turn time and time to first answer text are unchanged.

### Changed

- **Live feeds on the instance that runs the turn take frames from memory.** The live writer hands each batch of frames, with their sequence numbers, to that session's open feeds. A feed reads `live_events` only to start or resume, to fill a gap, for frames another instance wrote, when its 5-second poll runs out, and after the listener reconnects. Reads of `live_events` per run went from 909 to 25.
- **A live feed that has caught up waits instead of reading again.** Every read that returned frames was followed by one that returned none.
- **A live notification carries the sequence number of its newest frame** (`agent/key/seq` on `newt_live`), so an instance's own notification coming back wakes no feed. A notification without the number still makes feeds read. During a rolling upgrade, instances older than 2.2.1 don't recognize the new payload: their feeds pick up frames written by newer instances on the 5-second poll until every instance runs 2.2.1.

## [2.2.0] - 2026-11-21

### Added

- **`builtin_tools` turns newt's built-in tools off.** In the agent core's `newt` settings: `true` (all, the default), `false` (none) or the names of the ones to keep. A built-in that is off is left out of the tool list and a call to it is an unknown tool, including inside `batch` and for calls started early. Without `load_skill` the skills index leaves the system prompt too. A compaction turn always gets `checkpoint`. `require_end_turn` without `end_turn`, or a name that is not a built-in, is refused at connect. Protocol section 1.2.

## [2.1.1] - 2026-11-21

Fixes from a real model in a deployed demo and from fuzzing every surface that parses model output, tool input or client requests. The fuzz properties stay in the test suite.

### Fixed

- **A NUL character no longer stops a turn.** Postgres refuses NUL in text and jsonb, so a model reply, tool argument or tool result containing one failed every commit of its turn: the turn retried, re-running its model calls, and its events were dead-lettered after five attempts. Model output and tool output now replace NUL with U+FFFD where they enter the turn. The event API, `/chat`, `/cancel` and `/fork` answer 400 `invalid_request` for a NUL in what a client sends, where they used to answer 500.
- **A `functions.` prefix on a tool name resolves to the tool.** OpenAI models address tools as `functions.<name>` internally, and the prefix can leak into the names they write, as it did inside `batch`. A prefixed name now runs the tool when only the bare name exists, for top-level calls, calls inside `batch` and calls started early.
- **`batch` parses an inner call's `arguments` like a top-level call's.** A JSON-encoded string becomes the object, `null` becomes `{}`, and anything else is reported to the model as invalid arguments instead of reaching the tool as a string.
- **A glob can no longer crash newt.** A `glob` pattern (in `glob` or in `grep`'s `glob`) of about a thousand nested braces overflowed the stack and aborted the process, and one of a few hundred thousand characters panicked. Patterns longer than 512 bytes are refused with an error result.
- **Chat Completions hosts:**
  - a stream that ends with `[DONE]` and no `finish_reason` now marks its last tool call ready;
  - `"tool_calls": null` in a chunk is accepted;
  - tool call chunks without an `index` no longer merge parallel calls into one;
  - a tool call that never got an id or a name fails the response instead of reaching the turn.
- **`end_turn`** refuses arguments that are not an object.
- **`/chat`:** a tool frame without a `call_id` no longer produces a chunk with an empty `toolCallId`, and a tool call gets at most one `tool-input-available`, never after its output.
- **Event `meta` numbers keep their exact value.** Floats were parsed lossily, so a large float could come back slightly different.
- **`[database] max_connections = 0`** is refused with a clear error instead of panicking at startup.


## [2.1.0] - 2026-11-21

Chat streaming for the AI SDK and eve's React client, a new rule for ending turns, and the protection a production event API needs. 2.1 changes a few 2.0 defaults and responses, listed first; from 2.1 on, 2.x keeps the compatibility promise in the README.

### Changed from 2.0

- **A reply ends the turn.** A response with text and no tool calls ends the turn with that text as the reply, as in most agent loops, instead of a reminder and one more model call just for `end_turn`. `end_turn`'s `recorded` and `nothing_new` are optional. `require_end_turn: true` in the agent core's settings keeps the 2.0 rule. Protocol section 6.
- **One error shape.** Every 4xx and 5xx of the HTTP API, including the framework's own (unknown route, wrong method, body over 2 MiB), is `{"error": {"code", "message"}}` with stable codes (protocol section 5); `error` used to be a string. 429 and 503 carry `Retry-After`.
- **A reused event id with different content gets 409.** The inbox row stores a sha256 of the event's `text` and `meta` (keys sorted; migration `0009`). The same id with the same content is still a `duplicate`; 2.0 silently answered a different one with the first event. Rows from before the migration have no hash and stay duplicates.
- **At most 100 unconsumed events per session** (`max_pending_events`, per agent). `POST .../events` past it returns 429 with `Retry-After`; a repeat of an event already held is still a duplicate. The check and the insert are one statement under a per-session advisory lock (migration `0008`).
- **Slots held back for live chats.** `reserved_interactive` in `[runner]` (default `max(1, max_sessions / 4)`, always less than `max_sessions`) keeps that many turn slots for turns with an interactive event, so a burst of `priority: "background"` turns can no longer make a live chat wait.
- **`/healthz` answers JSON:** `ready`, `running`, `waiting`, `max_sessions` and `oldest_wait_ms`. Still 200 while the database check passes and 503 when it fails, now with the same body. Protocol 5.8.

### Added

Chat and streaming:

- **`live_stream` per agent.** The model's output on the live feed as it is written:
  - `text_delta` frames carry the answer text.
  - `tool_input_delta` frames carry each tool call's arguments, so an agent that answers through a tool such as `send_message` can be shown as it writes.
  - `tool_input` carries a call's complete, parsed arguments once the model has finished writing them, before the tool runs.
  - `tool_output` carries each result, cut to its first 4,000 characters.
  - The `received` frame carries the user's text.
  - `model_call_started` and `model_call_finished` mark the steps of a turn.

  Deltas are batched to about every 50 ms or a line or sentence end, and flushed before the model call ends. Reasoning is never streamed. None of these frames reach history, the journal, the JSONL event file or OTLP. All three provider protocols pass text and arguments through.
- **`POST .../chat` for the Vercel AI SDK.** It takes the body `DefaultChatTransport` sends, posts the last user message as an event (the message `id` is the event id, so retries deduplicate), and streams that turn as a UI message stream, so `useChat` shows text, tool input and tool results as they arrive. A failed turn shows a generic error, never the provider's text. It needs `live_stream`. Protocol 5.7 and `examples/ai-sdk-chat`; that example's check drives the real `ai` client.
- **`examples/eve-react`** turns newt's live feed into eve's message stream and serves the session endpoints `eve/react` calls, so a UI built for eve runs against newt: streamed text, tool input and results, and resume by index with no gaps or duplicates. Its check drives eve's own client and reducer.

Ending turns:

- **Any tool can end the turn.** A tool whose `_meta` says `newt/ends_turn: true` (eve's `endsTurn`) ends the turn once every call of its response has succeeded. A custom end tool like `no_reply` or `submit` can sit in the same response as the last action, which saves the closing model call. A result's `newt/ends_turn` overrides the tool's value for that one call. Protocol section 6.

Operations:

- **Cancel a running turn from any instance.** `POST .../cancel`, optionally naming `{"turn": "<id>"}` so a late cancel never hits the next turn.
  - The request is recorded on the session row (migration `0010`) and announced on `newt_cancel`.
  - The instance holding the lease cancels at once, or at its next lease heartbeat if the notification was lost.
  - Answers: 202 `{"turn"}` (also on a repeat); 409 `turn_not_running`; 404 `not_found`.
  - The turn ends with `turn_failed` and `reason: "cancelled"`. Its events are dead-lettered when no outward call had run, so it is not retried. Protocol 5.9.
- **`GET /metrics`** in Prometheus text format: events by outcome, turns by outcome, fenced commits, dead letters, retries, running and waiting turns, and a histogram of the wait for a slot. Protocol 5.10.
- **`max_waiting` in `[runner]`** (default `4 * max_sessions`) bounds the woken sessions that wait for a slot. A wake past it is dropped and left for the next sweep, and `newt_wakes_dropped_total` counts it.
- **A `gap` frame.** `GET .../live` resumed from a cursor older than the oldest kept frame starts with `{"type": "gap", "from", "to"}` instead of silently starting later. The SSE keep-alive, a comment line every 15 seconds, is now documented.

Speed:

- **Read-only tool calls start while the model is still writing.** A workspace read, or a call to an `ephemeral` tool, starts as soon as its arguments have finished streaming instead of after the whole response. Writes and sends still wait for the complete response, so a crash can never repeat them. Works on all three provider protocols.

Benchmarking (`newt-loadtest`):

- **A production-faithful bench.** `scripts/bench-compare.sh newt|eve` runs the same workload against newt or against eve 0.83 built for production (`eve build` + `eve start`, the Postgres workflow world, a Docker sandbox).
  - It runs sped up. The fake model and mock app log when they were busy on each turn's behalf, and `newt-loadtest decompose` splits every turn into the runtime's own time and the simulated wait, then scales that wait back to real speed.
  - `--replay` replays recorded production turns call by call: each call's duration, each tool call released at its recorded moment, each host tool taking its recorded latency. It then compares every turn with its production time.
  - Each run gets its own ports and databases.
  - `first_sign_ms` measures time to first sign of life.

### Fixed

- **A panic in a turn no longer stalls its session.** The runner releases the lease at once, fenced by owner and epoch, and retries with the usual backoff, instead of leaving the lease to expire (about 30 s).

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
