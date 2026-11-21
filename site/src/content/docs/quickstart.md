---
title: Quickstart
description: Run the newt demo assistant with Docker and no API key, send it a message, and watch the turn live.
---

`newt-demo` is a small assistant that keeps a person's notes and tasks and replies by chat. It is one MCP server, and that server is the whole agent. You need Docker, and nothing else: a fake model answers instead of a real one.

## 1. Start everything

```sh
docker compose -f deploy/compose.yaml up --build -d
```

This runs Postgres, the demo agent, the fake model from `newt-loadtest`, and newt on `localhost:7878`.

## 2. Send a message

```sh
curl -X POST localhost:7878/v1/agents/assistant/sessions/ana/events \
  -H "Authorization: Bearer demo-token" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow."}'
```

`assistant` is the agent and `ana` is the session: one conversation with one person. newt answers `202` in a few milliseconds and runs the turn in the background.

## 3. Watch the turn

```sh
curl -N localhost:7878/v1/agents/assistant/sessions/ana/live \
  -H "Authorization: Bearer demo-token"
```

The live feed is server-sent events: `received`, `turn_started`, a `tool_started` and `tool_finished` per tool call, then `turn_completed`. Frames carry names, counts and timings, never message text.

## What just happened

1. newt stored the event, woke the session and claimed it.
2. It loaded the agent's instructions, settings, tools and the person's pages (notes and tasks) from the demo's MCP server, and asked it with `newt/turn` for this turn's guidance: the time.
3. The model read the workspace, added the task through a tool, sent a reply, and ended the turn.
4. newt committed the turn: the write and the reply stayed in history, the reads were dropped.

[How a turn works](../how-a-turn-works/) covers each step.

## What the demo is made of

The demo is a [home server](../home-server/): its instructions, its settings in the `newt` extension, its tools, the person's notes and tasks as resources, a skill, and an answer to `newt/turn`. It also shows the experimental flows: `clear_tasks` asks the person to approve before it deletes anything (MCP `input_required`), and `weekly_review` runs as an MCP task that newt polls until it is done.

:::tip[Use a real model]
Set `OPENAI_API_KEY` and point `[models.default]` in `deploy/server.toml` at the `openai` route.
:::

## From source

newt runs its own Postgres in `.newt/postgres`, downloading the binaries once, so there is no database to set up.

```sh
export ASSISTANT_TOKEN=$(openssl rand -hex 16)

# terminal 1: the agent, an MCP server at localhost:4000/mcp
cargo run -p newt-demo

# terminal 2: newt, with the route keys server.example.toml names
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=...
NEWT_CONFIG=server.example.toml cargo run -p newt-server
```

Talk to it, watch the turn, and read what it sent:

```sh
curl -X POST localhost:7878/v1/agents/assistant/sessions/ana/events \
  -H "Authorization: Bearer $ASSISTANT_TOKEN" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow. Also, I moved to Lisbon."}'

curl -N localhost:7878/v1/agents/assistant/sessions/ana/live -H "Authorization: Bearer $ASSISTANT_TOKEN"

curl localhost:4000/outbox/ana -H "Authorization: Bearer $ASSISTANT_TOKEN"
```

To skip terminal 1, let newt run the demo itself: `cargo build -p newt-demo`, then set `mcp = "stdio:target/debug/newt-demo --stdio"` for the agent in `server.example.toml`. newt starts it, talks to it over stdin and stdout, and stops it on exit. The code is the same.

`newt-report newt-events.jsonl` breaks each turn down by model call. On a live run against `gpt-6.1-terra`, each turn took 6 to 8 seconds, of which newt itself spent about 20 ms.

## Next

- Copy `crates/newt-demo` to start your own agent.
- [The home server](../home-server/): what your agent serves.
- [Tools over MCP](../tools/): how a tool says what it leaves behind.
