export const RELEASE_HIGHLIGHTS = {
  "2.1.0": {
    title: "Stream into any chat UI, end turns like everyone else",
    body: "With `live_stream`, text and tool-call input stream on the live feed as the model writes them: `POST .../chat` speaks the Vercel AI SDK's UI message stream, and `examples/eve-react` serves eve's React client. A reply now ends the turn, and any tool can end it with `newt/ends_turn`. Cancel from any instance, `/metrics`, an inbox cap and slots held back for live chats round it out. [Events](../events/)",
  },
  "2.0.0": {
    title: "An agent is an MCP server",
    body: "The HTTP context provider is gone. An agent is now its agent core, an MCP server that declares its settings in the `newt` extension, plus any extra servers it pulls in. Plain MCP servers get the tool call and nothing of the session's, and newt can run any of them itself over stdio. [Build your agent](../agent-core/)",
  },
  "1.6.0": {
    title: "A model per turn, and starting a session over",
    body: "`/turn` can pick the model and effort for one turn, such as a stronger model for a hard request, and `reset: true` runs the turn as if the session had no history. Nothing is deleted: history and fork still see every entry. [Context provider](../agent-core/#use-a-different-model-for-one-turn)",
  },
  "1.5.0": {
    title: "The agent's reply in the turn-end callback",
    body: "The callback carries `reply`, what the agent said when it finished, so your provider can post it to a chat or a thread without a send tool on the agent. [Context provider](../agent-core/#do-something-after-each-turn-newtturn_end)",
  },
  "1.4.0": {
    title: "Tools read the session state",
    body: "A tool handler can check what your provider recorded for the session, such as the task list the agent saw last turn, straight from the call's `_meta`, with no lookup of its own. [Write tools](../tools/#what-newt-sends-with-every-call)",
  },
  "1.3.0": {
    title: "Pages and tools update the state mid-turn",
    body: "A lazy page load can note what it rendered and a tool can note what it did, each with a merge patch that touches only its own keys. The next turn gets both. [Session state](../agent-core/#remember-things-between-turns)",
  },
  "1.2.0": {
    title: "Context providers can be stateless",
    body: "newt keeps your provider's state per session and commits it with the turn, so the provider needs no session store and can run as many copies as you like. A new `/turn_end` callback reports every finished turn, delivered at least once. [Context provider](../agent-core/#remember-things-between-turns)",
  },
  "1.1.0": {
    title: "About two thirds fewer Postgres statements per turn",
    body: "A turn reaches its first model call in four statements and commits in one, which matters most when Postgres is not next to newt.",
  },
};
