# Protocol

This document specifies the two interfaces an application implements for newt, and the small event API it uses to talk to newt:

1. the **context provider**, three JSON endpoints newt calls over HTTP, which an agent can skip by being only an MCP server (2.7);
2. **MCP tools**, served over Streamable HTTP, with newt's `_meta` fields;
3. the **event API**, which the application calls on newt.

It is written from `crates/newt-server/src/{context,http}.rs`, `crates/newt-server/src/mcp/`, `crates/newt-core/src/{workspace,tools,skills,checkpoint,batch,commit}.rs` and the reference implementation in `crates/newt-demo`. Anything not confirmed in code is marked "(unverified)".

Terms: an **agent** is a name in `[agents.<name>]` in `server.toml`. A **session** is one conversation of one agent, identified by a **session key**, a string of 1 to 200 bytes with no `/`. A **turn** is one run of the model loop over the events that arrived for a session.

## 1. Context provider

Each agent names a base URL (`context_provider`) and a token variable (`token_env`). Below, `{base}` is the base URL with any trailing `/` removed.

### 1.1 Conventions

- Every request carries `Authorization: Bearer <token>`, where the token is the value of the agent's `token_env`. This is the same token callers of the event API use.
- Bodies are JSON. Newt sends `POST` bodies with `content-type: application/json` and does not send a body on `GET`.
- Each request has a 10 second timeout. Newt does not retry inside a request.
- A non-2xx response fails the call. The error keeps the status and the first 300 characters of the body.
- Decoding is strict. The bootstrap, the turn response, the pages response, each skill and each MCP server entry reject unknown fields. A page object tolerates extra fields.
- The protocol has no version field of its own. `version` below is the version of your bootstrap.
- A failure in any call, or a rejected bootstrap, stops the turn from starting. The session's events stay pending, the lease is released with exponential backoff (2 s doubling, capped at 300 s) and the sweeper tries again. The error is logged as `context provider for `<agent>`: ...`.

### 1.2 Call order

At the start of every turn newt calls `POST {base}/turn`. The response names a `version`. If newt holds no bootstrap for that agent at that version, or its tool set went stale since (section 2.1), it then calls `GET {base}/bootstrap`, which must return the same version, and connects the MCP servers. Otherwise it reuses the loaded bootstrap and tool set. Newt keeps one loaded bootstrap per agent in memory.

`POST {base}/pages` is called during the turn, when the agent reads a page that a tool made stale, and once at the start when `turn` returned no `pages`.

`POST {base}/turn_end` is called after a turn commits, when the bootstrap sets `turn_end` (1.8).

Newt keeps the state a provider needs between these calls (1.4 `state`, 2.3 `newt/turn_state`) and sends it on every call (`/turn`, `/pages`, each tool call), so a provider can run as many stateless instances as it likes: any instance can answer any call. Pages served as MCP resources (2.7) do not receive the state.

### 1.3 `GET {base}/bootstrap`

Everything that is the same for every session at one version. Changing any field means a new `version`; the first turn after that pays one prompt cache miss, because the instructions, skills index and tool list form the cached prefix.

Response:

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `version` | string | yes | | Non-empty after trimming. Must equal the `version` in the turn response. |
| `instructions` | string | yes | | The system prompt. Non-empty after trimming. |
| `model` | string | yes | | A `[models]` alias from `server.toml`. Unknown alias is an error. |
| `first_call_effort` | `"low"`, `"medium"` or `"high"` | no | none | Reasoning effort for the first model call of a turn. When absent, newt sends no effort. |
| `effort` | `"low"`, `"medium"` or `"high"` | no | none | Effort for later calls in the turn. |
| `max_output_tokens` | integer | no | 4096 | Per model call. Must be at least 1. |
| `max_model_calls` | integer | no | 8 | Model calls per turn. The turn fails if it uses them all without ending. Must be at least 1. |
| `max_turn_tokens` | integer | no | none | Input plus output tokens one turn may spend across its model calls. Past it, the model is told to send what it has and call `end_turn`; if its next call does not end the turn, the turn fails. |
| `recite` | boolean | no | `false` | Recitation: on long turns, from the fifth model call and every fourth after, append a `<goal>` note quoting the turn's events. Lives in scratch only. Experimental. |
| `history_budget` | integer | no | 60000 | Tokens of live history before compaction. Must be at least 1000. |
| `skills` | array of skill | no | `[]` | See 1.7. |
| `mcp_servers` | array of server | no | `[]` | See 2.1. |
| `turn_end` | boolean | no | `false` | Call `POST {base}/turn_end` after each committed turn (1.8). |

```json
{
  "version": "2026-11-12.1",
  "instructions": "You are a personal assistant for one person, reached by chat. ...",
  "model": "default",
  "first_call_effort": "low",
  "effort": "medium",
  "max_output_tokens": 4096,
  "max_model_calls": 8,
  "history_budget": 60000,
  "skills": [
    {
      "name": "writing",
      "description": "How to word a message to the person.",
      "content": "Short sentences. Lead with the answer."
    }
  ],
  "mcp_servers": [
    { "name": "app", "url": "https://app.example.com/mcp", "auth": "agent_token" }
  ]
}
```

### 1.4 `POST {base}/turn`

Called at the start of every turn, before the first model call.

Request:

```json
{
  "session": "ana",
  "events": [{ "id": "evt-41", "meta": { "channel": "whatsapp" } }],
  "state": { "last_seen_task": 12 }
}
```

| Field | Type | Meaning |
|---|---|---|
| `session` | string | The session key from the event URL, without the agent name. |
| `events` | array of `{ id, meta }` | The events this turn answers, oldest first: each event's `id` and the `meta` you sent with it (section 5.1), or `null`. A turn resumed after a crash gets the events it started with. Events that arrive during the turn as steering are not listed. |
| `state` | any JSON, or `null` | The `state` this provider returned on the last committed turn of the session, or `null` before the first. |

Response:

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `version` | string | yes | | The bootstrap version this response was built for. |
| `pages` | array of page | no | absent | The session's workspace. Absent or `null` means load lazily (1.5). `[]` means a workspace with no pages. |
| `pinned` | string | no | none | Session-stable text. Trimmed; empty is ignored. |
| `instructions` | string | no | none | Guidance for this turn only. |
| `constraints` | array of string | no | `[]` | Rules that hold right now, for this turn only. Entries are trimmed; empty ones are dropped. |
| `turn_state` | any JSON | no | none | The turn state the turn's tool calls start with (2.3), for facts the tools need that only this call knows, such as the channel to reply on. |
| `model` | string | no | the bootstrap's | A `[models]` alias for this turn's model calls. Resolved through the `server.toml` routes like the bootstrap's `model`. An alias `server.toml` does not define fails the turn like any other bad response: nothing is sent to the model, and the turn is retried with backoff. |
| `effort` | `"low"`, `"medium"` or `"high"` | no | the bootstrap's | `effort` for this turn's later model calls. |
| `first_call_effort` | `"low"`, `"medium"` or `"high"` | no | the bootstrap's | `first_call_effort` for this turn's first model call. |
| `reset` | boolean | no | `false` | `true` runs the turn as if the session had no history (1.4.1). |
| `state` | any JSON | no | none | The session's new provider state. Newt saves it in the same transaction that commits the turn, and sends it back as `state` on the next turn. Absent or `null` keeps the previous state; return `{}` to reset it. A turn that does not commit (the lease moved, or it was cancelled) saves nothing, so the state never runs ahead of the session's history. |

```json
{
  "version": "2026-11-12.1",
  "pages": [
    { "key": ["tasks"], "path": "tasks.md", "version": "7", "content": "# Tasks\n- [ ] #1 Call the bank\n" },
    { "key": ["notes"], "path": "notes.md", "version": "7", "content": "# Notes\n- Lives in Lisbon\n" }
  ],
  "pinned": "Write for WhatsApp: no markdown, two short paragraphs at most.",
  "instructions": "It is 2026-11-12T09:30:00Z.",
  "constraints": ["Do not message the person before Monday: they are on holiday."]
}
```

Where each field goes:

- `pinned` is appended to the system prompt as `\n\n<pinned>\n{pinned}\n</pinned>`, before the skills index. It is part of the cached prefix: free while it stays the same, one cache miss when it changes.
- `instructions` and `constraints` are rendered into one message at the start of the turn's scratch, and never enter history:

  ```
  <turn_context>
  <instructions>
  It is 2026-11-12T09:30:00Z.
  </instructions>
  <active_constraints>
  - Do not message the person before Monday: they are on holiday.
  </active_constraints>
  </turn_context>
  ```

  If both are empty, no message is added.
- `constraints` are text the model reads. They do not block a tool call. Enforce any constraint that must hold in your tool handlers as well.

- `model`, `effort` and `first_call_effort` replace the bootstrap's values for every model call of this turn, and for the compaction that runs right after it, and for nothing else: the next turn starts from the bootstrap again unless its `/turn` response says otherwise. A different model is a prompt cache miss for that turn and for the next one on the usual model, so use it for turns that are worth it.
- `reset` is described in 1.4.1.

Typical uses of `state`: what the agent saw last turn, so this turn can say what changed; a cursor into the app's own records; a note that something was already announced.

Later in the turn, a page fetch (1.6 `state_patch`) and a tool result (2.4 `newt/state_patch`) can change the state with a JSON merge patch. Patches touch only the keys they name, so a page load and a tool call running at the same time do not overwrite each other. The state the turn ends with is saved at commit, and only if `/turn`'s `state` or a patch changed it. A patch is lost when:

- the turn does not commit (it failed with no effects and will be retried, or its lease moved);
- it arrives after the commit, from a call that outlived the turn;
- the turn was resumed after a crash: replayed tool results do not apply their patches again, and pages fetched before the crash are not refetched.

So keep in the state what you can afford to recompute, and keep the record of anything that must not happen twice in your own data.

The whole state travels with every `/pages` request and every tool call, to every MCP server of the agent. Keep it small (a few kilobytes; newt does not cap it), and keep out of it anything one of the agent's tool servers should not read.

#### 1.4.1 Starting a session over

`"reset": true` rolls the session to a fresh conversation while it keeps its key, inbox, provider state and live feed. Use it when the conversation has gone cold, or when the agent's prompt moved on and the old history no longer fits it.

- The turn's model calls see no earlier history entries and no earlier checkpoint summary. The events the turn answers are delivered as usual, and the turn's own messages become the start of the new conversation. The history layer of the prompt changes, so the turn is a prompt cache miss.
- Nothing is deleted. `GET .../history` (5.4) still returns every entry and `fork` (5.5) still copies them; the entries are only never loaded into the model's prompt again. Compaction after a reset considers only what came after it.
- The reset is saved in the statement that commits the turn, so it takes effect only if the turn commits. A turn that does not commit (its lease moved, it was cancelled, or it failed with no effects and is retried) resets nothing, and its retry calls `/turn` again.
- A turn resumed after a crash (or retried) keeps the `model`, `effort`, `first_call_effort` and `reset` it had once any of its model calls is journaled, whatever `/turn` answers the second time. If it chose any of them, newt journals the choice before the first call. If it chose none, the resumed turn runs on the bootstrap's settings with no reset, even when `/turn` now asks for one. Before the first model call is journaled, a retry starts afresh and takes the new response.
- A reset needs history to start over from: a commit that adds no history rows resets nothing.
- `state` works as without a reset: return the new state, or `{}` to clear it.

Both are in the live feed: `turn_started` carries `model` (the alias the turn uses) and `reset` (5.3).

### 1.5 Pages and the workspace

A **page** is one file in the agent's workspace. The agent reads them with the built-in `read_file`, `glob` and `grep` tools.

Page object:

| Field | Type | Meaning |
|---|---|---|
| `key` | array of string, non-empty | Invalidation key. Tools invalidate pages by key prefix (2.3). Several pages may share a key. |
| `path` | string, non-empty | What the agent sees. Unique within one response. Glob patterns match it with `/` as a literal separator (`*` does not cross `/`, `**` does). |
| `version` | string | Opaque. Newt reports it back to tools in `newt/pages_read`. |
| `content` | string | The page text. |

```json
{ "key": ["customer", "profile"], "path": "profile.md", "version": "12", "content": "# Profile\nRemote only.\n" }
```

A response with an empty `key`, an empty `path` or a repeated `path` is rejected as a whole.

**Eager loading.** `turn` returns `pages`. The workspace starts with them.

**Lazy loading.** `turn` leaves out `pages`. Newt starts the first model call at once and requests every page in the background with `POST {base}/pages` and `keys: [[]]`. The first `read_file`, `glob` or `grep` waits for that load. If it fails, those tools return the error for the rest of the turn. Use this when rendering pages is slow.

**Refetching.** A successful tool call can make pages stale (2.3). The next read of a stale page asks the provider for it again, and `grep` refreshes all stale pages first. After a tool call, stale pages the agent has already read in this turn are also refreshed in the background.

### 1.6 `POST {base}/pages`

Request:

```json
{ "session": "ana", "keys": [["tasks"], ["customer", "profile"]], "state": { "last_seen_task": 12 } }
```

`state` is the session's provider state as the turn holds it now: what `/turn` returned or kept, with any patches applied so far (1.4), or `null`.

`keys` is a list of page keys. For a refetch newt sends the exact key of each stale page. The empty key `[]` means every page. Return the pages whose key starts with any listed key; `[]` matches all of them.

Response:

```json
{ "pages": [ { "key": ["tasks"], "path": "tasks.md", "version": "8", "content": "# Tasks\n..." } ], "state_patch": { "rendered_at": "2026-11-12T09:30:02Z" } }
```

`state_patch` (optional) is a JSON merge patch (RFC 7396) applied to the session's provider state (1.4): objects merge key by key, `null` removes a key, anything else replaces. It must be a JSON object; anything else fails the fetch. Use it to record what a lazy page load saw, such as the hashes of the pages it rendered, so the next turn can tell what changed.

Rules newt applies to the response:

- A page it asked for but that is missing from the response is deleted from the workspace.
- A returned page replaces the current one only if the page was one of those requested and has not been invalidated again while the request was in flight. If it was, the response for it is discarded and it stays stale.
- A returned page whose `path` the workspace does not have yet is added.
- The response is checked like any other page list (see the page object above).

### 1.7 Skills

A skill is instructions for one kind of task, loaded when the agent needs them.

```json
{ "name": "writing", "description": "How to word a message to the person.", "content": "Short sentences." }
```

All three fields are required strings. Newt appends this block to the system prompt, after `pinned`:

```
<skills>
Load a skill with load_skill before doing what it covers.
- writing: How to word a message to the person.
</skills>
```

and adds a built-in `load_skill` tool whose `name` argument is one of the skill names. The skill's `content` comes back as the tool result. It stays in that turn's scratch and never enters history. Newt does not check that bootstrap skill names are unique. Loaded skill names are passed to MCP tools in `newt/skills_loaded`.

MCP servers can serve skills too (section 2.6). Those follow the bootstrap's skills in the index, sorted by name.

### 1.8 `POST {base}/turn_end`

Called after a turn commits, when the bootstrap sets `turn_end: true`. Use it for bookkeeping that should follow the agent's turn: mark what it handled, stop a typing indicator, log the run, bill usage.

```json
{
  "agent": "assistant",
  "session": "ana",
  "turn": "0198f2a4-6b1e-7c3d-9a10-5e7d2c8b4f01",
  "outcome": "completed",
  "events": [{ "id": "evt-41", "meta": { "channel": "whatsapp" } }],
  "state": { "last_seen_task": 13 },
  "turn_state": { "sent": true },
  "reply": "Done: the bank call is on your list for tomorrow at 9."
}
```

| Field | Meaning |
|---|---|
| `outcome` | `completed`; `failed` for a turn that failed after it had effects (an outward call or a durable tool) and was committed with what it did; `dead_lettered` when newt gave up on the events after `max_attempts` failed turns with no effects. A failed turn with no effects that will be retried does not call `turn_end`. |
| `events` | Every event the turn consumed, as `{ id, meta }`, oldest first: the ones it started on and any that steered it. For `dead_lettered`, the events given up on. |
| `state` | The session's provider state after the turn (unchanged for `dead_lettered`). |
| `turn_state` | The turn state when the turn ended (2.3). After a crash and resume, only what calls made since the resume set. |
| `reply` | What the agent said when it finished, for a provider that posts it somewhere (a chat, a thread): the text of the message that called `end_turn`, or, when that message had none, the text-only message just before it that made newt remind the agent to end. `null` when there is neither, and always for `failed` and `dead_lettered`. Earlier mid-turn text is never used. |

Newt writes the callback in the same transaction as the turn, so it survives a crash, and delivers it at least once: right after the commit, then retried with backoff (at least 30 seconds, up to 5 minutes apart) until the provider answers 2xx. A delivery holds the callback for 30 seconds, so two runners do not send it at once, but a retry after a lost response is possible: use `turn` to deduplicate. Deliveries across turns can arrive late and out of order. Any 2xx counts; the body is ignored. After 30 failed attempts (about two hours) newt gives up on the callback and logs an error. Deleting the session deletes its undelivered callbacks.

## 2. MCP tools

Tools come from the servers in the bootstrap's `mcp_servers`. A server that knows nothing about newt still works; its tools are `durable` and invalidate nothing.

### 2.1 Servers and connections

```json
{ "name": "app", "url": "https://app.example.com/mcp", "headers": { "x-tenant": "acme" }, "auth": "agent_token" }
```

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `name` | string | yes | | Used in logs and errors. |
| `url` | string | yes | | Must start with `http://` or `https://`. Streamable HTTP endpoint. |
| `headers` | object of string to string | no | `{}` | Static headers sent on every request. Do not put secrets here; use `auth` for the agent token. |
| `auth` | `"none"` or `"agent_token"` | no | `"none"` | `"agent_token"` sends `Authorization: Bearer <agent token>`. |

Newt connects to each server in order and lists all its tools. All servers together get 20 seconds. Newt speaks MCP `2026-07-28` (`server/discover`, no handshake) and falls back to the `2025-11-25` `initialize` handshake for a server that does not answer `server/discover`. Two servers offering the same tool name is an error. Connections are shared by every session of the same bootstrap version.

Newt sorts each server's tools by name, so the tool list in the cached prompt prefix is byte-identical whatever order a server lists them in. Servers keep the order the bootstrap lists them in.

If a server accepts `subscriptions/listen` for `toolsListChanged`, newt keeps that subscription open. A `notifications/tools/list_changed` marks the tool set stale, and so does the subscription ending. A stale tool set is also what a closed or unsendable transport causes during a call. Either way, the next turn fetches the bootstrap again, reconnects and lists the tools again. A server that does not offer the subscription is only relisted on a new bootstrap version or a broken connection. A proxy that cuts long-lived streams causes one relist per cut.

Every call may go 90 seconds without a result or a `notifications/progress` for its progress token (newt always sends one), and at most 10 minutes in total, however much progress it reports. A call past either limit is cancelled with `notifications/cancelled`, and so is a call whose turn ends before it answers. Each progress notification also reaches the session's live feed as a `tool_progress` event (5.3) carrying the notification's `message`, or `{progress} of {total}` without one, so the app can show "searching…" while a tool runs.

### 2.2 Tool `_meta`

Set on each tool in the `tools/list` response. All optional.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `newt/retention` | `"ephemeral"`, `"durable"` or `"outward"` | `"durable"` | What reaches history (section 3). |
| `newt/invalidates` | array of page key | `[]` | Page key prefixes that every successful call makes stale. |
| `newt/idempotent` | boolean | `false` | Must be `true` on an `outward` tool. Declares that the handler honours `newt/idempotency_key`. |

```json
{
  "name": "send_message",
  "description": "Send a chat message to the person.",
  "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
  "_meta": { "newt/retention": "outward", "newt/idempotent": true }
}
```

```json
{
  "name": "complete_task",
  "description": "Mark a task done by its number.",
  "inputSchema": { "type": "object", "properties": { "id": { "type": "integer" } }, "required": ["id"] },
  "_meta": { "newt/retention": "durable", "newt/invalidates": [["tasks"]] }
}
```

A field with the wrong type or value, or an `outward` tool without `newt/idempotent: true`, makes the whole tool set fail to load, and the turn does not start. Tool names must not collide with newt's built-ins: `read_file`, `glob`, `grep`, `end_turn`, `remember`, `batch`, `checkpoint`, `load_skill`. A collision fails the turn with `two tools share the name`.

Newt does not validate call arguments against `inputSchema`. It checks only that `arguments` is a JSON object. Validate in the handler.

### 2.3 Call `_meta`

Newt sends these in `params._meta` of every `tools/call`:

| Field | Type | Meaning |
|---|---|---|
| `newt/session` | string | The session key, without the agent name. |
| `newt/idempotency_key` | string | `{agent}/{session}:{turn}:{call id}`. Opaque to you. Stable across retries and replays of the same call (section 4). For a call made through the `batch` tool the call id is `{batch call id}.{index}`. |
| `newt/skills_loaded` | array of string | Skills the agent has loaded this turn, sorted. Lets a handler refuse an action until the right skill was read. |
| `newt/pages_read` | array of `{ path, version }` | Pages the agent has read this turn, sorted by path, with the version newt holds. Lets a handler refuse to act on a page that has since changed. |
| `newt/state` | any JSON | The session's provider state as the turn holds it now (1.4), with any patches applied so far. Absent when there is none. Sent to all of the agent's MCP servers, like `newt/turn_state`. |
| `newt/turn_state` | any JSON | The turn state: the `turn_state` from `/turn` (1.4), or the last `newt/turn_state` a tool result returned this turn. Absent until one of them sets it. One turn state is shared by all of an agent's MCP servers, which newt treats as one trust domain (they receive the same agent token). |
| `traceparent` | string | W3C trace context of the call's `execute_tool` span, as newt's OTLP exporter emits it. Parent your own spans on it to see them under the turn's trace. Sent whether or not OTLP export is on. |
| `progressToken` | string or number | Standard MCP. Report `notifications/progress` with it to keep a long call alive (section 2.1). |

```json
{
  "name": "send_message",
  "arguments": { "text": "Reminder set for tomorrow." },
  "_meta": {
    "newt/session": "ana",
    "newt/idempotency_key": "assistant/ana:0198f2a4-6b1e-7c3d-9a10-5e7d2c8b4f01:call_9xk2",
    "newt/skills_loaded": ["writing"],
    "newt/pages_read": [
      { "path": "notes.md", "version": "7" },
      { "path": "tasks.md", "version": "7" }
    ]
  }
}
```

### 2.4 Result `_meta`

Set on the `tools/call` result. All optional. Newt applies them only to results that are not errors, except `newt/turn_state` and `newt/state_patch`.

| Field | Type | Meaning |
|---|---|---|
| `newt/summary` | string | What history keeps instead of the full result (section 3). |
| `newt/invalidate` | array of page key | More key prefixes to mark stale, in addition to the tool's `newt/invalidates`. |
| `newt/pages` | array of page | Fresh page data. Newt puts these pages in the workspace and clears their stale flag, so the next read needs no refetch. |
| `newt/state_patch` | object | A JSON merge patch (RFC 7396) applied to the session's provider state (1.4), saved when the turn commits (see 1.4 for when a patch is lost). For example, record which page hashes the agent saw. Applied whether or not the result is an error. A patch that is not an object is ignored, and the tool output tells the model so. |
| `newt/turn_state` | any JSON | Replaces the turn state that later calls in this turn receive (2.3). Applied whether or not the result is an error, so a refusal can be counted. Calls that run at the same time (a `batch`) each see the state from before them, and the last result to arrive wins. Starts each turn as `/turn`'s `turn_state`, or empty. A turn resumed after a crash replays its journaled tool results without their turn state, so it starts from `/turn`'s value there too. |

```json
{
  "content": [{ "type": "text", "text": "{\"task\":2}" }],
  "_meta": {
    "newt/summary": "Added task #2: Call the bank",
    "newt/pages": [
      { "key": ["tasks"], "path": "tasks.md", "version": "8", "content": "# Tasks\n- [ ] #1 Call the bank\n- [ ] #2 Call the bank\n" }
    ]
  }
}
```

Invalidation reaches only pages newt already has. A tool that creates a page should return it in `newt/pages`. When a call changes pages, newt appends `(workspace pages changed: a.md, b.md)` to the result the model sees.

If any of the three fields is malformed (wrong type, or a page list that breaks the page rules), newt drops all three, marks every page stale (`[[]]`), appends a note to the result, and logs an error. The call itself still counts as having run.

### 2.5 Result content and errors

- The model sees the text blocks joined with newlines. An image becomes `[image]`, audio `[audio]`, any other block `[resource]`. If there are no content blocks, the JSON of `structuredContent` is used.
- `isError: true` makes the result an error result.
- A JSON-RPC error from the server becomes `The tool call failed: <message>`.
- A broken connection, 90 seconds without a result or progress, or 10 minutes in total, becomes an error result that says the outcome is unknown and tells the model not to repeat the call but to check the workspace first.
- An `input_required` result (the MCP multi round-trip pattern) whose requests are all elicitations (a form, or a link) is how a tool asks the person before it acts, for example to approve a payment. Newt declares the `elicitation` capability (form and url) and turns the result into an error result that says nothing was done yet, lists each question with its fields, and tells the model to ask the person, then call the tool again with the same arguments plus `"_input": { "answers": { "<request id>": { ...fields } or "decline" }, "state": "<requestState>" }`. Newt takes `_input` out of the arguments and sends the answers as `inputResponses` (`{ "action": "accept", "content": ... }`, or `{ "action": "decline" }`) and `state` as `requestState`. The question is also the result's `newt/summary`, so a `durable` or `outward` tool keeps it in history and the answer can come in a later turn; nothing is stored in between. Requests for sampling or roots fail the call with nothing done. A tool with `additionalProperties: false` in its `inputSchema` should still accept `_input`, which never reaches the handler's arguments.
- Newt declares the `io.modelcontextprotocol/tasks` extension. A call the server answers with a task is polled with `tasks/get` at the task's `pollInterval` (default 1 s, kept between 250 ms and 10 s) for up to an hour, instead of the 10 minute call limit, and its completed result is read like any other. A task that fails, is cancelled, asks for input or outlives the hour becomes an error result that says the outcome is unknown. Polling is not journaled: if newt restarts while it waits, the call's outcome is unknown, as for any call.

### 2.6 Skills served over MCP

A server that declares the `io.modelcontextprotocol/skills` extension ([SEP-2640](https://modelcontextprotocol.io/seps/2640-skills-extension)) can serve skills in the [Agent Skills](https://agentskills.io/specification) format instead of putting them in the bootstrap. When newt connects, it calls `skills/list` (every page) and offers each skill under the name `{server}/{skill path}`: the server's name from the bootstrap, then the skill's URI without its scheme and `/SKILL.md`. `skill://acme/billing/refunds/SKILL.md` on server `app` becomes `app/acme/billing/refunds`. The description is the frontmatter's `description`.

A skill is not offered, with a warning in the log, if its `resources` is `"dynamic"` (newt cannot verify it), its URI does not end in `/{name}/SKILL.md`, it has more than 512 files or more than 16 MiB, its file list leaves out its own `SKILL.md`, or it lists a file outside its directory. Two skills from one server that would get the same name, or an MCP skill with the same name as a bootstrap skill, fail the bootstrap load.

Nothing is fetched until the model loads a skill. Then newt reads `SKILL.md` with `resources/read`, checks its size and `sha256` digest against the listing, and checks that its frontmatter equals the listing's `frontmatter`. The result is the file, followed by the skill's other files when it has any:

```
<skill_files>
Read one with load_skill and `file`:
- references/tone.md
</skill_files>
```

When any skill is served over MCP, `load_skill` also takes `file`, a path relative to the skill. Only files the listing names can be read, each checked the same way. Absolute paths, `..`, `.` and empty segments are refused. Verified files are cached by server, URI and digest for as long as the tool set lives. A file that fails a check is never shown to the model: the call fails, and the tool set is marked stale so the next turn lists the skills again. Only text contents are read.

### 2.7 An agent that is only an MCP server

An agent can skip the context provider entirely: `[agents.<name>] mcp = "<url>"` with a `model` (`docs/config.md`). Newt then gets everything from that one server, over plain MCP:

| Context provider field | From the MCP server |
|---|---|
| bootstrap `instructions` | The server's `instructions` (`server/discover`, or `initialize` on `2025-11-25`). Empty instructions fail the turn. |
| bootstrap `model` | `model` in `server.toml`. The other bootstrap knobs take their defaults (1.3). |
| bootstrap `skills` | `skills/list` (2.6). Skills are named `{agent}/{skill path}`. |
| bootstrap `mcp_servers` | The server itself, with `auth: "agent_token"`. |
| `version` | None. Newt reconnects when the tool set goes stale (2.1), which includes the server restarting. |
| turn `pages` | The server's resources for the session, loaded lazily (1.5). |
| turn `pinned`, `instructions`, `constraints` | None. |

**Pages as resources.** At the start of each turn newt calls `resources/list` (every page) and reads each resource with `resources/read`, both with `_meta: { "newt/session": "<session key>" }`, so one server can serve every session. Each text resource becomes a page:

| Page field | From the resource |
|---|---|
| `path` | `name` |
| `key` | `_meta["newt/key"]` when set (an array of string), otherwise `[uri]` |
| `version` | The `sha256:` digest of the text |
| `content` | The text contents whose `uri` matches |

Resources with a `skill://` URI are skills, not pages, and are left out. A resource with no text contents is left out with a warning. The page rules of 1.5 apply: two resources with the same `name` fail the load. Refetching a stale page lists the resources again and reads the matching ones, so a page that is no longer listed disappears and a new one appears.

**Invalidation.** Once the pages are listed, newt opens `subscriptions/listen` with `resourceSubscriptions` set to the session's resource URIs, and `resourcesListChanged`, for the rest of the turn. `notifications/resources/updated` marks that resource's page stale, and `notifications/resources/list_changed` marks every page stale; the next read refetches. The subscription follows the URIs of the latest listing and closes when the turn ends. A server that does not accept resource subscriptions still works: its pages are fresh at the start of each turn, and tool `_meta` (2.2, 2.4) can still invalidate them during one.

The update notification travels on its own stream, so it can arrive after the tool result that caused it. A tool that changes a page and wants the model's next read to see the change for certain should also return `newt/invalidate` or the page itself in `newt/pages` (2.4).

A minimal agent is an MCP server with instructions, tools that do things, and resources that show the person's state. Nothing in it needs to know about newt.

## 3. Retention classes

Retention decides what a turn leaves in history. Everything the model saw in the turn's scratch is dropped at commit except the parts below. The prompt has three layers: a frozen prefix (system prompt and tool list), history, and the turn's scratch.

| Class | Call and result in history | Intended for |
|---|---|---|
| `ephemeral` | Both dropped when the turn commits. | Reads and lookups. Everything the agent needs from them it can read again. |
| `durable` | Call kept. The result is replaced by `newt/summary`; with no summary it is cut to 500 characters. | Writes to your system the agent should remember doing. |
| `outward` | Kept like `durable`. Also journaled before it runs and run under the rules in section 4. | Anything with an effect outside your system: a message, an email, a payment. |

Also dropped at commit: the model's reasoning, the turn-context message, and harness reminders. Assistant text is kept. Built-in tools: `read_file`, `glob`, `grep`, `end_turn`, `batch`, `checkpoint` and `load_skill` are `ephemeral`; `remember` is `durable` with the summary `Remembered.`.

## 4. Exactly-once outward calls

Newt guarantees that an outward call is attempted again after a crash with the same idempotency key. It cannot make your handler idempotent: the pair (newt's journal, your handler's dedupe on the key) is what gives an effect exactly once.

What newt does for an `outward` call:

1. It writes an `outward_intent` entry to the turn journal in Postgres and waits for the write to be acknowledged. If that write fails, or an earlier journal write was lost, the call is not made and the model gets `Not sent: ...`.
2. It calls the tool with `newt/idempotency_key`. The turn id and call ids are fixed when the model's response is journaled, so the key is the same on every replay.
3. It records the tool output in the journal. This write is not awaited, so a crash can lose it.
4. On recovery, newt replays the journal. A call with a recorded output gets that output back and is not made again. A call with no recorded output, including one with an intent entry and nothing after it, is made again with the same key.

What the handler must do:

- Store the key in the same transaction as the effect.
- On a repeated key, return the original result (with the same `newt/summary`) and do nothing else.

Ordering rules in a model step:

- Outward calls run after all other calls of the step, one at a time.
- At most one outward call per `batch` call; `batch` holds up to 10 calls and cannot contain `batch`, `end_turn` or `checkpoint`.
- If an outward call fails, the later outward calls of the step are skipped (`Not run: ...`).
- During compaction no outward call runs.
- Every tool call, not only outward ones, carries an idempotency key. Only outward calls are journaled before running.

The reference handler is `send_message` in `crates/newt-demo/src/tools.rs`: it records the key and sends only the first time.

## 5. Event API (called by your application)

Authentication is the same bearer token. Agent and session key are in the path.

| Method and path | Purpose |
|---|---|
| `POST /v1/agents/{agent}/sessions/{key}/events` | Add an event to the session. |
| `GET /v1/agents/{agent}/sessions/{key}` | Session status. |
| `GET /v1/agents/{agent}/sessions/{key}/live` | Server-sent events for the session. |
| `GET /v1/agents/{agent}/sessions/{key}/history` | The session's history. |
| `POST /v1/agents/{agent}/sessions/{key}/fork` | A new session that starts with this one's history. |
| `DELETE /v1/agents/{agent}/sessions/{key}` | Delete the session and everything stored for it. |
| `GET /healthz` | 200 when Postgres answers, 503 otherwise. No auth. |

Errors are `{ "error": "<message>" }` with status 400 (bad body or key), 401 (missing or wrong token, or unknown agent), 404 (no such session), 409 (conflict, see each endpoint) or 500.

### 5.1 `POST .../events`

```json
{ "id": "m1", "text": "Remind me to call the bank tomorrow.", "kind": "message", "policy": "steer", "priority": "interactive", "meta": { "channel": "chat" } }
```

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `id` | string | yes | | Your id for the event, 1 to 200 bytes. A repeat of the same `id` in the same session is accepted and not queued again, for as long as newt keeps the consumed event (`[retention] inbox_days`, 30 days by default). |
| `text` | string | yes | | What the agent reads. Must not be empty after trimming. |
| `kind` | `"message"` or `"signal"` | no | `"message"` | Stored. Nothing else reads it. |
| `policy` | `"steer"`, `"queue"` or `"collect"` | no | `"steer"` | `steer`: if a turn is running, the event joins it between model calls, and interrupts a model call that has not started producing output. `queue` and `collect`: the event waits for the next turn. Only `steer` is treated differently in code. |
| `priority` | `"interactive"` or `"background"` | no | `"interactive"` | A turn that includes any interactive event runs in the interactive lane. Background model calls may use at most `max_concurrent - 1` of a route's slots (at least 1), which keeps a slot free for interactive turns. |
| `meta` | any JSON | no | none | Stored with the event and sent to the context provider with the turn that answers it (1.4). Not shown to the model. |

Unknown fields are rejected. Response, status 202:

```json
{ "inbox_id": 41, "duplicate": false }
```

### 5.2 `GET .../sessions/{key}`

```json
{ "lease_owner": "host-1234-0198f2a4", "lease_epoch": 3, "pending_events": 0, "history_entries": 12, "live_seq": 9 }
```

`lease_owner` is `null` when no runner holds the session. 404 if the session has never received an event.

### 5.3 `GET .../live`

A `text/event-stream`. Each frame has `id:` (the event's sequence number in the session) and a JSON `data:` payload. Resume with the `Last-Event-ID` header or `?cursor=<seq>`. Frames older than `[retention] live_days` (7 days by default) are deleted, so a resume from further back starts at the oldest frame kept. The server also sends keep-alives. The `type` field is one of `received` (`event_id`, `inbox_id`; sent when an event is accepted), `turn_started`, `turn_steered`, `tool_started`, `tool_progress` (`call_id`, `name`, `message`), `tool_finished`, `turn_completed` and `turn_failed`. All but `received` also carry `at` (RFC 3339) and `session` and `turn`. `turn_started` also carries `model` (the alias the turn's model calls use) and `reset` (whether the turn runs on a reset history, 1.4.1), and its `history_messages` counts the messages the model sees. Payloads hold names, counts and timings, and error text on failure; they do not hold message text.

```
id: 1
data: {"type":"received","event_id":"m1","inbox_id":41}

id: 2
data: {"at":"2026-11-12T17:30:00.120Z","type":"turn_started","session":"assistant/ana","turn":"0198f2a4-...","model":"default","reset":false,"history_messages":0,"events":1,"compaction":false}
```

### 5.4 `GET .../history`

The session's history entries in order. 404 if there is no such session.

```json
{ "entries": [ { "seq": 0, "turn": "0198f2a4-...", "kind": "message", "entry": { "role": "user", "parts": [ { "type": "text", "text": "Add milk." } ] } } ] }
```

`entry` is a message as newt stores it: `role` is `user` or `assistant`, and each part has a `type` of `text`, `tool_call` (`id`, `name`, `arguments`) or `tool_result` (`call_id`, `content`, `is_error`). Only what the retention rules kept is there (section 3). A history reset (1.4.1) leaves no entry: the history before it is returned like the rest.

### 5.5 `POST .../fork`

```json
{ "into": "ana-try", "agent": "assistant-canary", "through_turn": "0198f2a4-..." }
```

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `into` | string | yes | | The new session's key. It must not exist yet. |
| `agent` | string | no | the same agent | The new session's agent. The bearer token must be valid for it too. |
| `through_turn` | string | no | all of it | Copy history through this turn (a `turn` from 5.4). |

Newt creates the session with a copy of the history and checkpoints up to the cut, and nothing else: no pending events, journal or live feed. The next event sent to it starts a turn on that history, as the target agent, with its prompt, tools and model. Response, status 201:

```json
{ "agent": "assistant-canary", "key": "ana-try", "history_entries": 12 }
```

404 if the source session does not exist, 400 for an unknown `through_turn`, 409 if `into` already exists.

`newt-replay` builds on these two. `newt-replay assistant/ana assistant-canary/ana-replay` sends each turn's events from `assistant/ana` to the new session again, one turn at a time, waits for the turn to finish, and prints what the turn did then and what it did now. `--from-turn N` forks the first N turns in instead of replaying them, and `--turns N` stops after N. Point it at a server with `--url`, and name the variable holding the token with `--token-env` (default `NEWT_TOKEN`). A replay runs the target agent's tools for real, so give the target agent an MCP server that is safe to call: a staging app, or `newt-loadtest mock-app`. Give it the `newt-loadtest fake-model` route to replay for free, or a real model to compare prompts or models.

### 5.6 `DELETE .../sessions/{key}`

Deletes the session with its pending events, history, checkpoints, journal and live feed. Use it when a customer leaves or asks for their data to be erased; newt never deletes history on its own. Status 204 on success, 404 if there is no such session, 409 while a runner holds the session's lease (a turn is running or about to). Retry a 409 after the turn ends. An event sent after the delete starts a new, empty session under the same key.
