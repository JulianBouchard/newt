---
title: Tools over MCP
description: Serve tools from MCP servers and tag each one with what it leaves in history, which pages it changes, and whether it acts outside your system.
---

Tools come from the MCP servers your bootstrap lists. Any MCP server works as is. A few `_meta` fields let a tool tell newt what it leaves behind, and that is what keeps long sessions clean.

```json
{ "name": "app", "url": "https://app.example.com/mcp", "auth": "agent_token" }
```

`auth: "agent_token"` sends the agent's token as the bearer. The default sends none. `headers` are static and must not hold secrets. newt speaks Streamable HTTP, MCP `2026-07-28` with a fallback to `2025-11-25`, and shares one connection per server across every session of a bootstrap version.

## Tag each tool

Three optional fields on the tool, in `tools/list`:

| Field | Meaning |
|---|---|
| `newt/retention` | `ephemeral`, `durable` (default) or `outward`. What reaches history. |
| `newt/invalidates` | Page key prefixes that every successful call makes stale. |
| `newt/idempotent` | Must be `true` on an `outward` tool, or newt refuses to load the tool set. |

```json
{
  "name": "complete_task",
  "description": "Mark a task done by its number.",
  "inputSchema": { "type": "object", "properties": { "id": { "type": "integer" } }, "required": ["id"] },
  "_meta": { "newt/retention": "durable", "newt/invalidates": [["tasks"]] }
}
```

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

| Field in `params._meta` | Use it to |
|---|---|
| `newt/session` | Know which customer this is |
| `newt/idempotency_key` | Deduplicate effects |
| `newt/skills_loaded` | Refuse an action until the right [skill](../skills/) was read |
| `newt/pages_read` | Refuse to act on a page that changed since the agent read it |
| `traceparent` | Parent your own spans under the turn's trace |

## What a result can send back

| Field in the result's `_meta` | Effect |
|---|---|
| `newt/summary` | What history keeps instead of the full result |
| `newt/invalidate` | More page key prefixes to mark stale |
| `newt/pages` | Fresh page data, used without a refetch |

Invalidation only reaches pages newt already has, so a tool that creates a page returns it in `newt/pages`. A result with malformed newt metadata marks every page stale.

## Timeouts and unknown outcomes

A call may go 90 seconds without a result or a progress notification, and 10 minutes in total. Past either, or on a broken connection, the model is told the outcome is unknown and to check the workspace before trying again. Report `notifications/progress` to keep a long call alive; each one also reaches the live feed as `tool_progress`.

## Approvals and long-running tools

- **Ask first.** A tool that answers MCP `input_required` with an elicitation becomes a question to the person. The model asks, then calls the tool again with the answers. Nothing is stored in between.
- **Run as a task.** A call the server answers with an MCP task is polled for up to an hour.

## Names newt keeps

Your tools cannot use the built-in names: `read_file`, `glob`, `grep`, `end_turn`, `remember`, `batch`, `checkpoint`, `load_skill`. newt does not validate arguments against `inputSchema`; validate in the handler.

## Next

- [Protocol, sections 2 to 4](../protocol/#2-mcp-tools): every field and rule.
- [An agent that is only an MCP server](../mcp-agents/).
