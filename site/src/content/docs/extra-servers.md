---
title: "Add more MCP servers"
description: Give an agent more tools and pages from other MCP servers, yours or someone else's, with a name and a URL.
---

Your [agent core](../agent-core/) doesn't have to carry every tool itself. If the agent needs a partner's API, a shared internal service or some public MCP server, the agent core lists it in its settings and newt connects to it too.

```json title="the agent core's newt extension"
{
  "model": "default",
  "servers": [
    { "name": "crm", "url": "https://crm.example.com/mcp", "auth_env": "CRM_TOKEN" },
    { "name": "sourcing", "url": "stdio:node ./sourcing/server.js" }
  ]
}
```

Each server gets a `name`, a `url` (http, https or `stdio:`, same as the agent core) and optionally `auth_env`, the environment variable newt sends as its bearer token. There's no tool picking and nothing to approve, every tool the server lists becomes the agent's. [Protocol 2.1](../protocol/#21-extra-servers) has the exact rules for each field.

## How their tools and pages are named

An extra server's tools, pages and skills always live under its name:

| The server has | The model sees |
|---|---|
| Tool `search_jobs` | `crm__search_jobs` |
| Resource `jobs/acme-sre.md` | Page `crm/jobs/acme-sre.md` |
| Skill `refunds` | Skill `crm/refunds` |

The agent core's own tools and pages stay at the root. We prefix even when there's only one extra server, because tool names and paths get written into history. If adding a second server later renamed the first one's tools, every old turn would point at names that don't exist anymore.

## Which servers see the customer's data

Most MCP servers out there know nothing about newt, and they work fine. newt calls their tools, reads their resources, and treats every tool as `durable`.

Your own servers can opt in by declaring the `newt` extension, with an empty object:

```json title="server/discover result"
{ "capabilities": { "tools": {}, "resources": {}, "extensions": { "newt": {} } } }
```

Once they do, they get the session key and state with every call, and newt listens to what they say back about retention, pages and state. A plain server gets the tool call, an idempotency key and trace context, and that's it. So a server you don't run never sees which customer it's working for, and can't touch the session's pages or state. [Write tools](../tools/) covers what a server with the extension can do with it.

:::caution
Adding a server means trusting it. Its tool descriptions go into the model's prompt and its results go to the model, and newt doesn't filter either.
:::

## When a server is down

One broken server shouldn't take the whole agent down with it. If newt can't reach a server, or the server offers a tool newt can't load, newt leaves it out of the turn and tells the model it's unavailable, so the agent can say so instead of guessing. Everything else runs as normal.

newt tries a server that was unreachable again every 30 seconds, and adds it back before the next turn once it connects. A server that was left out because of a bad tool waits until the agent core reconnects, since retrying won't change its tool list.
