---
sidebar:
  label: "Send events"
title: "Send events from your app"
description: Post events to a session, follow its turns on the live feed, read its history, and delete it.
---

Your app talks to newt over a small HTTP API. The agent and the session key go in the path, and the agent's token goes in `Authorization: Bearer`.

| Method and path | What it does |
|---|---|
| `POST /v1/agents/{agent}/sessions/{key}/events` | Send an event |
| `GET /v1/agents/{agent}/sessions/{key}` | Session status |
| `GET /v1/agents/{agent}/sessions/{key}/live` | Live feed |
| `GET /v1/agents/{agent}/sessions/{key}/history` | What history kept |
| `POST /v1/agents/{agent}/sessions/{key}/fork` | Copy the session (see [Re-run a real conversation](../replay/)) |
| `DELETE /v1/agents/{agent}/sessions/{key}` | Delete the session |
| `GET /healthz` | 200 when Postgres answers, no auth |

A session is one conversation with one person, and the key is whatever you call it, like `ana`. It starts with its first event.

## Send an event

Something happened, the person wrote, an order shipped, a reminder is due. Post it:

```json
{ "id": "m1", "text": "Remind me to call the bank tomorrow." }
```

`id` is yours, and posting the same one twice is safe, the second one is ignored. `text` is what the agent reads. newt answers `202` right away and runs the turn in the background.

A few optional fields:

- `policy`: by default an event that arrives while a turn is running joins it between model calls, so the agent sees a follow-up message before it replies. Set `queue` to make it wait for the next turn instead.
- `priority`: set `background` for events nobody is waiting on, like a nightly check, so they leave room for the ones a person is waiting on.
- `meta`: any JSON you want back later. The agent never sees it, but your agent core gets it in `newt/turn` and `newt/turn_end`.

All the fields and status codes are in [Protocol 5.1](../protocol/#51-post-events).

## Show progress in your app

```sh
curl -N localhost:7878/v1/agents/assistant/sessions/ana/live -H "Authorization: Bearer $TOKEN"
```

The feed is server-sent events: `received`, `turn_started`, `turn_steered`, `tool_started`, `tool_progress`, `tool_finished`, then `turn_completed` or `turn_failed`. `turn_started` also says which model the turn uses and whether it started the conversation over. Frames carry names, counts and timings, never message text, so it's safe to show "searching…" or a typing indicator from it. Resume with `Last-Event-ID` after a disconnect.

The person's replies don't come through the feed. They go out through your own tool, like `send_message`, or from the `reply` in [`newt/turn_end`](../agent-core/#do-something-after-each-turn-newtturn_end).

## Delete a customer's data

`DELETE` removes everything newt stored for the session: its events, history, checkpoints, journal and live feed. Use it when a customer leaves or asks you to erase their data, newt never deletes history on its own. If a turn is running you get a `409`, so try again once it's done.

## Next

- [Protocol, section 5](../protocol/#5-event-api-called-by-your-application): every field and status code.
- [Re-run a real conversation](../replay/).
