---
title: What is newt
description: An agent runtime for products. One long-lived agent per customer, your backend as its world, and a database's discipline about context.
---

newt runs one long-lived agent per customer of your product. Your app sends it events ("the person wrote", "the order shipped"), and newt runs a turn: it reads your app's state, calls your tools, and acts. A conversation can last for months without its prompt rotting.

It is one Rust binary plus Postgres.

## What you bring, what newt does

| You bring | newt does |
|---|---|
| A **context provider**: three HTTP endpoints that return instructions and the customer's state as text pages | Builds every prompt, keeps it cacheable, refetches pages when they change |
| **Tools** on an MCP server, each tagged with what it leaves behind | Runs the turn loop, decides what reaches history, sends outward calls exactly once |
| **Events**, posted when something happens | Queues, steers and schedules turns across any number of instances |
| A **model route**: Anthropic, OpenAI, or any Chat Completions host | Streams, retries, falls back along a chain, and reports every cache miss |

You can skip the context provider: an agent can be [nothing but an MCP server](../mcp-agents/).

## How the pieces connect

```text
              events (POST)                        live feed (SSE)
  your app ----------------------+   +--------------------------> your app
                                 v   |
                         +---------------------+
                         |   newt-server       |
                         |   event API         |
                         |   runner: leases,   |
                         |   sweeps, turn loop |
                         +---------------------+
                           |     |       |     |
         model calls       |     |       |     |   state, journal, wakeups
        +------------------+     |       |     +----------------------+
        v                        v       v                            v
  Model providers      Context provider  MCP servers             Postgres
  (Anthropic, OpenAI)  (bootstrap, turn, (tools with              (inbox, history,
                        pages)            newt _meta)              journal, leases)
```

## Why the context stays clean

Most runtimes let the prompt grow and hope. That has two costs: stale tool results sit in the prompt and the model acts on them, and any edit to earlier text throws away the provider's prompt cache. newt treats the prompt like a database treats a log:

- **Append, never rewrite.** Each turn's prompt is the previous one plus a suffix, so the cache covers everything before it.
- **Keep what was done, not what was seen.** Reads are dropped at the end of a turn. The agent reads your current state again next time.
- **Rebuild from a source of truth.** Your app owns the workspace pages. When a tool changes something, the page goes stale and the next read fetches it fresh.

[How a turn works](../how-a-turn-works/) walks through one turn. [Design](../design/) has the full reasoning, with a pointer to the code for each claim.

## When newt fits

newt fits a product where each customer has one agent that talks to them, or acts for them, over weeks: an assistant, a concierge, an account manager, a support agent with memory.

It leaves out, on purpose:

- memory shared across sessions;
- sandboxed code execution;
- multi-agent orchestration;
- a scheduler for timers. Your app sends the event when it is time.

## Status

newt is stable from 1.0, and the [changelog](../changelog/) lists what each release added. Each release ships `newt` binaries for Linux (x86_64, arm64) and macOS (arm64) and a Docker image at `ghcr.io/julianbouchard/newt`. Building from source needs Rust 1.99. newt runs its own Postgres 18 or connects to yours.

| Maturity | What |
|---|---|
| Stable | The turn loop and retention classes, steering, the crash-safe journal, fenced leases and several instances on one Postgres, the events API and live feed, deleting sessions and pruning old rows, the context provider, MCP tools, approvals through MCP `input_required`, MCP tasks, agents made of one MCP server, compaction, prompt cache keys, OTLP tracing, embedded Postgres |
| Experimental | Skills over MCP (SEP-2640, a draft spec), replay and fork, the `openai-chat` protocol, the turn token budget, recitation (`recite` in the bootstrap, off by default) |

Experimental features work and are tested, including end to end against `newt-demo`, but the specs they follow or the shape of their settings may still change. [Stability](../stability/) lists what 1.x promises.

## Next

- [Quickstart](../quickstart/): run the demo assistant with Docker and no API key.
- [How a turn works](../how-a-turn-works/): the loop, step by step.
- [Context provider](../context-provider/): the three endpoints your app serves.
