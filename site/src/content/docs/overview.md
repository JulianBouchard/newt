---
title: What is newt
description: An agent runtime for products. One long-lived agent per customer, your backend as its world, and a database's discipline about context.
---

newt runs one long-lived agent per customer of your product. Your app sends it events ("the person wrote", "the order shipped"), and newt runs a turn: it reads your app's state, calls your tools, and acts. A conversation can last for months without its prompt rotting.

It is one Rust binary plus Postgres.

## What you bring, what newt does

| You bring | newt does |
|---|---|
| A **home MCP server**: the agent's instructions, settings, tools, and the customer's state as resources | Builds every prompt, keeps it cacheable, reads pages again when they change |
| **Tools** on that server and on any servers it adds, each tagged with what it leaves behind | Runs the turn loop, decides what reaches history, sends outward calls exactly once |
| **Events**, posted when something happens | Queues, steers and schedules turns across any number of instances |
| A **model route**: Anthropic, OpenAI, or any Chat Completions host | Streams, retries, falls back along a chain, and reports every cache miss |

An agent is [its home server](../home-server/) plus any [servers it adds](../servers/). newt reaches each one over HTTP, or runs it itself over stdio.

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
  Model providers      Home MCP server   Added MCP servers       Postgres
  (Anthropic, OpenAI)  (the agent:       (more tools and          (inbox, history,
                        prompt, tools,    pages)                   journal, leases)
                        pages)
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

newt follows semantic versioning, and the [changelog](../changelog/) lists what each release added. 2.0 replaced the HTTP context provider with the home MCP server. Each release ships `newt` binaries for Linux (x86_64) and macOS (arm64) and a Docker image at `ghcr.io/julianbouchard/newt`. Building from source needs Rust 1.99. newt runs its own Postgres 18 or connects to yours.

| Maturity | What |
|---|---|
| Stable | The turn loop and retention classes, steering, the crash-safe journal, fenced leases and several instances on one Postgres, the events API and live feed, deleting sessions and pruning old rows, the home server and the `newt` MCP extension, servers the home adds, the stdio transport, approvals through MCP `input_required`, MCP tasks, compaction, prompt cache keys, OTLP tracing, embedded Postgres |
| Experimental | Skills over MCP (SEP-2640, a draft spec), replay and fork, the `openai-chat` protocol, the turn token budget, recitation (`recite` in the home's settings, off by default) |

Experimental features work and are tested, including end to end against `newt-demo`, but the specs they follow or the shape of their settings may still change. [Stability](../stability/) lists what each major version promises.

## Next

- [Why is newt](../motivation/): what started it, and the model it is built for.
- [Quickstart](../quickstart/): run the demo assistant with Docker and no API key.
- [How a turn works](../how-a-turn-works/): the loop, step by step.
- [The home server](../home-server/): the MCP server that is your agent.
