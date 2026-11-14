---
title: Skills
description: Instructions for one kind of task, loaded only when the agent needs them, from the bootstrap or from an MCP server.
---

A skill is instructions for one kind of task, loaded only when the agent needs it. It keeps the system prompt short and the cache warm.

## Skills in the bootstrap

```json
"skills": [
  { "name": "writing", "description": "How to word a message to the person.", "content": "Short sentences. Lead with the answer." }
]
```

newt adds an index of names and descriptions to the system prompt:

```text
<skills>
Load a skill with load_skill before doing what it covers.
- writing: How to word a message to the person.
</skills>
```

and gives the agent a built-in `load_skill` tool. The index changes only with the bootstrap version, so it stays in the cached prefix. A loaded skill's text stays in that turn's scratch and never enters history.

## Make a tool wait for a skill

Every tool call carries `newt/skills_loaded`, the skills the agent loaded this turn. A handler can refuse an action until the right skill was read, for example a refund tool that needs the `refunds` skill first.

## Skills served over MCP

:::note[Experimental]
This follows SEP-2640, a draft MCP extension.
:::

An MCP server that declares `io.modelcontextprotocol/skills` can serve skills in the Agent Skills format instead of the bootstrap.

- newt lists them on connect and offers each as `{server}/{skill path}`: `skill://acme/billing/refunds/SKILL.md` on server `app` becomes `app/acme/billing/refunds`.
- Nothing is fetched until the model loads one. Then newt reads `SKILL.md` and checks its size, `sha256` digest and frontmatter against the listing.
- A skill's other files are readable through `load_skill` with `file`, each checked the same way.

A file that fails a check never reaches the model. [Protocol 2.6](../protocol/#26-skills-served-over-mcp) has the full rules.
