export const ARCHITECTURE_DESCRIPTION = `How a newt turn works:

1. Your app posts an event to newt.
2. newt reads the customer's pages, the resources of your agent core.
3. newt calls the model. The prompt is a fixed part (instructions, tools), history that only grows, and this turn. The fixed part and history come from the cache, so each turn pays only for what is new.
4. The model calls your MCP tools. A tool such as complete_task invalidates the tasks page, so the next read fetches it fresh, as React Query refetches an invalidated query.
5. When the turn ends, reads are dropped and writes are kept in history as one-line summaries.

Postgres holds history, the journal and leases. An outward call such as send_message is written to the journal before it runs, so it is sent once even after a crash, and any newt instance can run any session.`;

export const NEWT_PLUS_MCP_DESCRIPTION =
  "newt plus an MCP server equals an agent. Point newt at an MCP server, or let newt run it over stdio, and the server's instructions, tools and resources become the agent.";

export const UNIFIED_MODEL_DESCRIPTION = `One API behind every client, in three tiers:

1. The core API is exposed as public (anyone browsing), authenticated (a logged-in user) and private (an admin, or your own backend). Each tier is wrapped as routes for the frontend and as an MCP server for agents.
2. The frontend calls public and authenticated routes. Other people's agents call the public and authenticated MCP servers, anonymously or as the user. newt's agent calls tools on its agent core, the private tier.
3. All of them reach the same core API. complete_task does the work and says which keys changed: tasks.
4. The frontend refetches the tasks query in the background, and the agent's tasks.md page goes stale, so its next read is current.

One API, and one way to say what changed, for the UI and the agent.`;

export const PROMPT_LAYERS_DESCRIPTION =
  "Two turns side by side. Each prompt is a fixed part, then history, then this turn. What turn 1 keeps is appended to history, so turn 2's fixed part and history are the same bytes as turn 1's prompt and come from the cache. Only turn 2's own part is new.";

export const RETENTION_DESCRIPTION =
  "Before a turn commits, it holds reads (read_file, get_order), the model's reasoning, a durable write (complete_task), an outward call (send_message) and the reply. On commit the reads and the reasoning are dropped. History keeps the write and the outward call as short summaries, and the reply.";

export const WORKSPACE_DESCRIPTION =
  "1. The model calls complete_task(7) on your MCP server. 2. The result says the tasks key changed, so the workspace marks tasks.md stale. 3. The model reads tasks.md. 4. Because it is stale, newt reads it again from your server, so the agent never acts on an old copy.";

export const COMPACTION_DESCRIPTION =
  "History grows turn by turn until it passes history_budget. The agent then writes a checkpoint (memory, open threads, commitments, decisions, tone), and later prompts start with the checkpoint and the latest turns. A checkpoint that drops a commitment is refused. Raw history stays in Postgres.";

export const JOURNAL_DESCRIPTION =
  "1. newt writes an intent entry and waits for Postgres to confirm it. 2. It calls send_message with the key t7-c2. 3. newt crashes before the turn commits. 4. Another newt replays the turn. 5. It calls send_message again with the same key. 6. Your handler has seen t7-c2 already and returns the first result, so the person gets one message.";

export const RUNNER_DESCRIPTION =
  "Your app posts an event, which lands in Postgres (inbox, history, journal, leases). Postgres notifies every newt instance. newt A takes the lease on the session and runs it; newt B waits and sweeps every second for sessions nobody holds. Any instance can run any session, one at a time.";

export const PIECES_DESCRIPTION =
  "Your app sends events to newt and follows the live feed. Each turn, newt talks to four things: the model, your agent core (prompt, tools and pages), any extra servers (more tools and pages), and Postgres (history, journal and leases).";

