---
title: Sending events
description: Post events to a session, follow turns on the live feed, read history, and delete a session.
---

Your app talks to newt through a small HTTP API. Agent and session key are in the path, and the agent's token is the bearer.

| Method and path | Purpose |
|---|---|
| `POST /v1/agents/{agent}/sessions/{key}/events` | Add an event |
| `GET /v1/agents/{agent}/sessions/{key}` | Session status |
| `GET /v1/agents/{agent}/sessions/{key}/live` | Live feed (server-sent events) |
| `GET /v1/agents/{agent}/sessions/{key}/history` | What history kept |
| `POST /v1/agents/{agent}/sessions/{key}/fork` | A new session with a copy of this one's history |
| `DELETE /v1/agents/{agent}/sessions/{key}` | Delete the session and everything stored for it |
| `GET /healthz` | 200 when Postgres answers. No auth. |

A session key is 1 to 200 bytes with no `/`. A session starts with its first event.

## Post an event

```json
{ "id": "m1", "text": "Remind me to call the bank tomorrow.", "policy": "steer", "priority": "interactive" }
```

| Field | Default | Meaning |
|---|---|---|
| `id` | required | Your id for the event. A repeat in the same session is accepted and not queued again. |
| `text` | required | What the agent reads. |
| `policy` | `steer` | `steer` joins a running turn between model calls. `queue` and `collect` wait for the next turn. |
| `priority` | `interactive` | `background` turns leave a model slot free for interactive ones. |
| `meta` | none | Any JSON, stored with the event and sent to your context provider with the turn that answers it. Never shown to the model. |

newt answers `202` with `{ "inbox_id": 41, "duplicate": false }`. Retrying a post with the same `id` is safe.

## Follow the live feed

```sh
curl -N localhost:7878/v1/agents/assistant/sessions/ana/live -H "Authorization: Bearer $TOKEN"
```

Each frame has an `id` (its sequence number) and a JSON payload with a `type`: `received`, `turn_started`, `turn_steered`, `tool_started`, `tool_progress`, `tool_finished`, `turn_completed` or `turn_failed`. Payloads hold names, counts and timings, not message text. Resume with `Last-Event-ID` or `?cursor=<seq>`.

Replies reach the person through your own outward tool, such as `send_message`, not through the feed.

## Delete a session

`DELETE` removes the session's events, history, checkpoints, journal and live feed. Use it when a customer leaves or asks for their data to be erased: newt never deletes history on its own. It answers `409` while a turn holds the lease; retry after the turn ends.

## Errors

Errors are `{ "error": "<message>" }` with status 400 (bad body or key), 401 (missing or wrong token, or unknown agent), 404 (no such session), 409 (conflict) or 500.

## Next

- [Protocol, section 5](../protocol/#5-event-api-called-by-your-application): every field and status.
- [Replay and fork](../replay/).
