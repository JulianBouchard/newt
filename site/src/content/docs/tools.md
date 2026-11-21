---
title: Tools over MCP
description: Serve tools from MCP servers, tag each one with what it leaves in history, which pages it changes and whether it acts outside your system, and share state between calls.
---

Tools come from the agent's MCP servers: the [home](../home-server/) and any [servers it adds](../servers/). Any MCP server works as is. A server that declares the `newt` extension can also use a few `_meta` fields to tell newt what each tool leaves behind, and that is what keeps long sessions clean.

newt speaks MCP `2026-07-28` with a fallback to `2025-11-25`, over Streamable HTTP or stdio, and shares one connection per server across every session of the agent.

## Tag each tool

Three optional fields on the tool, in `tools/list`. newt reads them only from a server that declares the `newt` extension; any other server's tools are `durable` and invalidate nothing.

| Field | Meaning |
|---|---|
| `newt/retention` | `ephemeral`, `durable` (default) or `outward`. What reaches history. |
| `newt/invalidates` | Page key prefixes that every successful call makes stale. |
| `newt/idempotent` | Must be `true` on an `outward` tool, or newt refuses to load the tool set. |

## Invalidation works like React Query

If you have used React Query, you know the model: a mutation invalidates the keys it touched, and the next read fetches fresh data. A newt tool does the same with workspace pages.

```ts title="React Query"
useMutation({
  mutationFn: completeTask,
  onSuccess: () => queryClient.invalidateQueries({ queryKey: ["tasks"] }),
});
```

```json title="A newt tool"
{
  "name": "complete_task",
  "_meta": { "newt/retention": "durable", "newt/invalidates": [["tasks"]] }
}
```

After `complete_task` succeeds, every page whose key starts with `["tasks"]` is stale. The agent's next read of `tasks.md` reads it again from your server.

## Pick a retention class

| Class | Use it for | History keeps |
|---|---|---|
| `ephemeral` | Reads and lookups | Nothing |
| `durable` | Writes to your own system | The call and its `newt/summary` (or the result cut to 500 characters) |
| `outward` | Messages, emails, payments: effects outside your system | Same as `durable`, and the call is journaled before it runs |

A server that ignores these fields still works: its tools count as `durable` and invalidate nothing.

## Make outward calls exactly once

A turn can send a message and crash before it commits. newt makes sure the retry carries the same key; your handler makes sure the effect happens once.

1. newt writes an intent entry to Postgres and waits for it before the call. If that write fails, the call is not made.
2. Every call carries `newt/idempotency_key`, stable across retries and replays.
3. Your handler stores the key in the same transaction as the effect. On a repeated key it returns the original result and does nothing else.

Outward calls run after the other calls of a model step, one at a time. If one fails, the later ones in that step are skipped. The reference handler is `send_message` in `crates/newt-demo/src/tools.rs`.

## What newt sends with every call

A server that speaks newt gets all of these. Any other server gets only `newt/idempotency_key` and `traceparent`.

| Field in `params._meta` | Use it to |
|---|---|
| `newt/session` | Know which customer this is |
| `newt/idempotency_key` | Deduplicate effects |
| `newt/skills_loaded` | Refuse an action until the right [skill](../skills/) was read |
| `newt/pages_read` | Refuse to act on a page that changed since the agent read it |
| `newt/state` | Read the [session state](../home-server/#session-state) as the turn holds it |
| `newt/turn_state` | Read the [turn state](#turn-state) |
| `traceparent` | Parent your own spans under the turn's trace |

## What a result can send back

newt applies these only from a server that speaks newt.

| Field in the result's `_meta` | Effect |
|---|---|
| `newt/summary` | What history keeps instead of the full result |
| `newt/invalidate` | More page key prefixes to mark stale |
| `newt/pages` | Fresh page data, used without a refetch |
| `newt/state_patch` | A JSON merge patch for the [session state](../home-server/#session-state), saved when the turn commits |
| `newt/turn_state` | Replaces the [turn state](#turn-state) for later calls in this turn |

Invalidation only reaches pages newt already has, and only the calling server's own pages, so a tool that creates a page returns it in `newt/pages`. A result with a malformed `newt/summary`, `newt/invalidate` or `newt/pages` marks all of that server's pages stale. Error results apply only `newt/state_patch` and `newt/turn_state`.

## Turn state

Turn state is one JSON value that the tools of a single turn share. It starts as the `turn_state` from [`newt/turn`](../home-server/#this-turn-newtturn), or empty. Each call receives it as `newt/turn_state`, and a result that returns `newt/turn_state` replaces it for every later call.

```json title="send_message result"
{
  "content": [{ "type": "text", "text": "Sent." }],
  "_meta": { "newt/turn_state": { "channel": "whatsapp", "sent": true } }
}
```

The next `send_message` in the same turn sees `"sent": true` and can refuse a second message. An error result can set it too, so a tool can count its own refusals.

- Calls that run at the same time (a `batch`) each see the state from before them, and the last result to arrive wins.
- All of an agent's servers that speak newt share one turn state. Other servers never see it.
- A turn that resumes after a crash starts again from `newt/turn`'s value.
- The final turn state reaches your app in [`newt/turn_end`](../home-server/#hear-when-a-turn-ends).

What must outlive the turn belongs in the [session state](../home-server/#session-state) or your own data.

## Timeouts and unknown outcomes

A call may go 90 seconds without a result or a progress notification, and 10 minutes in total. Past either, or on a broken connection, the model is told the outcome is unknown and to check the workspace before trying again. Report `notifications/progress` to keep a long call alive; each one also reaches the live feed as `tool_progress`.

## Approvals and long-running tools

- **Ask first.** A tool that answers MCP `input_required` with an elicitation becomes a question to the person. The model asks, then calls the tool again with the answers. Nothing is stored in between.
- **Run as a task.** A call the server answers with an MCP task is polled for up to an hour.

## Names newt keeps

Your tools cannot use the built-in names: `read_file`, `glob`, `grep`, `end_turn`, `remember`, `batch`, `checkpoint`, `load_skill`. newt does not validate arguments against `inputSchema`; validate in the handler.

## Next

- [Adding servers](../servers/): tools from servers you do not own.
- [Protocol, sections 2 to 4](../protocol/#2-servers-and-tools): every field and rule.
