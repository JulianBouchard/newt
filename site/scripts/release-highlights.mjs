export const RELEASE_HIGHLIGHTS = {
  "2.0.0": {
    title: "An agent is an MCP server",
    body: "The HTTP context provider is gone: an agent is its home MCP server, which declares its settings in the `newt` extension, plus any servers it adds. Servers that do not speak newt get a plain tool call and nothing of the session's, and newt can run any of them itself over stdio. [The home server](../home-server/)",
  },
  "1.6.0": {
    title: "A model per turn, and starting a session over",
    body: "`/turn` can pick the model and effort for one turn, such as a stronger model for a hard request, and `reset: true` runs the turn as if the session had no history. Nothing is deleted: history and fork still see every entry. [Context provider](../home-server/#pick-the-model-for-one-turn)",
  },
  "1.5.0": {
    title: "The agent's reply in the turn-end callback",
    body: "The callback carries `reply`, what the agent said when it finished, so your provider can post it to a chat or a thread without a send tool on the agent. [Context provider](../home-server/#hear-when-a-turn-ends)",
  },
  "1.4.0": {
    title: "Tools read the session state",
    body: "A tool handler can check what your provider recorded for the session, such as the task list the agent saw last turn, straight from the call's `_meta`, with no lookup of its own. [Tools over MCP](../tools/#what-newt-sends-with-every-call)",
  },
  "1.3.0": {
    title: "Pages and tools update the state mid-turn",
    body: "A lazy page load can note what it rendered and a tool can note what it did, each with a merge patch that touches only its own keys. The next turn gets both. [Session state](../home-server/#session-state)",
  },
  "1.2.0": {
    title: "Context providers can be stateless",
    body: "newt keeps your provider's state per session and commits it with the turn, so the provider needs no session store and can run as many copies as you like. A new `/turn_end` callback reports every finished turn, delivered at least once. [Context provider](../home-server/#session-state)",
  },
  "1.1.0": {
    title: "About two thirds fewer Postgres statements per turn",
    body: "A turn reaches its first model call in four statements and commits in one, which matters most when Postgres is not next to newt.",
  },
};
