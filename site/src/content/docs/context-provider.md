---
title: Context provider
description: The JSON endpoints your app serves so newt can build each prompt from the customer's current state, and the session state newt keeps for it.
---

The context provider is how your app tells newt what the agent is and what it can see. It is three JSON endpoints under one base URL, plus an optional fourth. newt calls them; your app never calls newt back.

The provider can be stateless. newt keeps a small JSON state per session for it, so any copy of your provider can answer any call.

```toml
[agents.assistant]
context_provider = "https://app.example.com/assistant"
token_env = "ASSISTANT_TOKEN"
```

One token authenticates both directions: callers send it to newt as `Authorization: Bearer`, and newt sends it to your provider.

:::tip
An agent can skip the context provider and be [only an MCP server](../mcp-agents/).
:::

## The endpoints

| Call | When newt makes it | Returns |
|---|---|---|
| `GET {base}/bootstrap` | First turn, then whenever `version` changes | Instructions, model, skills, MCP servers |
| `POST {base}/turn` | Start of every turn | The session's pages and this turn's guidance |
| `POST {base}/pages` | A read of a stale page, or the first read when `turn` sent no pages | Fresh pages |
| `POST {base}/turn_end` | After each committed turn, if the bootstrap sets `turn_end: true` | Nothing; any 2xx |

## Bootstrap: the same for every session

```json
{
  "version": "2026-11-12.1",
  "instructions": "You are a personal assistant for one person, reached by chat.",
  "model": "default",
  "skills": [
    { "name": "writing", "description": "How to word a message to the person.", "content": "Short sentences. Lead with the answer." }
  ],
  "mcp_servers": [
    { "name": "app", "url": "https://app.example.com/mcp", "auth": "agent_token" }
  ]
}
```

`model` is a `[models]` alias from `server.toml`. Changing any field means a new `version`, which costs one prompt cache miss on the next turn. Optional knobs (`effort`, `max_model_calls`, `history_budget` and more) are in [Protocol 1.3](../protocol/#13-get-basebootstrap).

## Turn: this session, right now

newt sends the events the turn answers and the state you returned last time:

```json
{
  "session": "ana",
  "events": [{ "id": "m1", "meta": { "channel": "whatsapp" } }],
  "state": { "last_seen_task": 12 }
}
```

`events` lists each event's `id` and the `meta` your app posted with it, oldest first. Events that join the turn later as steering are not listed. `state` is `null` before the first turn.

You answer with the session's pages and this turn's guidance:

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

- `pinned` is session-stable text, such as the writing guide for the person's channel. It sits in the cached prefix.
- `instructions` and `constraints` reach the model at the start of this turn only and never enter history.
- `state` (optional) replaces the session state. See [Session state](#session-state).
- `turn_state` (optional) is the value the turn's tool calls start with, for facts only this call knows, such as the channel to reply on. See [Turn state](../tools/#turn-state).
- `model`, `effort`, `first_call_effort` and `reset` (optional) change how this one turn runs. See [Pick the model for one turn](#pick-the-model-for-one-turn).

:::caution
Constraints are text the model reads. They do not block a tool call. Enforce any rule that must hold in your tool handlers too.
:::

## Pages: the workspace

A page is one file the agent can read with `read_file`, `glob` and `grep`.

| Field | Meaning |
|---|---|
| `key` | Invalidation key, such as `["customer", "profile"]`. Tools mark pages stale by key prefix. |
| `path` | What the agent sees, such as `profile.md`. Unique per response. |
| `version` | Opaque. newt tells tools which version the agent read. |
| `content` | The text. |

Pages are rebuilt from your app, never edited by newt. When a tool marks a page stale, the next read calls `POST {base}/pages` with that key, and a page missing from the answer is gone.

**Slow pages?** Leave `pages` out of the turn response. newt starts the first model call at once and loads every page in the background. The first read waits for that load.

## Pick the model for one turn

A `/turn` response can override the bootstrap for this turn only:

```json title="/turn response"
{
  "version": "2026-11-12.1",
  "model": "strong",
  "effort": "high",
  "first_call_effort": "medium"
}
```

- `model` is a `[models]` alias from `server.toml`, resolved through its routes like the bootstrap's. An alias that does not exist fails the turn before anything reaches the model, and the turn retries with backoff.
- `model`, `effort` and `first_call_effort` apply to every model call of this turn and to the compaction right after it. The next turn starts from the bootstrap again.
- A different model is a prompt cache miss for this turn and for the next one on the usual model, so keep it for turns that are worth it.

### Start the conversation over

`"reset": true` runs the turn as if the session had no history. The model sees no earlier messages and no earlier checkpoint; the turn's events and its own messages start the new conversation. The session keeps its key, inbox, state and live feed. Use it when a conversation has gone cold, or when your prompt has moved on and the old history no longer fits it.

- Nothing is deleted. [History and fork](../replay/) still see every entry; they are only never loaded into the prompt again.
- The reset is saved by the statement that commits the turn. A turn that does not commit resets nothing, and its retry asks `/turn` again.
- Once a turn has journaled a model call, a resume after a crash keeps the model, efforts and reset it chose, whatever `/turn` answers the second time.

The live feed's `turn_started` frame carries the turn's `model` and `reset`.

## Session state

newt stores one JSON value per session for your provider and hands it back on every call. Use it for what the agent saw last time, so this turn can say what changed; a cursor into your own records; a note that something was already announced.

```json title="/turn response"
{
  "version": "2026-11-12.1",
  "instructions": "New since the last turn: task #13, Renew passport.",
  "state": { "last_seen_task": 13 }
}
```

- **`/turn` sets it.** newt saves the returned `state` in the same transaction that commits the turn and sends it back on the next one. A turn that does not commit saves nothing, so the state never runs ahead of the session's history. Absent or `null` keeps the previous state; `{}` resets it.
- **Pages and tools patch it.** A `/pages` response can carry `state_patch`, and a tool result `newt/state_patch`: a JSON merge patch ([RFC 7396](https://www.rfc-editor.org/rfc/rfc7396)) that touches only the keys it names, so a page load and a tool call running at the same time do not overwrite each other.
- **Everyone can read it.** Every `/pages` request carries `state`, and every tool call `newt/state`, with the turn's patches applied so far.

```json title="/pages response"
{
  "pages": [{ "key": ["tasks"], "path": "tasks.md", "version": "8", "content": "# Tasks\n..." }],
  "state_patch": { "tasks_rendered": "8" }
}
```

:::caution
A patch is lost if the turn does not commit, if it arrives after the commit, or if the turn resumes after a crash (replayed results do not patch again). Keep in the state what you can recompute, and keep the record of anything that must not happen twice in your own data.
:::

The state travels with every page fetch and every tool call, to every MCP server of the agent. Keep it to a few kilobytes, and keep out of it anything one of those servers should not read.

## Hear when a turn ends

Set `"turn_end": true` in the bootstrap and newt calls `POST {base}/turn_end` after every committed turn. Use it for bookkeeping that follows the agent: mark messages handled, stop a typing indicator, log the run, bill usage.

```json
{
  "agent": "assistant",
  "session": "ana",
  "turn": "0198f2a4-6b1e-7c3d-9a10-5e7d2c8b4f01",
  "outcome": "completed",
  "events": [{ "id": "m1", "meta": { "channel": "whatsapp" } }],
  "state": { "last_seen_task": 13 },
  "turn_state": { "sent": true },
  "reply": "Done: the bank call is on your list for tomorrow at 9."
}
```

`reply` is what the agent said when it finished: the text of the message that called `end_turn`, or of the text-only message just before it. Post it to a chat or a thread from here, and the agent needs no send tool. It is `null` when there is no such text, and always for `failed` and `dead_lettered`.

| `outcome` | When |
|---|---|
| `completed` | The turn ended normally. |
| `failed` | The turn failed after it had effects, and was committed with what it did. |
| `dead_lettered` | newt gave up on the events after `max_attempts` failed turns with no effects. |

`events` lists every event the turn consumed, steered ones included. The callback is written in the same transaction as the turn and delivered at least once: right away, then retried with backoff for about two hours until you answer 2xx. Deduplicate by `turn`, and expect callbacks to arrive late or out of order.

## Rules newt enforces

- Each request times out after 10 seconds. newt does not retry inside a request.
- Decoding is strict: unknown fields, an unknown model alias or a version mismatch fail the turn loudly.
- A failed call stops the turn from starting. The events stay pending and the session retries with backoff (2 s doubling, up to 300 s).

## Next

- [Tools over MCP](../tools/): the other half your app serves.
- [Protocol, section 1](../protocol/#1-context-provider): every field and rule, including [`/turn_end`](../protocol/#18-post-baseturn_end).
