---
title: Adding servers
description: Give an agent more tools and pages from other MCP servers, yours or someone else's, with no setup beyond a name and a URL.
---

An agent is its [home server](../home-server/) plus any servers the home adds. Add a server for tools the home should not carry itself: a partner's API, a shared internal service, a public MCP server.

```json title="the home's newt extension"
{
  "model": "default",
  "servers": [
    { "name": "crm", "url": "https://crm.example.com/mcp", "auth_env": "CRM_TOKEN" },
    { "name": "sourcing", "url": "stdio:node ./sourcing/server.js" }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | 1 to 32 lowercase letters, digits or `-`, and not the agent's own name. Becomes the server's folder and its tools' prefix; the home's resources cannot use that folder. |
| `url` | `https://…`, `http://…` or `stdio:<command>`, as for the home. |
| `auth_env` | Optional. The environment variable whose value newt sends as `Authorization: Bearer`. newt refuses to load the agent if it is unset. |

That is all the setup there is. There is no list of tools to pick and nothing to approve.

## Names and paths never move

| From the server | The model sees |
|---|---|
| Tool `search_jobs` | `crm__search_jobs` |
| Resource named `jobs/acme-sre.md` | Page `crm/jobs/acme-sre.md` |
| Skill `skill://refunds/SKILL.md` | Skill `crm/refunds` |

The home's tools and pages keep their own names, at the root. An added server's always get its prefix, even when it is the only one. Paths and tool names are written into history and checkpoints, so adding a second server later must not move the first one's.

## Servers that speak newt, and the rest

A server opts into newt's protocol by declaring the `newt` extension, with an empty object:

```json title="server/discover result"
{ "capabilities": { "tools": {}, "resources": {}, "extensions": { "newt": {} } } }
```

| | Speaks newt | Any other server |
|---|---|---|
| Tool calls carry | the session, its state, turn state, pages and skills read, the idempotency key | the idempotency key and trace context only |
| Retention, invalidation | from the tool's `_meta` | every tool is `durable` |
| Result `_meta` (summary, pages, state) | applied, to its own pages | ignored |
| Resources | read with the session and state | read with nothing of newt's |

Declare the extension on your own servers, the ones that should know which customer a call is for. A server you do not run never sees the session's data and cannot change its pages or state.

:::caution
Choosing a server is choosing to trust it. Its tool descriptions reach the model's prompt and its results reach the model, and newt does not filter either.
:::

## When a server is down

newt connects to every server at once, each with its own 20 second limit. A server that cannot be reached, or offers a tool newt cannot load (a name over 64 characters with its prefix, an outward tool that is not idempotent), is left out of the turn: its tools and pages are missing, and the model is told which server is unavailable so it can say so. The rest of the agent runs, and the home's pages stay readable even if a server drops out mid-turn. newt tries the server again every 30 seconds and adds it back before the next turn once it connects and lists cleanly.

A server that changes its tool descriptions costs one prompt cache miss on the next turn, and `newt-report` names the tool that changed.

## Next

- [Tools over MCP](../tools/): the `_meta` fields a server that speaks newt can use.
- [Protocol 2.1](../protocol/#21-servers-the-home-adds): every rule.
