---
title: How a turn works
description: One turn from event to commit, the three layers of the prompt, and what survives into history.
---

A **session** is one conversation of one agent, named by a session key such as `ana`. A **turn** is one run of the model loop over the events that arrived for a session. Here is one turn, from the event to the commit.

## The prompt has three layers

Every model call is built from the same three layers, in this order:

```text
frozen prefix   instructions · pinned · skills index · tool list   cached
history         latest checkpoint · committed turns                cached
scratch         turn context · new events · replies · tool calls   new
```

- The **frozen prefix** changes only when your app publishes a new bootstrap version, or the session's `pinned` text changes.
- **History** only grows. A turn appends to it when it commits.
- **Scratch** is what the current turn adds. Most of it is thrown away at the end.

So each turn's prompt is the previous one plus a suffix, and the provider's prefix cache covers everything before the suffix.

## One turn, step by step

1. **An event arrives.** Your app posts it to `POST /v1/agents/{agent}/sessions/{key}/events`. newt stores it (deduplicated by its `id`), answers `202`, and wakes the session with a Postgres `NOTIFY`.
2. **A runner claims the session.** Any newt instance can. It takes a lease, so two instances never run the same session.
3. **newt asks your app for context.** It calls `POST {base}/turn` with the turn's events and the session's state, and gets back the pages and turn guidance. It calls `GET {base}/bootstrap` when the version changed. See [Context provider](../context-provider/).
4. **The model loop runs.** The model reads pages with `read_file`, `glob` and `grep`, and calls your [tools](../tools/). An event that arrives meanwhile joins the turn between model calls (steering).
5. **Tools mark pages stale.** A tool that changes a customer's tasks invalidates the `tasks` page. The next read fetches it fresh.
6. **Outward calls go through the journal.** Before a tool that sends a message or charges a card runs, newt writes an intent entry to Postgres and waits for it. A crash mid-turn replays without sending twice.
7. **The turn commits.** Retention classes decide what moves from scratch into history. Your provider's new session state is saved in the same transaction.
8. **Your app hears about it.** If the bootstrap sets `turn_end`, newt calls `POST {base}/turn_end` with the outcome, the events consumed and the final state.
9. **Compaction, if needed.** When live history passes the budget (60,000 tokens by default), the agent writes a checkpoint. See [Memory and compaction](../memory/).

## What survives a turn

Each tool declares a retention class. That class decides what history keeps.

| Class | What history keeps | For |
|---|---|---|
| `ephemeral` | Nothing: call and result are dropped | Reads and lookups |
| `durable` | The call, with the result cut to its `newt/summary` | Writes to your own system |
| `outward` | Same as `durable`, plus journaled exactly-once handling | Messages, emails, payments |

The model's text replies are kept and its reasoning is dropped. History carries what the agent did, not what it saw: anything it learned from a read, it can read again from the workspace.

## Where to put guidance

Your app has three places to put text, and each has a different cache cost.

| Field | Lives in | Cache cost |
|---|---|---|
| bootstrap `instructions` | Frozen prefix, every session | One miss per new version |
| turn `pinned` | Frozen prefix, this session | Free while unchanged, one miss when it changes |
| turn `instructions`, `constraints` | Scratch, this turn only | Free, never enters history |

## Next

- [Design](../design/) explains why each piece works this way, with pointers to the code.
- [Tools over MCP](../tools/) shows how to tag a tool.
