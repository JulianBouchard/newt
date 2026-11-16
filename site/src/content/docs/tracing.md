---
title: Tracing
description: Read every turn from a local JSONL file with newt-report, or export one OTLP trace per turn to Langfuse or any collector.
---

Every turn emits one typed event stream. Sinks read it: a local JSONL file, the live feed, and OTLP traces.

## Locally: the JSONL file and `newt-report`

```toml
[events]
jsonl = "newt-events.jsonl"
capture_content = false
```

The file leaves out message content unless `capture_content = true`. Run `newt-report newt-events.jsonl` for timing and flags per turn, and a **cache report card**: the cached share of all input and, for every call that read under half from cache, what broke it.

| Miss cause | Example |
|---|---|
| First call of the session | Expected once |
| A changed piece of the prefix | `tool \`search\` changed, about 6120 bytes into the prefix` |
| History rewritten from message n | Usually compaction |
| An idle gap past the cache lifetime | The person came back later |
| Routing | Prefix and history unchanged, no gap: the provider sent it to a machine without the cache |

`--prices input,cached,output` adds the cost.

## In production: OTLP traces

```toml
[tracing]
endpoint = "https://cloud.langfuse.com/api/public/otel/v1/traces"
profile = "langfuse"
headers_env = "NEWT_OTLP_HEADERS"
capture_content = true
```

newt exports one trace per turn over OTLP/HTTP JSON:

- an `invoke_agent` root span;
- a `chat` span per model call, with token usage, timings and the messages new to that call;
- an `execute_tool` span per tool call.

A session's turns share `gen_ai.conversation.id`. The `langfuse` profile also sets the Langfuse session, observation types and usage details. Span ids derive from session, turn and call, so a re-export overwrites rather than duplicates.

:::caution[Content is exported by default]
`[tracing] capture_content` defaults to `true`, unlike `[events]`. Set it to `false` to export only timings, token counts and tool names, with the session key hashed and error text replaced by `failed`.
:::

## Your spans under the turn

Every tool call carries a `traceparent` in its `_meta`. Parent your own spans on it to see them inside the turn's trace. Library users can add an `Enricher` (extra attributes per session) and a `Redactor` (runs over every captured message before it leaves the process).

## Next

- [Configuration: `[tracing]`](../configuration/#tracing) and [`[events]`](../configuration/#events).
