---
title: Context provider
description: The three JSON endpoints your app serves so newt can build each prompt from the customer's current state.
---

The context provider is how your app tells newt what the agent is and what it can see. It is three JSON endpoints under one base URL. newt calls them; your app never calls newt back.

```toml
[agents.assistant]
context_provider = "https://app.example.com/assistant"
token_env = "ASSISTANT_TOKEN"
```

One token authenticates both directions: callers send it to newt as `Authorization: Bearer`, and newt sends it to your provider.

:::tip
An agent can skip the context provider and be [only an MCP server](../mcp-agents/).
:::

## The three endpoints

| Call | When newt makes it | Returns |
|---|---|---|
| `GET {base}/bootstrap` | First turn, then whenever `version` changes | Instructions, model, skills, MCP servers |
| `POST {base}/turn` | Start of every turn | The session's pages and this turn's guidance |
| `POST {base}/pages` | A read of a stale page, or the first read when `turn` sent no pages | Fresh pages |

## Bootstrap: the same for every session

```json
{
  "version": "2026-11-12.1",
  "instructions": "You are a personal assistant for one person, reached by chat.",
  "model": "default",
  "skills": [
    { "name": "writing", "description": "How to word a message to the person.", "content": "Short sentences. Lead with the answer." }
  ],
  "mcp_servers": [
    { "name": "app", "url": "https://app.example.com/mcp", "auth": "agent_token" }
  ]
}
```

`model` is a `[models]` alias from `server.toml`. Changing any field means a new `version`, which costs one prompt cache miss on the next turn. Optional knobs (`effort`, `max_model_calls`, `history_budget` and more) are in [Protocol 1.3](../protocol/#13-get-basebootstrap).

## Turn: this session, right now

```json
{
  "version": "2026-11-12.1",
  "pages": [
    { "key": ["tasks"], "path": "tasks.md", "version": "7", "content": "# Tasks\n- [ ] #1 Call the bank\n" },
    { "key": ["notes"], "path": "notes.md", "version": "7", "content": "# Notes\n- Lives in Lisbon\n" }
  ],
  "pinned": "Write for WhatsApp: no markdown, two short paragraphs at most.",
  "instructions": "It is 2026-11-12T09:30:00Z.",
  "constraints": ["Do not message the person before Monday: they are on holiday."]
}
```

- `pinned` is session-stable text, such as the writing guide for the person's channel. It sits in the cached prefix.
- `instructions` and `constraints` reach the model at the start of this turn only and never enter history.

:::caution
Constraints are text the model reads. They do not block a tool call. Enforce any rule that must hold in your tool handlers too.
:::

## Pages: the workspace

A page is one file the agent can read with `read_file`, `glob` and `grep`.

| Field | Meaning |
|---|---|
| `key` | Invalidation key, such as `["customer", "profile"]`. Tools mark pages stale by key prefix. |
| `path` | What the agent sees, such as `profile.md`. Unique per response. |
| `version` | Opaque. newt tells tools which version the agent read. |
| `content` | The text. |

Pages are rebuilt from your app, never edited by newt. When a tool marks a page stale, the next read calls `POST {base}/pages` with that key, and a page missing from the answer is gone.

**Slow pages?** Leave `pages` out of the turn response. newt starts the first model call at once and loads every page in the background. The first read waits for that load.

## Rules newt enforces

- Each request times out after 10 seconds. newt does not retry inside a request.
- Decoding is strict: unknown fields, an unknown model alias or a version mismatch fail the turn loudly.
- A failed call stops the turn from starting. The events stay pending and the session retries with backoff (2 s doubling, up to 300 s).

## Next

- [Tools over MCP](../tools/): the other half your app serves.
- [Protocol, section 1](../protocol/#1-context-provider): every field and rule.
