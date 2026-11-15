export const RELEASE_HIGHLIGHTS = {
  "1.4.0": {
    title: "Tools read the session state",
    body: "A tool handler can check what your provider recorded for the session, such as the task list the agent saw last turn, straight from the call's `_meta`, with no lookup of its own. [Tools over MCP](../tools/#what-newt-sends-with-every-call)",
  },
  "1.3.0": {
    title: "Pages and tools update the state mid-turn",
    body: "A lazy page load can note what it rendered and a tool can note what it did, each with a merge patch that touches only its own keys. The next turn gets both. [Session state](../context-provider/#session-state)",
  },
  "1.2.0": {
    title: "Context providers can be stateless",
    body: "newt keeps your provider's state per session and commits it with the turn, so the provider needs no session store and can run as many copies as you like. A new `/turn_end` callback reports every finished turn, delivered at least once. [Context provider](../context-provider/#session-state)",
  },
  "1.1.0": {
    title: "About two thirds fewer Postgres statements per turn",
    body: "A turn reaches its first model call in four statements and commits in one, which matters most when Postgres is not next to newt.",
  },
};
