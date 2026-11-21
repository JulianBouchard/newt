---
title: Skills
description: Instructions for one kind of task, served by your MCP servers and loaded only when the agent needs them.
---

A skill is instructions for one kind of task, loaded only when the agent needs it. It keeps the system prompt short and the cache warm.

:::note[Experimental]
Skills follow SEP-2640, a draft MCP extension.
:::

## Serve skills from an MCP server

A server that declares `io.modelcontextprotocol/skills` serves skills in the Agent Skills format: a `SKILL.md` with frontmatter, plus any files it lists. Your [home server](../home-server/) and any [server it adds](../servers/) can serve them.

- newt lists them on connect and offers each as `{server}/{skill path}`: `skill://billing/refunds/SKILL.md` on the home of agent `app` becomes `app/billing/refunds`. An added server's skills use its name instead of the agent's.
- newt adds an index of names and descriptions to the system prompt:

  ```text
  <skills>
  Load a skill with load_skill before doing what it covers.
  - app/billing/refunds: How to handle a refund request.
  </skills>
  ```

  and gives the agent a built-in `load_skill` tool. The index changes only when the servers' skills change, so it stays in the cached prefix.
- Nothing is fetched until the model loads one. Then newt reads `SKILL.md` and checks its size, `sha256` digest and frontmatter against the listing. A skill's other files are readable through `load_skill` with `file`, each checked the same way.
- A loaded skill's text stays in that turn's scratch and never enters history.

A file that fails a check never reaches the model. [Protocol 1.5](../protocol/#15-skills) has the full rules.

## Make a tool wait for a skill

Every tool call to a server that speaks newt carries `newt/skills_loaded`, the skills the agent loaded this turn. A handler can refuse an action until the right skill was read, for example a refund tool that needs the `refunds` skill first.
