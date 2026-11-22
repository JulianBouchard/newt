---
title: "See what each turn did"
description: See what every turn did. Locally from a JSONL file with newt-report, or in production as one OTLP trace per turn in Langfuse or any collector.
---

Every turn emits one stream of events, and newt can write it to a local file, to the live feed, and out as OTLP traces.

## See why the cache missed: `newt-report`

```toml
[events]
jsonl = "newt-events.jsonl"
capture_content = false
```

Leave `capture_content` off and the file has no message text. Run `newt-report newt-events.jsonl` to get timings per turn and a **cache report card**: how much of all input came from the cache, and for every call that read less than half from it, what broke it.

| What broke the cache | Example |
|---|---|
| The session's first call | Expected, once |
| Something in the fixed part changed | `tool \`search\` changed, about 6120 bytes into the prefix` |
| History was rewritten from message n | Usually compaction |
| A long pause | The person came back after the cache expired |
| Routing | Nothing changed and no pause, the provider just sent it to a machine without the cache |

Add `--prices input,cached,output` to see what it cost.

## Send traces to Langfuse or any OTLP collector

```toml
[tracing]
endpoint = "https://cloud.langfuse.com/api/public/otel/v1/traces"
profile = "langfuse"
headers_env = "NEWT_OTLP_HEADERS"
capture_content = true
```

newt sends one trace per turn: an `invoke_agent` span for the turn, a `chat` span per model call (tokens, timings and the new messages), and an `execute_tool` span per tool call. A session's turns share `gen_ai.conversation.id`, and the `langfuse` profile also fills in Langfuse's session and usage fields. Sending the same turn twice overwrites it instead of duplicating it.

:::caution[Message text is exported by default]
Unlike the local file, `[tracing]` sends message text unless you set `capture_content = false`. Then it only sends timings, token counts and tool names, hashes the session key, and replaces error text with `failed`.
:::

## Add your own spans

Every tool call carries a `traceparent`, so your servers can put their own spans under the turn's trace. If you use newt as a library, an `Enricher` adds attributes per session and a `Redactor` runs over every captured message before it leaves the process.

## Next

- [Configuration: `[tracing]`](../configuration/#tracing) and [`[events]`](../configuration/#events).
