# Protocol

This document specifies what an application implements for newt, and the small event API it uses to talk to newt:

1. the **agent core**: an MCP server that is the agent. Its instructions are the system prompt, it declares the agent's settings in the `newt` extension, its resources are the workspace, and it can answer two requests the extension defines, `newt/turn` and `newt/turn_end`;
2. **extra servers** the agent core adds, and the `_meta` fields newt reads and sends on tools, calls and results;
3. the **event API**, which the application calls on newt.

There is nothing else to implement: an agent is its agent core, plus any extra servers. The guides on the docs site explain how to use each part; this page is the exact contract.

It is written from `crates/newt-server/src/home/`, `crates/newt-server/src/http.rs`, `crates/newt-server/src/mcp/`, `crates/newt-core/src/{workspace,tools,skills,checkpoint,batch,commit}.rs` and the reference agent in `crates/newt-demo`. Anything not confirmed in code is marked "(unverified)".

Terms: an **agent** is a name in `[agents.<name>]` in `server.toml`. A **session** is one conversation of one agent, identified by a **session key**, a string of 1 to 200 bytes with no `/`. A **turn** is one run of the model loop over the events that arrived for a session. A server **has the newt extension** when it declares `capabilities.extensions.newt`; the agent core always counts as one.

## 1. The agent core

### 1.1 Connecting

Each agent names its agent core in `server.toml` (`mcp`) with a token variable (`token_env`):

| `mcp` | How newt reaches it |
|---|---|
| `https://…` or `http://…` | MCP over Streamable HTTP, with `Authorization: Bearer <token>`, the value of the agent's `token_env`. This is the same token callers of the event API use. |
| `stdio:<command>` | Newt runs `<command>` with `sh -c` and speaks MCP over its stdin and stdout. Its stderr goes to newt's. No token is sent. When the connection is dropped, newt closes its stdin, waits, then sends SIGTERM and finally kills it. A server that exits is started again before the next turn. |

Newt speaks MCP `2026-07-28` (`server/discover`, no handshake) and falls back to the `2025-11-25` `initialize` handshake for a server that does not answer `server/discover`. Connecting and listing tools may take 20 seconds per server.

Newt keeps one connection per server, shared by every session, and before the next turn rebuilds only what changed. A server whose connection broke or closed, that said its tools changed (`notifications/tools/list_changed` on a `subscriptions/listen` stream), or whose change stream ended, is listed again; an extra server that was down is tried again (2.1); every other server is kept as it is. A server newt runs over stdio keeps its state in its process, so while the process runs newt lists it again on the same connection instead of starting a new one.

When newt connects to the agent core again (its connection closed, or an agent core reached over HTTP said its tools changed), it reads `server/discover` again, so a change to the agent's instructions or settings takes effect on the next turn after one of these. An agent core that changes its settings should restart or announce a tool change. An agent core newt runs over stdio declares its settings when newt starts it.

A failure to connect to the agent core, or an agent core whose declaration is rejected (1.2), stops the turn from starting. The session's events stay pending, the lease is released with exponential backoff (2 s doubling, capped at 300 s) and the sweeper tries again. The error is logged as `agent `<agent>`: ...`.

Newt keeps the state an agent core needs between turns (1.3 `state`, 2.3 `newt/turn_state`) and sends it on every request to a server with the newt extension (`newt/turn`, each resource read, each tool call), so the agent core can run as many stateless copies as it likes: any copy can answer any request.

### 1.2 The `newt` extension: the agent's settings

The agent core declares the agent in `capabilities.extensions.newt` of its `server/discover` result (or `initialize`, on `2025-11-25`), next to its `instructions`, which are the system prompt.

```json
{
  "instructions": "You are a personal assistant for one person, reached by chat. ...",
  "capabilities": {
    "tools": {}, "resources": { "subscribe": true },
    "extensions": {
      "newt": {
        "model": "default",
        "first_call_effort": "low",
        "effort": "medium",
        "turn": true,
        "turn_end": true,
        "servers": [
          { "name": "crm", "url": "https://crm.example.com/mcp", "auth_env": "CRM_TOKEN" }
        ]
      }
    }
  }
}
```

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `model` | string | yes | | A `[models]` alias from `server.toml`. Unknown alias is an error. |
| `first_call_effort` | `"low"`, `"medium"` or `"high"` | no | none | Reasoning effort for the first model call of a turn. When absent, newt sends no effort. |
| `effort` | `"low"`, `"medium"` or `"high"` | no | none | Effort for later calls in the turn. |
| `max_output_tokens` | integer | no | 4096 | Per model call. Must be at least 1. |
| `max_model_calls` | integer | no | 8 | Model calls per turn. The turn fails if it uses them all without ending. Must be at least 1. |
| `max_turn_tokens` | integer | no | none | Input plus output tokens one turn may spend across its model calls. Past it, the model is told to send what it has and call `end_turn`; if its next call does not end the turn, the turn fails. |
| `recite` | boolean | no | `false` | Recitation: on long turns, from the fifth model call and every fourth after, append a `<goal>` note quoting the turn's events. Lives in scratch only. Experimental. |
| `require_end_turn` | boolean | no | `false` | Every turn must end with `end_turn`, which must say what the turn recorded (`recorded`) or why there was nothing new (`nothing_new`); a reply with no tool calls gets a reminder instead of ending the turn. Off: a reply with no tool calls ends the turn. See section 6. |
| `builtin_tools` | boolean or array of string | no | `true` | Which of newt's built-in tools the agent is offered: `true` for all of them, `false` for none, or the names of the ones to keep (`read_file`, `glob`, `grep`, `end_turn`, `remember`, `batch`, `checkpoint`, `load_skill`). A built-in that is off is not in the tool list, and a call to it gets an unknown-tool error. Without `load_skill` the skills index leaves the system prompt too. A compaction turn always gets `checkpoint`. Any other name, or `require_end_turn` without `end_turn`, is refused when newt connects. |
| `history_budget` | integer | no | 60000 | Tokens of live history before compaction. Must be at least 1000. |
| `preload_priority` | number | no | 1.0 | Pages whose `annotations.priority` is at least this are read in the background as each turn starts (1.4). |
| `turn` | boolean | no | `false` | Send `newt/turn` before each turn (1.3). |
| `turn_end` | boolean | no | `false` | Send `newt/turn_end` after each committed turn (1.6). |
| `servers` | array of server | no | `[]` | Servers the agent adds (2.1). |

Decoding is strict: an unknown field fails the turn. So do empty instructions, and an agent core that does not declare the extension.

The instructions, the skills index and the tool list form the cached prompt prefix. Any change to them, from the agent core or from an extra server, makes the next turn pay one prompt cache miss; `newt-report` names what changed.

### 1.3 `newt/turn`

Sent to the agent core at the start of every turn, before the first model call, when the agent core declares `turn: true`. It is a JSON-RPC request with method `newt/turn`, like `tools/call`, and has a 10 second timeout. An agent core that does not declare `turn` is never asked, and the turn runs on the declared settings with no per-turn text.

Params:

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
| `state` | any JSON, or `null` | The `state` the agent core set on the last committed turn of the session, or `null` before the first. |

Result, every field optional (unknown fields fail the turn; `_meta` is allowed):

| Field | Type | Default | Meaning |
|---|---|---|---|
| `pinned` | string | none | Session-stable text. Trimmed; empty is ignored. |
| `instructions` | string | none | Guidance for this turn only. |
| `constraints` | array of string | `[]` | Rules that hold right now, for this turn only. Entries are trimmed; empty ones are dropped. |
| `turn_state` | any JSON | none | The turn state the turn's tool calls start with (2.3), for facts the tools need that only this request knows, such as the channel to reply on. |
| `model` | string | the declared one | A `[models]` alias for this turn's model calls. An alias `server.toml` does not define fails the turn like any other bad result: nothing is sent to the model, and the turn is retried with backoff. |
| `effort` | `"low"`, `"medium"` or `"high"` | the declared one | `effort` for this turn's later model calls. |
| `first_call_effort` | `"low"`, `"medium"` or `"high"` | the declared one | `first_call_effort` for this turn's first model call. |
| `reset` | boolean | `false` | `true` runs the turn as if the session had no history (1.3.1). |
| `state` | any JSON | none | The session's new state. Newt saves it in the same transaction that commits the turn, and sends it back as `state` on the next turn. Absent or `null` keeps the previous state; return `{}` to reset it. A turn that does not commit (the lease moved, or it was cancelled) saves nothing, so the state never runs ahead of the session's history. |

```json
{
  "pinned": "Write for WhatsApp: no markdown, two short paragraphs at most.",
  "instructions": "It is 2026-11-12T09:30:00Z.",
  "constraints": ["Do not message the person before Monday: they are on holiday."]
}
```

Where each field goes:

- `pinned` is appended to the system prompt as `\n\n<pinned>\n{pinned}\n</pinned>`, before the skills index. It is part of the cached prefix: free while it stays the same, one cache miss when it changes.
- `instructions` and `constraints` are rendered into one message at the start of the turn, and never enter history:

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

  The same message names any extra server that could not be reached (2.1). If there is nothing to say, no message is added.
- `constraints` are text the model reads. They do not block a tool call. Enforce any constraint that must hold in your tool handlers as well.
- `model`, `effort` and `first_call_effort` replace the declared values for every model call of this turn, and for the compaction that runs right after it, and for nothing else: the next turn starts from the declared settings again unless its `newt/turn` result says otherwise. A different model is a prompt cache miss for that turn and for the next one on the usual model, so use it for turns that are worth it.
- `reset` is described in 1.3.1.

Later in the turn, a resource read (1.4) and a tool result (2.4 `newt/state_patch`) can change the state with a JSON merge patch (RFC 7396): objects merge key by key, `null` removes a key, anything else replaces. Patches touch only the keys they name, so a page read and a tool call running at the same time do not overwrite each other. The state the turn ends with is saved at commit, and only if `newt/turn`'s `state` or a patch changed it. A patch is lost when:

- the turn does not commit (it failed with no effects and will be retried, or its lease moved);
- it arrives after the commit, from a call that outlived the turn;
- the turn was resumed after a crash: replayed tool results do not apply their patches again, and pages read before the crash are not read again.

So keep in the state what you can afford to recompute, and keep the record of anything that must not happen twice in your own data.

The state travels with every resource read and tool call to every server with the newt extension. Keep it small (a few kilobytes; newt does not cap it). Servers without it never receive it.

#### 1.3.1 Starting a session over

`"reset": true` rolls the session to a fresh conversation while it keeps its key, inbox, state and live feed.

- The turn's model calls see no earlier history entries and no earlier checkpoint summary. The events the turn answers are delivered as usual, and the turn's own messages become the start of the new conversation. The history layer of the prompt changes, so the turn is a prompt cache miss.
- Nothing is deleted. `GET .../history` (5.4) still returns every entry and `fork` (5.5) still copies them; the entries are only never loaded into the model's prompt again. Compaction after a reset considers only what came after it.
- The reset is saved in the statement that commits the turn, so it takes effect only if the turn commits. A turn that does not commit (its lease moved, it was cancelled, or it failed with no effects and is retried) resets nothing, and its retry sends `newt/turn` again.
- A turn resumed after a crash (or retried) keeps the `model`, `effort`, `first_call_effort` and `reset` it had once any of its model calls is journaled, whatever `newt/turn` answers the second time. If it chose any of them, newt journals the choice before the first call. If it chose none, the resumed turn runs on the declared settings with no reset, even when `newt/turn` now asks for one. Before the first model call is journaled, a retry starts afresh and takes the new result.
- A reset needs history to start over from: a commit that adds no history rows resets nothing.
- `state` works as without a reset: return the new state, or `{}` to clear it.

Both are in the live feed: `turn_started` carries `model` (the alias the turn uses) and `reset` (5.3).

### 1.4 Pages: resources as the workspace

A **page** is one file in the agent's workspace. The agent reads pages with the built-in `read_file`, `glob` and `grep` tools. Pages are the resources of the agent core and of its extra servers; there is no other way to supply them.

At the start of each turn newt calls `resources/list` on each server that offers resources. Nothing is read yet. When the agent has `read_file`, the turn opens with an index of the listed pages, in the same harness note as the turn's context (never committed to history): top-level pages, pages with a priority and a folder's only page by path, with their `size` and `description`, highest priority first; every other folder as one line with its page count and the `glob` that lists it. The first model call waits for the listing to build it. A server with a very large flat namespace makes a large index: organise pages into folders.

A page is read with `resources/read` only when the agent asks for it. `read_file` waits only for the pages it names, reads them concurrently, and returns the ones that read with a note for each that did not. `glob` answers from the listing. `grep` reads the pages its path pattern matches, and fails if one of them cannot be read. Pages whose `annotations.priority` is at least the agent's `preload_priority` (1.2, default 1.0) are read in the background as the turn starts, so a read finds them loaded; a preloaded page counts as read only once the agent reads it. A tool result's `resource_link` content items are read ahead the same way: each link's page key is its `_meta["newt/key"]` or its URI, scoped to the server that returned it, and a link to a page the listing does not have is ignored.

If the agent core cannot be listed, reads report that error. If an extra server cannot be listed, its last listing is kept (none on the turn it first fails), and reading one of its pages reports the server's error. Requests to a server with the newt extension carry `_meta: { "newt/session": "<session key>", "newt/state": <state> }`, so one server can serve every session; requests to any other server carry nothing of newt's. Each text resource becomes a page:

| Page field | From the resource |
|---|---|
| `path` | `name`, under `<server>/` for an extra server (2.1) |
| `key` | `_meta["newt/key"]` when set (an array of string), otherwise `[uri]` |
| `version` | The `sha256:` digest of the text |
| `content` | The text contents whose `uri` matches |

Resources with a `skill://` URI are skills, not pages, and are left out. A resource with no text contents is left out with a warning. Two pages with the same path, a page with an empty path or key, or an agent core resource whose name starts with an extra server's folder (`crm/…` when the agent core adds `crm`), fail the listing. A failed read fails only the pages it was for; readers waiting on the same read get the same error, and the next read tries again.

A `resources/read` result from a server with the newt extension may carry `_meta["newt/state_patch"]`, a JSON merge patch applied to the session's state (1.3). A patch that is not an object fails the read.

Paths match `glob` patterns with `/` as a literal separator (`*` does not cross `/`, `**` does). A pattern longer than 512 bytes is refused with an error result. Newt reports the path and version of each page the agent read to tools in `newt/pages_read` (2.3).

**Invalidation.** A page goes stale when:

- a tool's `newt/invalidates` (2.2) or a result's `newt/invalidate` (2.4) names a prefix of its key;
- the server sends `notifications/resources/updated` for its URI, or `notifications/resources/list_changed` (which makes all of that server's pages stale). Once the pages are listed, newt opens `subscriptions/listen` with `resourceSubscriptions` set to the session's resource URIs, and `resourcesListChanged`, for the rest of the turn. The subscription follows the URIs of the latest listing and closes when the turn ends. A server that does not accept resource subscriptions still works: its pages are fresh at the start of each turn, and tool metadata can still invalidate them during one.

The next read of a stale page reads it again. A server's `notifications/resources/list_changed` lists it again, so a page that is no longer listed disappears and a new one appears; a read of a key the last listing did not have also lists its server again, once for all readers waiting on it. After a tool call, stale pages the agent has already read in this turn are also refreshed in the background.

A server's keys are its own: a key from an extra server's tool or notification only ever matches that server's pages. The agent core's empty key `[]` matches every page of the agent.

The update notification travels on its own stream, so it can arrive after the tool result that caused it. A tool that changes a page and wants the model's next read to see the change for certain should also return `newt/invalidate` or the page itself in `newt/pages` (2.4).

### 1.5 Skills

A skill is instructions for one kind of task, loaded when the agent needs them. Skills are served over MCP: a server that declares the `io.modelcontextprotocol/skills` extension ([SEP-2640](https://modelcontextprotocol.io/seps/2640-skills-extension)) serves skills in the [Agent Skills](https://agentskills.io/specification) format. When newt connects, it calls `skills/list` (every page) and offers each skill under the name `{server}/{skill path}`: the server's name (the agent's name for the agent core, the name in `servers` for an extra server), then the skill's URI without its scheme and `/SKILL.md`. `skill://acme/billing/refunds/SKILL.md` on the agent core of agent `app` becomes `app/acme/billing/refunds`. The description is the frontmatter's `description`.

Newt appends this block to the system prompt, after `pinned`, sorted by name:

```
<skills>
Load a skill with load_skill before doing what it covers.
- app/acme/billing/refunds: How to handle a refund request.
</skills>
```

and adds a built-in `load_skill` tool whose `name` argument is one of the skill names. Loaded skill names are passed to tools in `newt/skills_loaded`.

A skill is not offered, with a warning in the log, if its `resources` is `"dynamic"` (newt cannot verify it), its URI does not end in `/{name}/SKILL.md`, it has more than 512 files or more than 16 MiB, its file list leaves out its own `SKILL.md`, or it lists a file outside its directory. Two skills from one server that would get the same name fail the load.

Nothing is fetched until the model loads a skill. Then newt reads `SKILL.md` with `resources/read`, checks its size and `sha256` digest against the listing, and checks that its frontmatter equals the listing's `frontmatter`. The result is the file, followed by the skill's other files when it has any:

```
<skill_files>
Read one with load_skill and `file`:
- references/tone.md
</skill_files>
```

`load_skill` also takes `file`, a path relative to the skill. Only files the listing names can be read, each checked the same way. Absolute paths, `..`, `.` and empty segments are refused. Verified files are cached by server, URI and digest for as long as the connection lives. A file that fails a check is never shown to the model: the call fails, and the tool set is marked stale so the next turn lists the skills again. Only text contents are read. The skill stays in that turn's scratch and never enters history.

### 1.6 `newt/turn_end`

Sent to the agent core after a turn commits, when it declares `turn_end: true`.

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
| `outcome` | `completed`; `failed` for a turn that failed after it had effects (an outward call or a durable tool) and was committed with what it did; `dead_lettered` when newt gave up on the events after `max_attempts` failed turns with no effects. A failed turn with no effects that will be retried does not send `newt/turn_end`. |
| `events` | Every event the turn consumed, as `{ id, meta }`, oldest first: the ones it started on and any that steered it. For `dead_lettered`, the events given up on. |
| `state` | The session's state after the turn (unchanged for `dead_lettered`). |
| `turn_state` | The turn state when the turn ended (2.3). After a crash and resume, only what calls made since the resume set. |
| `reply` | What the agent said when it finished: the text of the message that called `end_turn`, or, when that message had none, the text-only message just before it that made newt remind the agent to end. `null` when there is neither, and always for `failed` and `dead_lettered`. Earlier mid-turn text is never used. |

Answer with an empty result. Newt writes the request in the same transaction as the turn, so it survives a crash, and sends it at least once: right after the commit, then again with backoff (at least 30 seconds, up to 5 minutes apart) until the agent core answers with a result instead of an error. A delivery holds the request for 30 seconds, so two runners do not send it at once, but a repeat after a lost answer is possible: use `turn` to deduplicate. Deliveries across turns can arrive late and out of order. After 30 failed attempts (about two hours) newt gives up and logs an error. Deleting the session deletes its undelivered requests.

## 2. Servers and tools

### 2.1 Extra servers

The agent core lists the agent's extra servers in `servers` (1.2):

```json
{ "name": "crm", "url": "https://crm.example.com/mcp", "auth_env": "CRM_TOKEN" }
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | 1 to 32 lowercase letters, digits or `-`, starting with a letter or digit. Unique per agent, and not the agent's own name. It becomes the server's folder in the workspace and the prefix of its tools. |
| `url` | string | yes | `https://…`, `http://…` or `stdio:<command>`, as for the agent core (1.1). |
| `auth_env` | string | no | The environment variable holding the server's token, sent as `Authorization: Bearer <token>`. Newt refuses to load the agent if it is unset or empty. Without it, nothing is sent. |

Newt connects to every extra server at the same time, each with its own 20 second limit. A server that cannot be reached, does not list its tools in time, offers a tool newt cannot load (a name over the length limit, an `outward` tool without `newt/idempotent`), or offers a tool named like one of the agent core's, is left out: its tools and pages are missing, and the turn context names it as unavailable (1.3) so the model can say so. The rest of the agent runs. A server that did not answer is tried again every 30 seconds in the background, the same way, and added back before the next turn once it connects and lists cleanly; a reload never waits on it. A server left out for what it offers is tried again when newt next connects to the agent core.

What newt does with an extra server depends on whether it has the newt extension (declares `capabilities.extensions.newt`, with an empty object):

| | Has the newt extension | Plain MCP server |
|---|---|---|
| Tool names | `<name>__<tool>` | `<name>__<tool>` |
| Tool `_meta` (2.2) | used | ignored: every tool is `durable` and invalidates nothing |
| Call `_meta` (2.3) | all of it | `newt/idempotency_key`, `traceparent` and `progressToken` only |
| Result `_meta` (2.4) | applied, to its own pages | ignored |
| Resources | pages under `<name>/`, read with the session and state | pages under `<name>/`, read with nothing of newt's |
| Skills | offered as `<name>/<skill path>` | the same |
| `instructions` | not used | not used |

Choosing a server is choosing to trust it: its tool descriptions reach the model's prompt and its results reach the model. Newt only keeps the session's data from servers that did not ask for it, and never takes instructions on retention, pages or state from them.

The agent core's tools keep their names. An extra server's tools are named `<name>__<tool>`, so no two servers' tools can clash. A name longer than 64 characters, the limit model APIs accept, fails the load for the agent core and leaves an extra server out. Tool names must not collide with newt's built-ins: `read_file`, `glob`, `grep`, `end_turn`, `remember`, `batch`, `checkpoint`, `load_skill`. A collision fails the turn with `two tools share the name`.

Newt sorts each server's tools by name, so the tool list in the cached prompt prefix is byte-identical whatever order a server lists them in. The agent core's tools come first, then each extra server's in the order `servers` lists them.

Every call may go 90 seconds without a result or a `notifications/progress` for its progress token (newt always sends one), and at most 10 minutes in total, however much progress it reports. A call past either limit is cancelled with `notifications/cancelled`, and so is a call whose turn ends before it answers. Each progress notification also reaches the session's live feed as a `tool_progress` event (5.3) carrying the notification's `message`, or `{progress} of {total}` without one, so the app can show "searching…" while a tool runs.

### 2.2 Tool `_meta`

Set on each tool in the `tools/list` response of a server with the newt extension. All optional. Other servers' tool `_meta` is ignored.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `newt/retention` | `"ephemeral"`, `"durable"` or `"outward"` | `"durable"` | What reaches history (section 3). |
| `newt/invalidates` | array of page key | `[]` | Key prefixes of the server's own pages that every successful call makes stale. |
| `newt/idempotent` | boolean | `false` | Declares that the handler honours `newt/idempotency_key`. Must be `true` on an `outward` tool. On a `durable` tool, a call a crash interrupted is made again instead of reported (section 4). |
| `newt/ends_turn` | boolean | `false` | A successful call ends the turn once every call of its response has succeeded, with no further model call (6). Newt adds a sentence saying so to the tool's description. |

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

A field with the wrong type or value, or an `outward` tool without `newt/idempotent: true`, makes the whole tool set fail to load, and the turn does not start.

Newt does not validate call arguments against `inputSchema`. It checks only that `arguments` is a JSON object. Validate in the handler.

### 2.3 Call `_meta`

Newt sends these in `params._meta` of every `tools/call` to a server with the newt extension. A plain server gets only `newt/idempotency_key`, `traceparent` and `progressToken`.

| Field | Type | Meaning |
|---|---|---|
| `newt/session` | string | The session key, without the agent name. |
| `newt/idempotency_key` | string | `{agent}/{session}:{turn}:{call id}`. Opaque to you. Stable across retries and replays of the same call (section 4). For a call made through the `batch` tool the call id is `{batch call id}.{index}`. |
| `newt/skills_loaded` | array of string | Skills the agent has loaded this turn, sorted. Lets a handler refuse an action until the right skill was read. |
| `newt/pages_read` | array of `{ path, version }` | Pages the agent has read this turn, sorted by path, with the version newt holds. Lets a handler refuse to act on a page that has since changed. |
| `newt/state` | any JSON | The session's state as the turn holds it now (1.3), with any patches applied so far. Absent when there is none. |
| `newt/turn_state` | any JSON | The turn state: the `turn_state` from `newt/turn` (1.3), or the last `newt/turn_state` a tool result returned this turn. Absent until one of them sets it. One turn state is shared by all of the agent's servers that speak newt. |
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
      { "path": "notes.md", "version": "sha256:…" },
      { "path": "tasks.md", "version": "sha256:…" }
    ]
  }
}
```

The call carries the tool's name on its own server, without the `<name>__` prefix.

### 2.4 Result `_meta`

Set on the `tools/call` result by a server with the newt extension; ignored from any other server. All optional. Newt applies them only to results that are not errors, except `newt/turn_state` and `newt/state_patch`.

| Field | Type | Meaning |
|---|---|---|
| `newt/summary` | string | What history keeps instead of the full result (section 3). |
| `newt/invalidate` | array of page key | More key prefixes of the server's own pages to mark stale, in addition to the tool's `newt/invalidates`. |
| `newt/ends_turn` | boolean | Overrides the tool's `newt/ends_turn` for this call: `true` ends the turn after a tool that does not declare it, `false` keeps going after one that does (6). |
| `newt/pages` | array of page | Fresh page data for the server's own pages: `{ key, path, version, content }`, with the path as the server names it (newt puts an extra server's under its folder). Newt puts these pages in the workspace and clears their stale flag, so the next read needs no refetch. |
| `newt/state_patch` | object | A JSON merge patch (RFC 7396) applied to the session's state (1.3), saved when the turn commits (see 1.3 for when a patch is lost). Applied whether or not the result is an error. A patch that is not an object is ignored, and the tool output tells the model so. |
| `newt/turn_state` | any JSON | Replaces the turn state that later calls in this turn receive (2.3). Applied whether or not the result is an error, so a refusal can be counted. Calls that run at the same time (a `batch`) each see the state from before them, and the last result to arrive wins. Starts each turn as `newt/turn`'s `turn_state`, or empty. A turn resumed after a crash replays its journaled tool results without their turn state, so it starts from `newt/turn`'s value there too. |

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

If any of the three workspace fields is malformed (wrong type, or a page list that breaks the page rules), newt drops all three, marks every page of that server stale, appends a note to the result, and logs an error. The call itself still counts as having run.

### 2.5 Result content and errors

- The model sees the text blocks joined with newlines. An image becomes `[image]`, audio `[audio]`, any other block `[resource]`. If there are no content blocks, the JSON of `structuredContent` is used.
- `isError: true` makes the result an error result.
- A JSON-RPC error from the server becomes `The tool call failed: <message>`.
- A broken connection, 90 seconds without a result or progress, or 10 minutes in total, becomes an error result that says the outcome is unknown and tells the model not to repeat the call but to check the workspace first.
- An `input_required` result (the MCP multi round-trip pattern) whose requests are all elicitations (a form, or a link) is how a tool asks the person before it acts, for example to approve a payment. Newt declares the `elicitation` capability (form and url) and turns the result into an error result that says nothing was done yet, lists each question with its fields, and tells the model to ask the person, then call the tool again with the same arguments plus `"_input": { "answers": { "<request id>": { ...fields } or "decline" }, "state": "<requestState>" }`. Newt takes `_input` out of the arguments and sends the answers as `inputResponses` (`{ "action": "accept", "content": ... }`, or `{ "action": "decline" }`) and `state` as `requestState`. The question is also the result's `newt/summary`, so a `durable` or `outward` tool keeps it in history and the answer can come in a later turn; nothing is stored in between. Requests for sampling or roots fail the call with nothing done. A tool with `additionalProperties: false` in its `inputSchema` should still accept `_input`, which never reaches the handler's arguments.
- Newt declares the `io.modelcontextprotocol/tasks` extension. A call the server answers with a task is polled with `tasks/get` at the task's `pollInterval` (default 1 s, kept between 250 ms and 10 s) for up to an hour, instead of the 10 minute call limit, and its completed result is read like any other. A task that fails, is cancelled, asks for input or outlives the hour becomes an error result that says the outcome is unknown. Polling is not journaled: if newt restarts while it waits, the call's outcome is unknown, as for any call.

## 3. Retention classes

Retention decides what a turn leaves in history. Everything the turn added to the prompt is dropped at commit except the parts below.

| Class | Call and result in history |
|---|---|
| `ephemeral` | Both dropped when the turn commits. |
| `durable` | Call kept. The result is replaced by `newt/summary`; with no summary it is cut to 500 characters. Runs only once its model response is journaled (section 4.1). |
| `outward` | Kept like `durable`. Also journaled before it runs and run under the rules in section 4. |

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
- Every tool call, not only outward ones, carries an idempotency key.

### 4.1 Durable calls after a crash

A model response is journaled when it arrives. Its `durable` calls run only once Postgres has acknowledged that write (not its flush to disk: the entry survives newt crashing, not Postgres). `ephemeral` calls don't wait. If the write fails, or an earlier journal write was lost, the step's `durable` calls are not made and the model gets `Not run: ...`.

On recovery, a `durable` call with a recorded output gets that output back. A call from a journaled response with no recorded output may or may not have taken effect:

- With `newt/idempotent: true` it is made again with the same idempotency key.
- Without it, it is not made again. The model gets an error result starting with `Interrupted:`, which says the call may or may not have taken effect and to check the current state before calling it again.

If the response itself was not journaled, none of its `durable` calls ran: newt asks the model again.

The reference handler is `send_message` in `crates/newt-demo/src/tools.rs`: it records the key and sends only the first time.

## 5. Event API (called by your application)

Authentication is the same bearer token. Agent and session key are in the path.

| Method and path | Purpose |
|---|---|
| `POST /v1/agents/{agent}/sessions/{key}/events` | Add an event to the session. |
| `POST /v1/agents/{agent}/sessions/{key}/cancel` | Cancel the session's running turn (5.9). |
| `POST /v1/agents/{agent}/sessions/{key}/chat` | Add a chat message and stream its turn as an AI SDK UI message stream (5.7). |
| `GET /v1/agents/{agent}/sessions/{key}` | Session status. |
| `GET /v1/agents/{agent}/sessions/{key}/live` | Server-sent events for the session. |
| `GET /v1/agents/{agent}/sessions/{key}/history` | The session's history. |
| `POST /v1/agents/{agent}/sessions/{key}/fork` | A new session that starts with this one's history. |
| `DELETE /v1/agents/{agent}/sessions/{key}` | Delete the session and everything stored for it. |
| `GET /healthz` | Load and readiness (5.8). No auth. |
| `GET /metrics` | Counters and gauges in Prometheus text format (5.10). No auth. |

Every error of this API, from a handler or from the framework (unknown route, wrong method, body too large), has one JSON shape:

```json
{ "error": { "code": "inbox_full", "message": "the session holds too many unconsumed events; retry shortly" } }
```

`code` is stable and meant for programs; `message` is for people and may change. 429 and 503 carry a `Retry-After` header in seconds. The codes:

| Status | `code` | When |
|---|---|---|
| 400 | `invalid_request` | Bad body, id, text or session key, or a NUL character anywhere in them (Postgres cannot store it). |
| 401 | `unauthorized` | Missing or wrong token, or unknown agent. |
| 404 | `not_found` | No such session or route. |
| 405 | `method_not_allowed` | Wrong method for the path. |
| 409 | `event_id_conflict` | An event id reused with different content (5.1). |
| 409 | `turn_not_running` | `cancel` when no turn is running, or the `turn` given is not the running one (5.9). |
| 409 | `session_running` | `DELETE` on a session that is running a turn. |
| 409 | `session_exists` | `fork` onto a session that already exists. |
| 409 | `live_frames_gone` | `chat` accepted the message but its live frames were already deleted. |
| 413 | `payload_too_large` | Body over 2 MiB. |
| 415 | `unsupported_media_type` | |
| 429 | `inbox_full` | The session holds `max_pending_events` unconsumed events (5.1). |
| 500 | `internal` | Details are in the server log only. |
| 503 | | `GET /healthz` when Postgres does not answer (5.8); its body is the health body, not this shape. |

### 5.1 `POST .../events`

```json
{ "id": "m1", "text": "Remind me to call the bank tomorrow.", "kind": "message", "policy": "steer", "priority": "interactive", "meta": { "channel": "chat" } }
```

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `id` | string | yes | | Your id for the event, 1 to 200 bytes. A repeat of the same `id` with the same content in the same session is accepted and not queued again, for as long as newt keeps the consumed event (`[retention] inbox_days`, 30 days by default). A repeat with different content is refused with 409 (below). |
| `text` | string | yes | | What the agent reads. Must not be empty after trimming. |
| `kind` | `"message"` or `"signal"` | no | `"message"` | Stored. Nothing else reads it. |
| `policy` | `"steer"`, `"queue"` or `"collect"` | no | `"steer"` | `steer`: if a turn is running, the event joins it between model calls, and interrupts a model call that has not started producing output. `queue` and `collect`: the event waits for the next turn. Only `steer` is treated differently in code. |
| `priority` | `"interactive"` or `"background"` | no | `"interactive"` | A turn that includes any interactive event runs in the interactive lane. Background model calls may use at most `max_concurrent - 1` of a route's slots (at least 1), which keeps a slot free for interactive turns. |
| `meta` | any JSON | no | none | Stored with the event and sent to the agent core in `newt/turn` with the turn that answers it (1.3). Not shown to the model. |

Unknown fields are rejected. Response, status 202:

```json
{ "inbox_id": 41, "duplicate": false }
```

A session holds at most `[agents.<name>] max_pending_events` unconsumed events (100 by default). An event that would go past it is not stored and the answer is 429 with a `Retry-After` header (seconds) and the JSON error body; send it again after a pause, with the same `id`. A repeat of an `id` the session already holds (same content) is not counted again and still answers 202 with `"duplicate": true`, even when the session is full. An `id` that was accepted before with different content is refused with 409 and the error `an event with this id and different content was already accepted`, and nothing is stored: the first event stands. "Content" is a sha256 over the event's `text` and `meta` (object keys sorted, so key order does not matter); `kind`, `policy` and `priority` are not part of it, and neither is anything else, as an event has no other body fields. Events accepted before newt stored hashes (migration `0009`) are always treated as the same content. The count and the insert happen in one statement under a per-session lock, so concurrent posts cannot go past the cap.

### 5.2 `GET .../sessions/{key}`

```json
{ "lease_owner": "host-1234-0198f2a4", "lease_epoch": 3, "pending_events": 0, "history_entries": 12, "live_seq": 9 }
```

`lease_owner` is `null` when no runner holds the session. 404 if the session has never received an event.

### 5.3 `GET .../live`

A `text/event-stream`. Each frame has `id:` (the event's sequence number in the session) and a JSON `data:` payload. Resume with the `Last-Event-ID` header or `?cursor=<seq>`. Frames older than `[retention] live_days` (7 days by default) are deleted; a resume from further back gets a `gap` frame first (below) and then starts at the oldest frame kept. The server sends an SSE comment line (`:`) as a keep-alive every 15 seconds when nothing else was sent, so a proxy with an idle timeout above that keeps the connection open. A client should treat 30 seconds of silence as a dead connection and reconnect with its last `id`.

When the cursor (`?cursor=` or `Last-Event-ID`) is older than the oldest frame still kept, the stream starts with one `gap` frame before anything else: `data: {"type":"gap","from":2,"to":3}`, the first and last sequence numbers that were deleted and can no longer be read. It has no `id:`, so it does not move the client's cursor; the next frame carries the first id still available. A fresh stream (no cursor, or 0) never starts with a gap, and neither does a cursor that is current. A client that needs the lost frames' effects can read `GET .../history` (5.4). The `type` field is one of `received` (`event_id`, `inbox_id`, the event's `meta` when the agent sets `live_meta`, and its `text` when the agent sets `live_stream`; sent when an event is accepted), `turn_started`, `turn_steered`, `tool_started`, `tool_progress` (`call_id`, `name`, `message`), `tool_finished`, `turn_completed` and `turn_failed` (`error`, and `reason: "cancelled"` when the turn was cancelled, 5.9). An agent with `live_stream = true` also gets `text_delta` (`text`), `tool_input_delta` (`call_id`, `name`, `delta`), `tool_input` (`call_id`, `name`, `input`), `tool_output` (`call_id`, `name`, `output`, `is_error`), `model_call_started` (`call`, `model`) and `model_call_finished` (`call`, `stop`); see below. All but `received` also carry `at` (RFC 3339) and `session` and `turn`. `turn_started` also carries `model` (the alias the turn's model calls use) and `reset` (whether the turn runs on a reset history, 1.3.1), and its `history_messages` counts the messages the model sees. Payloads hold names, counts and timings, and error text on failure; they do not hold message text or tool arguments, except the frames `live_stream` adds.

`text_delta` frames carry a chunk of the answer the model is writing, in order: concatenated, the chunks of one model call equal the text that call adds to history. Newt batches them, sending at most about every 50 ms or at a line or sentence end, and always before the model call ends, so every `text_delta` of a turn comes before its `turn_completed`. They are live-only: never journaled, never in history, never in the JSONL event file or OTLP, and removed with the rest of the live feed after `live_days`. Reasoning text is never sent. A model call replayed from the journal after a crash sends no text frames.

`tool_input_delta` frames carry a chunk of the arguments of the tool call `call_id` as the model writes them, in order and batched the same way: concatenated, the chunks of one call are the JSON text of its arguments, which can be partial JSON until the call is complete. They always come after the call's `tool_started` and before its `tool_input`. `tool_input` carries the call's complete, parsed `input` once the model has finished writing it, before the tool runs and so before its `tool_finished`. Together with `text_delta` they let an app that answers through a tool (a `send_message` whose `text` argument is the reply) show the reply as it is written. `tool_output` carries the call's result as the model will see it, cut to its first 4,000 characters, just before its `tool_finished`. All of these are live-only like `text_delta`, and are sent only to agents with `live_stream = true`; `tool_started` and `tool_finished` are sent to every agent.

`model_call_started` and `model_call_finished` mark the start and end of each model call of the turn, so a client can tell the steps of a turn apart. They carry no prompt or output, and are sent only under `live_stream`.

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

`entry` is a message as newt stores it: `role` is `user` or `assistant`, and each part has a `type` of `text`, `tool_call` (`id`, `name`, `arguments`) or `tool_result` (`call_id`, `content`, `is_error`). Only what the retention rules kept is there (section 3). A history reset (1.3.1) leaves no entry: the history before it is returned like the rest.

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

Deletes the session with its pending events, history, checkpoints, journal and live feed. Newt never deletes history on its own. Status 204 on success, 404 if there is no such session, 409 while a runner holds the session's lease (a turn is running or about to). Retry a 409 after the turn ends. An event sent after the delete starts a new, empty session under the same key.

### 5.7 `POST .../chat`

An endpoint for chat UIs built on the Vercel AI SDK: point `DefaultChatTransport` of `useChat` at it (`api: ".../v1/agents/{agent}/sessions/{key}/chat"`, with the agent's bearer token in `headers`). It needs the agent's `live_stream = true` and answers 400 without it.

The body is the one the transport sends: `{ "id": "...", "messages": [UIMessage], "trigger": "...", ... }`. Newt takes the last message with role `user`, joins its `text` parts with a blank line, and accepts it exactly like `POST .../events` with the message's `id` as the event id and the default policy (`steer`), so the same token, validation and deduplication apply: posting the same message `id` again does not add an event, and replays the stream of the turn that answered it. Everything else in the body is ignored, because the session's history is newt's, not the browser's. 400 if there is no user message with text.

The response is a `text/event-stream` with the header `x-vercel-ai-ui-message-stream: v1`. Every frame is `data: {json}`, and the stream ends with `data: [DONE]`. The chunks are the AI SDK's UI message stream chunks, mapped from the live feed frames of the turn that consumes the message (5.3):

| Live frame | Chunks |
|---|---|
| start of the stream | `start` |
| `model_call_started` | `start-step` (closing the previous step first) |
| `text_delta` | `text-start` when no text block is open, then `text-delta` (`id`, `delta`) |
| `tool_started` | `text-end` for an open text block, then `tool-input-start` (`toolCallId`, `toolName`) |
| `tool_input_delta` | `tool-input-delta` (`toolCallId`, `inputTextDelta`) |
| `tool_input` | `tool-input-available` (`toolCallId`, `toolName`, `input`) |
| `model_call_finished` | `text-end` if open, then `finish-step` |
| `tool_output` | `tool-output-available` (`toolCallId`, `output`: the result text), or `tool-output-error` (`toolCallId`, `errorText`: the error the model sees) when the call failed |
| `tool_finished` | only for a call with no `tool_output` before it: `tool-output-available` (`toolCallId`, `output: { ms }`), or `tool-output-error` with a generic `errorText` |
| `turn_completed` | `finish` |
| `turn_failed` | `error` (`errorText`: a generic message, never the provider's error), then `finish` |

One step is one model call. A tool's output is what the model sees, cut to its first 4,000 characters like the `tool_output` frame, so a tool that returns text the end user must not read should not be shown with this endpoint as is.

The stream follows the turn that consumes the message. A message that arrives while a turn is running is steered into it (`policy: steer`), and the stream joins that running turn from its `turn_steered` frame: text and tool calls the turn produced before then are not repeated. If the running turn ends before it picks the message up, the message starts the next turn, and the stream follows that one. A second message sent while the first is still streaming opens a second stream on the same turn, and both end when it does.

### 5.8 `GET /healthz`

No auth, so keep the port off the public internet (5.10). Answers 200 while a `select 1` on Postgres succeeds and 503 (with `Retry-After`) when it fails, with the same JSON body either way:

```json
{ "ready": true, "running": 3, "waiting": 1, "max_sessions": 64, "oldest_wait_ms": 120 }
```

| Field | Meaning |
|---|---|
| `ready` | `true` when the database check passed. |
| `running` | Turns running on this instance, each holding one of the `max_sessions` slots. |
| `waiting` | Sessions that were woken and are waiting for a slot. |
| `max_sessions` | The `[runner] max_sessions` this instance runs with. |
| `oldest_wait_ms` | How long the longest-waiting session has waited, 0 when none is. |

The figures are this instance's own. A load balancer can use the status for liveness and `waiting` or `oldest_wait_ms` to shed load before callers see slow starts.

### 5.9 `POST .../cancel`

Cancels the turn the session is running right now, on whichever instance runs it: send the request to any instance. Same auth as the rest of the API.

```json
{ "turn": "0198f2a4-..." }
```

The body is optional and `turn` is optional. Without it, the request cancels whatever turn is running. With it, the request cancels only that turn: a client that saw `turn_started` for turn X and cancels X can never cancel the turn that follows it. The turn id is the `turn` field of the live feed's frames (5.3). Unknown fields are rejected.

| Status | `code` | Meaning |
|---|---|---|
| 202 `{ "turn": "<turn id>" }` | | The request is recorded for that turn and every instance was told. The turn stops at its next check, the live feed gets `turn_failed` with `"reason": "cancelled"`, and the lease is released. The answer does not wait for the turn to stop. A repeat for the same turn answers 202 again and still cancels the turn once. |
| 404 | `not_found` | No such session. |
| 409 | `turn_not_running` | No turn holds the session's lease right now (it is idle, between turns, or its owner died and the lease expired), or `turn` was given and is not the running turn. Nothing is recorded. |

How it works: the request writes `cancel_turn` (the turn id) and `cancel_requested_at` on the session row and sends a Postgres notification on the `newt_cancel` channel. Every instance listens on that channel. The instance that runs the turn reads the flag, checks that the turn id is the one it is running under its current lease, and cancels the turn's token at once. A notification is not durable, so the owner also finds the flag at its next lease heartbeat, which returns it in the same statement that renews the lease (every `lease_ttl / 3`, 10 s by default): a request is acted on within one heartbeat even when the notification was lost, and no model call reads the database for it. The flag is cleared when the turn ends, however it ends (committed, dead-lettered), and the owner acts only when the flag's turn id is the turn it runs, so a stale flag never cancels a later turn.

What happens to the events the turn held follows the failure rules (design: retries), with cancel counted as a failure that is not retried:

- No outward effect happened yet: the turn's events are dead-lettered at once with the reason `cancelled`. They are consumed, so the session does not run them again, and the turn's journal is dropped. The next event starts a new turn.
- An outward call already ran: the turn commits what it did, like any failed turn with effects, and its events are consumed.

This differs from shutdown, which cancels turns through the same token but leaves their events unconsumed so another instance resumes them from the journal.

A turn stops when the model call in progress is interrupted or at the next batch of tool calls (calls still pending in a batch are skipped); a tool call already running is not interrupted. A turn that crashed and has not been resumed yet holds no live lease, so cancelling it answers 409; its events are not cancelled by this endpoint.

### 5.10 `GET /metrics`

Prometheus text format (`text/plain; version=0.0.4`). No auth, like `/healthz`: the output holds counts and timings only, never agent names, session keys or event content, but keep the port off the public internet. Everything is per instance and resets when the process restarts; sum across instances in the scraper.

| Series | Type | Meaning |
|---|---|---|
| `newt_events_total{outcome}` | counter | Events posted to the event API (and `/chat`): `accepted`, `duplicate` (same id and content), `limited` (429, inbox full) or `conflicted` (409, same id, other content). |
| `newt_turns_total{outcome}` | counter | `started`, `completed`, `failed` and `cancelled` turns. A turn interrupted by shutdown counts as cancelled. |
| `newt_commits_fenced_total` | counter | Turn commits discarded because the lease had moved to another owner or epoch. |
| `newt_dead_letters_total` | counter | Events given up on (after the retry limit, or on cancel). |
| `newt_retries_total` | counter | Sessions released with a backoff to be retried. |
| `newt_wakes_dropped_total` | counter | Wakes dropped because `[runner] max_waiting` sessions were already waiting for a slot; the sweep picks those sessions up later. |
| `newt_running_turns` | gauge | Turns holding a slot. |
| `newt_waiting_turns` | gauge | Woken sessions waiting for a slot. |
| `newt_max_sessions` | gauge | `[runner] max_sessions`. |
| `newt_oldest_wait_seconds` | gauge | How long the longest-waiting session has waited. |
| `newt_slot_wait_seconds` | histogram | Time from a session's wake to getting a turn slot (buckets 1 ms to 120 s). |

## 6. How a turn ends

A turn ends at the first of these:

- **A reply.** A model response with text and no tool calls ends the turn, and its text is the turn's reply. With `require_end_turn` (1.2) it gets a reminder instead, and the turn fails if the next response also calls no tool.
- **An ending call.** After a response's tool calls have run, the turn ends if one of them is an ending call and every call of the response succeeded. If any call failed, the turn goes on and the model sees the errors. The ending call can sit in the same response as the last action (a send and the end together), which saves the model call that would otherwise only say it is done. Ending calls are:
  - the built-in `end_turn`. Its `recorded` and `nothing_new` are optional, and required (one of them) only with `require_end_turn`;
  - a call to a tool that declares `newt/ends_turn: true` (2.2), unless its result says `newt/ends_turn: false` (2.4);
  - a call whose result says `newt/ends_turn: true`.
- **The limits.** `max_model_calls` and `max_turn_tokens` (1.2) fail a turn that has not ended.

A response with no text and no tool calls gets one reminder and then fails the turn. A compaction turn ends only with `checkpoint`.
