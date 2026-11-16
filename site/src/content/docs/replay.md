---
title: Replay and fork
description: Copy a real conversation into a new session, then replay it against a new prompt or model and compare each turn.
---

:::note[Experimental]
Replay and fork work and are tested, but their shape may change within 1.x.
:::

## Fork a session

`POST /v1/agents/{agent}/sessions/{key}/fork` starts a new session with a copy of another's history, up to a turn, under the same or another agent.

```json
{ "into": "ana-try", "agent": "assistant-canary", "through_turn": "0198f2a4-..." }
```

The fork gets history and checkpoints up to the cut, and nothing else: no pending events, journal or live feed. The next event sent to it starts a turn on that history, with the target agent's prompt, tools and model.

## Replay a conversation

```sh
newt-replay assistant/ana assistant-canary/ana-replay
```

`newt-replay` sends each turn's events from `assistant/ana` to the new session, one turn at a time, waits for the turn to finish, and prints what the turn did then and what it did now.

| Flag | Effect |
|---|---|
| `--from-turn N` | Fork the first N turns in instead of replaying them |
| `--turns N` | Stop after N turns |
| `--url` | The newt server |
| `--token-env` | The variable holding the token (default `NEWT_TOKEN`) |

:::caution
A replay runs the target agent's tools for real. Give the target agent an MCP server that is safe to call: a staging app, or `newt-loadtest mock-app`.
:::

Point the target agent at the `newt-loadtest fake-model` route to replay for free, or at a real model to compare prompts or models.

## Next

- [Protocol 5.4 and 5.5](../protocol/#54-get-history).
- [Load testing](../load-testing/) for the fake model and mock app.
