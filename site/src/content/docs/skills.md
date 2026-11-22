---
title: "Add skills"
description: Instructions for one kind of task, served by your MCP servers and only loaded when the agent needs them.
---

A skill is a set of instructions for one kind of task, like handling a refund. The agent only loads it when it needs it, so the system prompt stays short and the cache stays warm.

:::note[Experimental]
Skills follow SEP-2640, a draft MCP extension, so the details can still change.
:::

## Serve a skill

Any of the agent's servers can serve skills by declaring the `io.modelcontextprotocol/skills` extension. A skill is a `SKILL.md` with frontmatter, plus any files it lists, in the Agent Skills format.

newt lists them when it connects and names each one after its server. `skill://billing/refunds/SKILL.md` on the agent core of agent `app` becomes `app/billing/refunds`, and on an [extra server](../extra-servers/) called `crm` it becomes `crm/billing/refunds`.

The agent sees an index of names and descriptions in its system prompt, and a built-in `load_skill` tool:

```text
<skills>
Load a skill with load_skill before doing what it covers.
- app/billing/refunds: How to handle a refund request.
</skills>
```

Nothing is fetched until the agent loads a skill. Then newt reads it and checks it against the listing (size, `sha256` and frontmatter), so a skill can't change between listing and loading without newt noticing. A skill's text stays in that turn only and never goes into history. [Protocol 1.5](../protocol/#15-skills) has the full rules.

## Require a skill before a tool runs

Every call to a server with the `newt` extension says which skills the agent loaded this turn, in `newt/skills_loaded`. So a refund tool can refuse to run until the agent has read the `refunds` skill.
