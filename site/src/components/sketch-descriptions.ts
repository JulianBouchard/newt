export const ARCHITECTURE_DESCRIPTION = `How a newt turn works:

1. Your app posts an event to newt.
2. newt reads the customer's pages: the resources of your agent's MCP server.
3. newt calls the model. The prompt is a frozen prefix (instructions, tools), append-only history, and this turn's scratch. The prefix and history are cached, so each turn pays only for what is new.
4. The model calls your MCP tools. A tool such as complete_task invalidates the tasks page, so the next read fetches it fresh, as React Query refetches an invalidated query.
5. When the turn ends, reads are dropped and writes are kept in history as one-line summaries.

Postgres holds history, the journal and leases. An outward call such as send_message is written to the journal before it runs, so it is sent once even after a crash, and any newt instance can run any session.`;

export const NEWT_PLUS_MCP_DESCRIPTION =
  "newt plus an MCP server equals an agent. Point newt at an MCP server, or let newt run it over stdio, and the server's instructions, tools and resources become the agent.";

export const UNIFIED_MODEL_DESCRIPTION = `One API behind every client, in three tiers:

1. The core API is exposed as public (anyone browsing), authenticated (a logged-in user) and private (an admin, or your own backend). Each tier is wrapped as routes for the frontend and as an MCP server for agents.
2. The frontend calls public and authenticated routes. Other people's agents call the public and authenticated MCP servers, anonymously or as the user. newt's agent calls tools on its home MCP server, the private tier.
3. All of them reach the same core API. complete_task does the work and says which keys changed: tasks.
4. The frontend refetches the tasks query in the background, and the agent's tasks.md page goes stale, so its next read is current.

One API, and one way to say what changed, for the UI and the agent.`;
