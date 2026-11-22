---
title: Quickstart
description: Run the newt demo assistant with Docker and no API key, send it a message, and watch the turn live.
---

`newt-demo` is a small assistant that keeps a person's notes and tasks and replies by chat. The whole agent is one MCP server, its [agent core](../agent-core/). All you need is Docker. A fake model answers, so you don't need an API key either.

## 1. Start everything

```sh
docker compose -f deploy/compose.yaml up --build -d
```

That starts Postgres, the demo agent, a fake model and newt on `localhost:7878`.

## 2. Send a message

```sh
curl -X POST localhost:7878/v1/agents/assistant/sessions/ana/events \
  -H "Authorization: Bearer demo-token" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow."}'
```

`assistant` is the agent and `ana` is the session, one conversation with one person. newt answers `202` straight away and runs the turn in the background.

## 3. Watch the turn

```sh
curl -N localhost:7878/v1/agents/assistant/sessions/ana/live \
  -H "Authorization: Bearer demo-token"
```

You'll see the turn start, each tool call start and finish, and the turn complete. [What happens when a message comes in](../how-a-turn-works/) explains what happened at each step.

## What's in the demo

Everything an agent core can serve: instructions, settings, tools, the person's notes and tasks as pages, a skill, and a `newt/turn` answer that tells the model the time. It also shows two experimental flows. `clear_tasks` asks the person before it deletes anything, and `weekly_review` runs as a long MCP task.

:::tip[Use a real model]
Set `OPENAI_API_KEY` and point `[models.default]` in `deploy/server.toml` at the `openai` route.
:::

## Run it without Docker

newt runs its own Postgres in `.newt/postgres` (it downloads it once), so there's no database to set up.

```sh
export ASSISTANT_TOKEN=$(openssl rand -hex 16)

# terminal 1: the agent, an MCP server at localhost:4000/mcp
cargo run -p newt-demo

# terminal 2: newt, with the provider keys server.example.toml uses
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=...
NEWT_CONFIG=server.example.toml cargo run -p newt-server
```

Talk to it, watch the turn, and see what it sent:

```sh
curl -X POST localhost:7878/v1/agents/assistant/sessions/ana/events \
  -H "Authorization: Bearer $ASSISTANT_TOKEN" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow. Also, I moved to Lisbon."}'

curl -N localhost:7878/v1/agents/assistant/sessions/ana/live -H "Authorization: Bearer $ASSISTANT_TOKEN"

curl localhost:4000/outbox/ana -H "Authorization: Bearer $ASSISTANT_TOKEN"
```

Don't want a second terminal? Let newt run the demo itself. Build it with `cargo build -p newt-demo`, then set `mcp = "stdio:target/debug/newt-demo --stdio"` for the agent in `server.example.toml`. Same code, newt just starts and stops it.

On a live run against `gpt-6.1-terra`, each turn took 6 to 8 seconds, and newt itself spent about 20 ms of that. `newt-report newt-events.jsonl` shows you the breakdown ([See what each turn did](../tracing/)).

## Next

- Copy `crates/newt-demo` to start your own agent.
- [Build your agent](../agent-core/): what your agent serves.
- [Write tools](../tools/): how a tool says what it leaves behind.
