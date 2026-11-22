---
title: "What's stable"
description: What's stable, what's experimental, and what newt promises not to break within a major version.
---

newt follows semantic versioning. From 2.1 on, nothing below changes within 2.x in a way that breaks a working setup. 2.1 settled a few of 2.0's defaults and responses; the [changelog](../changelog/) lists what each release changed.

## What's stable and what's experimental

| | |
|---|---|
| Stable | The turn loop and what history keeps, messages joining a running turn, the crash-safe journal, several instances on one Postgres, the event API and live feed, deleting sessions and pruning old rows, the agent core and the `newt` MCP extension, extra servers, stdio, approvals through MCP `input_required`, MCP tasks, compaction, prompt cache keys, OTLP tracing, the built-in Postgres |
| Experimental | Skills over MCP (SEP-2640, a draft spec), replay and fork, the `openai-chat` provider, the per-turn token budget, recitation |

Experimental features work and are tested, including end to end against `newt-demo`. But the specs they follow, or how you configure them, might still change.

Each release ships `newt` binaries for Linux (x86_64) and macOS (arm64), and a Docker image at `ghcr.io/julianbouchard/newt`. Building from source needs Rust 1.99. newt runs its own Postgres 18 or connects to yours.

## What won't break within 2.x

- **The event API**: paths, fields and status codes ([Protocol, section 5](../protocol/#5-event-api-called-by-your-application)).
- **The `newt` MCP extension**: the agent core's settings, `newt/turn`, `newt/turn_end` and the `newt/*` `_meta` fields ([Protocol, sections 1 to 4](../protocol/)).
- **`server.toml`**: keys and defaults ([Configuration](../configuration/)).
- **The database.** newt runs its own migrations on start, and within 2.x they only add tables, columns and indexes. Upgrading means replacing the binary. Downgrading isn't supported.

A minor release can add optional fields, endpoints, live feed events and config keys, so ignore fields you don't know.

## What can still change

- Experimental features. A minor release can change them, and the changelog will say so.
- The Rust crates and their APIs, since they aren't published.
- The JSONL event file and the OTLP span attributes.
