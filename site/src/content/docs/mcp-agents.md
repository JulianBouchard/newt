---
title: MCP-only agents
description: Build an agent from one MCP server and nothing else. Its instructions, tools, skills and resources become the agent.
---

An agent can be nothing but an MCP server. Build the server and the agent comes with it: no context provider, and nothing in the server needs to know about newt.

```toml
[agents.tasks]
mcp = "https://app.example.com/tasks/mcp"
model = "default"
token_env = "TASKS_TOKEN"
```

newt sends the agent's token to the server as the bearer.

## What newt takes from the server

| The agent's | Comes from |
|---|---|
| Instructions | The server's `instructions`. Empty instructions fail the turn. |
| Model | `model` in `server.toml` |
| Tools | `tools/list` |
| Skills | `skills/list`, named `{agent}/{skill path}` (experimental) |
| Workspace pages | The session's resources, loaded at the start of each turn |

There is no `version`, `pinned`, turn `instructions` or `constraints`. newt reconnects when the tool set goes stale, which includes the server restarting.

## Resources are the workspace

At the start of each turn newt lists and reads the resources with `_meta: { "newt/session": "<key>" }`, so one server can serve every session. Each text resource becomes a page:

| Page field | From the resource |
|---|---|
| `path` | `name` |
| `key` | `_meta["newt/key"]` when set, otherwise `[uri]` |
| `version` | The `sha256:` digest of the text |
| `content` | The text contents |

## Keeping pages fresh

newt subscribes to the session's resources for the rest of the turn. `notifications/resources/updated` marks one page stale and `notifications/resources/list_changed` marks them all; the next read fetches again.

:::tip
The update notification travels on its own stream, so it can arrive after the tool result that caused it. A tool that changes a page should also return `newt/invalidate`, or the page itself in `newt/pages`, so the model's next read sees the change for certain.
:::

## Next

- `assistant-mcp` in `crates/newt-demo` is a complete example. Run it with the [Quickstart](../quickstart/).
- [Protocol 2.7](../protocol/#27-an-agent-that-is-only-an-mcp-server) has every rule.
