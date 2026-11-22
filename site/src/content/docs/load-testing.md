---
sidebar:
  label: "Load testing"
title: "Load test without paying for tokens"
description: Load test newt with a fake model and a fake agent, so you measure newt and don't pay for tokens.
---

`newt-loadtest` puts load on newt without a real model behind it. It has three parts:

| Part | What it does |
|---|---|
| `fake-model` | Pretends to be OpenAI's streaming API, with random text. Its timing and tool use copy a real production assistant, about 16 tool calls per turn. It also fakes the prompt cache, so a rewritten history misses just like it would for real. |
| `mock-app` | A fake agent core with a production-sized prompt, pages and skills, and tools as slow and flaky as that assistant's. `--stdio true` lets newt run it itself. |
| `run` | Sends sessions at newt and reports how fast events are picked up, how long turns take, model calls, cache rate and tool calls. |

## Run it

```sh
newt-loadtest fake-model --speed 1 &   # --speed 10 makes every delay ten times shorter
newt-loadtest mock-app --speed 1 &
LOADTEST_TOKEN=... newt-loadtest run --base http://127.0.0.1:7878 --agent assistant \
  --sessions 200 --turns 5 --concurrency 200 --think-ms 5000 \
  --fake http://127.0.0.1:9100 --mock http://127.0.0.1:9200
```

Point a route's `base_url` at `http://127.0.0.1:9100/v1` and the agent's `mcp` at `http://127.0.0.1:9200/mcp`, or at `stdio:newt-loadtest mock-app --stdio true`.

The fake cache works like the real ones: a request reuses the longest run of 128-token blocks an earlier request with the same `prompt_cache_key` wrote, at least 1,024 tokens, readable 2 seconds after the write, gone after five minutes.

`scripts/bench.sh` does all of this in one go and adds newt's CPU, memory and cache report card. The method and results are on [Benchmark](../benchmark/).
