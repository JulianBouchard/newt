---
title: Load testing
description: Run load against newt with a fake model and a mock app, so you measure newt and pay for no tokens.
---

`newt-loadtest` runs load against newt with no real model behind it. It has three parts.

| Part | What it does |
|---|---|
| `fake-model` | Speaks the OpenAI Responses stream with random text. Timings and tool use come from a production assistant's profile: about 16 tool calls per turn. Prompt caching is simulated per prefix, so a rewritten history misses as it would against a real model. |
| `mock-app` | A context provider with a production-sized prompt and workspace, and an MCP server whose tools have that assistant's latencies and error rates. |
| `run` | Drives sessions against the events API and live feed, and reports read receipt, time to turn start, turn duration, model calls, cache rate and tool calls. |

## Run it

```sh
newt-loadtest fake-model --speed 1 &   # --speed 10 compresses every delay tenfold
newt-loadtest mock-app --speed 1 &
LOADTEST_TOKEN=... newt-loadtest run --base http://127.0.0.1:7878 --agent assistant \
  --sessions 200 --turns 5 --concurrency 200 --think-ms 5000 \
  --fake http://127.0.0.1:9100 --mock http://127.0.0.1:9200
```

Point a route's `base_url` at `http://127.0.0.1:9100/v1` and the agent's `context_provider` at `http://127.0.0.1:9200/agent`.

## The fake model's cache

A request reuses the longest run of 128-token blocks that an earlier request with the same `prompt_cache_key` wrote: at least 1,024 tokens, readable 2 seconds after the write, with a five-minute expiry.

## One command

`scripts/bench.sh` does all of this in one command and adds newt's CPU, memory and cache report card. See [Benchmark](../benchmark/) for the method and results.
