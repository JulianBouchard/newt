---
title: "Write tools"
description: Tag your tools with what they leave in history, which pages they change and whether they reach outside your system. Plus what newt sends with every call, what a result can send back, and the built-in tools.
---

The agent's tools come from its [agent core](../agent-core/) and any [extra servers](../extra-servers/). Any MCP server works as it is. If the server declares the `newt` extension, it can also tell newt a bit more about each tool, and that's what keeps long sessions clean.

newt talks MCP `2026-07-28`, falls back to `2025-11-25`, and keeps one connection per server for every session of the agent.

## Say what a tool leaves in history

Add up to three fields to a tool's `_meta` in `tools/list`:

```json
{
  "name": "send_message",
  "description": "Send a chat message to the person.",
  "_meta": { "newt/retention": "outward", "newt/idempotent": true }
}
```

- `newt/retention` says what history keeps of the call:
  - `ephemeral` for reads and lookups. Nothing is kept.
  - `durable` for writes to your own system. The call is kept with a short summary. This is the default.
  - `outward` for anything that reaches outside your system, like a message, an email or a payment. Kept like `durable`, and sent exactly once.
- `newt/invalidates` lists the page keys every successful call changes.
- `newt/idempotent` says your handler does nothing new when it sees a key twice. It has to be `true` on an `outward` tool, or newt won't load the tools at all. On a `durable` tool it's optional: if newt crashes while the tool runs, newt runs it again with the same key. Without it, newt tells the model the call was interrupted and may or may not have taken effect, and the model checks before it tries again.

[How newt works](../design/#what-a-turn-keeps-in-history) explains why history works this way. Tools from a plain MCP server are all `durable` and change no pages.

## Mark pages as changed

If you've used React Query, you already know this. A mutation invalidates the keys it touched, and the next read fetches fresh data. A newt tool does the same with pages.

```ts title="React Query"
useMutation({
  mutationFn: completeTask,
  onSuccess: () => queryClient.invalidateQueries({ queryKey: ["tasks"] }),
});
```

```json title="newt tool"
{
  "name": "complete_task",
  "_meta": { "newt/retention": "durable", "newt/invalidates": [["tasks"]] }
}
```

After `complete_task` succeeds, every page whose key starts with `["tasks"]` is stale, and the agent's next read of `tasks.md` gets the new list from your server.

## Send messages and payments only once

A turn can send a message and crash before it commits. When newt picks the turn up again, it calls the tool again with the same key (see [How newt works](../design/#how-a-message-never-gets-sent-twice)). Your handler makes sure the message only goes out once:

1. Read `newt/idempotency_key` from the call.
2. Store it in the same transaction as the effect.
3. If you've seen the key before, return the first result and do nothing else.

The reference handler is `send_message` in `crates/newt-demo/src/tools.rs`.

## What newt sends with every call

Servers with the `newt` extension get all of these in `params._meta`. Plain MCP servers only get the idempotency key and `traceparent`.

| Field | What you can do with it |
|---|---|
| `newt/session` | Know which customer this is |
| `newt/idempotency_key` | Make an effect happen once |
| `newt/pages_read` | Refuse to act on a page that changed since the agent read it |
| `newt/skills_loaded` | Refuse an action until the agent read the right [skill](../skills/) |
| `newt/state` | Read the [session state](../agent-core/#remember-things-between-turns) |
| `newt/turn_state` | Read the [turn state](#share-state-between-tools-in-one-turn) |
| `traceparent` | Put your own spans under the turn's trace |

## What a result can send back

Only results from servers with the `newt` extension, in the result's `_meta`:

| Field | What it does |
|---|---|
| `newt/summary` | What history keeps instead of the full result |
| `newt/invalidate` | More page keys this call changed |
| `newt/pages` | The fresh pages themselves, so newt doesn't have to fetch them |
| `newt/state_patch` | Changes to the [session state](../agent-core/#remember-things-between-turns) |
| `newt/turn_state` | A new [turn state](#share-state-between-tools-in-one-turn) for the rest of the turn |

A tool can only mark its own server's pages stale, and only pages newt already has. So a tool that creates a page should hand it back in `newt/pages`. [Protocol 2.4](../protocol/#24-result-_meta) has the details, like what happens on an error result.

## Share state between tools in one turn

Turn state is one JSON value that the tools of a single turn share. It starts as whatever `newt/turn` returned as `turn_state`, or empty. Each call sees it, and a result that returns a new one replaces it for every call after.

```json title="send_message result"
{
  "content": [{ "type": "text", "text": "Sent." }],
  "_meta": { "newt/turn_state": { "channel": "whatsapp", "sent": true } }
}
```

So the next `send_message` in the same turn sees `"sent": true` and can refuse to send a second message. It's gone when the turn ends (your agent core gets the last value in `newt/turn_end`). Anything that has to last longer goes in the session state or your own data.

## Slow tools, asking first, and long jobs

- **Slow tools.** A call can go 90 seconds without a result or a progress update, and 10 minutes in total. After that the model is told the outcome is unknown and to check the pages before trying again. Send MCP progress notifications to keep a long call alive, and they show up on the [live feed](../events/#show-progress-in-your-app) too.
- **Ask first.** A tool can answer with an MCP `input_required` question. The model asks the person, then calls the tool again with their answer.
- **Long jobs.** A tool can answer with an MCP task, and newt checks on it for up to an hour.

## Tools every agent gets

newt gives every agent a few tools of its own, so your servers can't use these names unless you turn that tool off:

| Tool | What it's for |
|---|---|
| `read_file`, `glob`, `grep` | Reading pages |
| `remember` | Saving one fact for good, like "Lives in Lisbon". It's kept in history like a `durable` call. |
| `load_skill` | Loading a [skill](../skills/) |
| `batch` | Running several tool calls at once |
| `end_turn` | Ending the turn |
| `checkpoint` | Writing the checkpoint during [compaction](../design/#what-happens-when-history-gets-long) |

Every tool the model sees is one more thing it can pick, so turn off the ones your agent doesn't need. `builtin_tools` in your agent core's `newt` settings is `false` for none, or a list of the ones to keep:

```json
{ "model": "default", "builtin_tools": ["end_turn"] }
```

A reply already ends the turn, so an agent that only answers and calls its own tools can do without all of them. Compaction still gets `checkpoint` when it runs. Turning off `load_skill` hides your skills too.

newt doesn't check arguments against your `inputSchema`, so validate them in the handler.

## See also

- [Add more MCP servers](../extra-servers/): tools from servers you don't run.
- [Protocol, sections 2 to 4](../protocol/#2-servers-and-tools): every field and rule.
