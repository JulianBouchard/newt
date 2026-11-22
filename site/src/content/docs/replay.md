---
sidebar:
  label: "Replay a conversation"
title: "Re-run a real conversation"
description: Copy a real conversation into a new session, then run it again against a new prompt or model and compare each turn.
---

:::note[Experimental]
Replay and fork work and are tested, but how you call them might still change within 2.x.
:::

## Copy a conversation

`POST /v1/agents/{agent}/sessions/{key}/fork` starts a new session with a copy of another one's history, up to a turn you pick, under the same agent or another one.

```json
{ "into": "ana-try", "agent": "assistant-canary", "through_turn": "0198f2a4-..." }
```

The fork only gets the history. The next event you send it starts a turn on that history, with the other agent's prompt, tools and model.

## Run it again on a new prompt or model

```sh
newt-replay assistant/ana assistant-canary/ana-replay
```

`newt-replay` sends each of the real conversation's turns to the new session, one at a time, and prints what the turn did back then next to what it does now. `--from-turn N` copies the first N turns over instead of replaying them, and `--turns N` stops after N.

:::caution
A replay calls the other agent's tools for real. Point it at a server that's safe to call, like a staging app or `newt-loadtest mock-app`.
:::

Point the other agent at the fake model from [Load test without paying for tokens](../load-testing/) to replay for free, or at a real model to compare prompts or models.

## Next

- [Protocol 5.4 and 5.5](../protocol/#54-get-history).
