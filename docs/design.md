# Design

This is the reasoning behind newt, with a pointer to the code for each claim. If a claim here has no pointer, treat it as opinion.

The premise: an agent that talks to one person over weeks accumulates context, and most agent runtimes handle that by letting the prompt grow and hoping. That has two costs. Stale tool results sit in the prompt and the model acts on them. And any edit to earlier text invalidates the provider's prompt cache, so every turn pays for the whole prompt again. Newt treats the prompt the way a database treats a log: append, never rewrite, and rebuild the working view from a source of truth.

## An agent is MCP servers

An agent is one MCP server, its home, plus any servers the home adds (`crates/newt-server/src/home.rs`, `mcp/`). The home's instructions are the system prompt, its declared `newt` extension holds the model and the turn budgets, its resources are the workspace, and its tools and skills are the agent's. Newt has no other interface for an application to implement, so an agent can be written with any MCP SDK, tested in any MCP client, and run hosted (`https://`), next to newt in the same deploy (`http://localhost`), or by newt itself over stdio, with the same code.

Newt's own protocol (session state, retention classes, page invalidation, per-turn context through `newt/turn`, the turn-end request) is an MCP extension. A server opts in by declaring it; any other server is treated as plain MCP: it gets the tool call and nothing of the session's, its tools are `durable`, and newt ignores whatever it says about pages or state. Added servers are the developer's choice and the developer's to trust. Newt keeps their failures from stopping the agent: a server that cannot be reached is left out of that turn and named in the turn context.

Paths and tool names reach history, so they never depend on how many servers there are: the home's pages sit at the root and its tools keep their names, while an added server's pages always sit under `<name>/` and its tools are always `<name>__<tool>`.

## The prompt has three layers

Each model call is built from three layers, in this order (`crates/newt-core/src/prompt.rs`):

1. **Frozen prefix.** The system prompt (the home server's `instructions`, then the session's `pinned` text, then the skills index) and the tool list. It changes only when the agent's servers change their instructions, settings or tools, or when `pinned` changes.
2. **History.** The committed messages of the session, oldest first, preceded by the latest checkpoint if there is one.
3. **Scratch.** What the current turn adds: the turn context (`instructions` and `constraints`), the new events, the model's replies and the tool calls and results so far.

Within a turn, a model call's input extends the previous call's input and nothing earlier changes. Between turns, the turn's scratch is filtered and appended to history; earlier history is untouched. The next turn's prompt is therefore the previous one plus a suffix. The provider's prefix cache keys on exact prefixes, so the whole earlier prompt is a cache hit and only the suffix is new.

The code helps keep this true:

- Each model call emits three fingerprints, one per layer (`LayerFingerprint`: hash, bytes, items). The prefix and history fingerprints also carry one hash per piece: the system prompt and each tool, and each history message. `newt-report` reads them from the event file and prints a cache report card: the cached share of all input, the cost at given prices, and for every call that read under 50% from cache, what broke it. The causes are a first call, a changed model, the first changed piece of the prefix (`tool \`search\` changed, about 6120 bytes into the prefix`, or a tool added, removed or reordered), history rewritten from message n (and whether compaction did it), an idle gap longer than the cache lives, or, with prefix and history unchanged and no gap, routing to a machine without the cache.
- For Anthropic, newt marks cache breakpoints on the system block, the last tool, the last history message and the last message (`provider/anthropic.rs`). For OpenAI it sends a `prompt_cache_key` (`provider/openai.rs`); see `[runner] cache_key_buckets` in `docs/config.md` for why sessions of one agent share a few keys.
- Recitation, off unless the home sets `recite`: on a long turn the model is reminded what it is answering. From the fifth model call, and every fourth after, the harness appends a `<goal>` note quoting the turn's events (each cut to 500 characters). Like every harness note it lives in the turn's scratch, after everything cached, and never reaches history. It is off by default because it did not pay: in `examples/recitation_eval.rs` (50 pairs of turns of up to 25 model calls on `gpt-6.1-terra`) every turn passed with and without it, and it cost 1 to 12% more input tokens.
- Per-turn guidance goes in scratch, not in the prefix, so it costs nothing to change. Text that is stable for a session goes in `pinned`, which sits in the prefix: free while unchanged, one miss when it changes.

The honest exceptions are a change to the agent's instructions or tools (one miss on the prefix) and compaction, which replaces the older part of history once per budget crossing (below).

## Retention classes

A turn's scratch is full of material that was useful once: file reads, lookups, search results, the model's reasoning. Keeping it makes history grow and goes stale. Newt decides what survives per tool, using a class the tool declares (`crates/newt-core/src/commit.rs`, `tools.rs`):

- `ephemeral`: the call and its result are dropped when the turn commits. Reads and lookups.
- `durable`: the call stays, and its result is replaced by a short `newt/summary` from the tool (or cut to 500 characters). Writes to the application's own state.
- `outward`: kept like `durable`, plus the exactly-once handling below. Anything with an effect outside the application.

The model's own text replies are kept. Its reasoning is dropped. What the agent learned from an ephemeral read is expected to be re-readable from the workspace, so history carries what was done, not what was seen. The agent can also save a fact explicitly with the built-in `remember` tool, which is `durable`.

## The reactive workspace

The workspace is a set of text pages the application owns and the agent reads through `read_file`, `glob` and `grep` (`crates/newt-core/src/workspace.rs`). The pages are the MCP resources of the agent's servers (`crates/newt-server/src/mcp/pages.rs`): the home's at the root, each added server's under its own folder. A page has a key (an array of strings), a path and a version.

When a tool changes the application's state, it says which pages that makes stale, by key prefix, either in its definition (`newt/invalidates`) or in the result (`newt/invalidate`). The next read of a stale page reads it again from its server, so the agent never reads a copy it knows is out of date. A tool that knows the new content can return the page itself (`newt/pages`) and skip the refetch. Pages the agent has already read are refreshed in the background so the next read is quick.

Because reads are ephemeral and the workspace is current, nothing a read returned ever needs to be remembered by history. The workspace is what makes that safe. The cost is that the server must be able to render pages on request; newt starts the first model call while pages load, so a slow render delays only the first read.

Tools are also told what the agent has seen: the pages read this turn with their versions, and the skills loaded. A handler can use this to refuse an action based on a page that has changed since the agent read it.

## Compaction, checkpoints and commitments

History cannot grow forever. After a turn commits, if the session's live history is over `history_budget` tokens (60,000 by default, set by the home), newt compacts it (`crates/newt-server/src/compaction.rs`, `checkpoints.rs`).

Compaction is a turn like any other, run by the agent's own model with the same prefix and history, plus one instruction. In it the agent first saves anything not yet recorded, then calls `checkpoint` with its memory, open threads, commitments, decisions and tone. Nothing is sent while compacting: outward tools are refused.

The risk in summarising is silent loss of a promise. Newt refuses a checkpoint that drops a commitment from the previous checkpoint unless it is listed under `resolved`, and it refuses an empty one or one longer than the budget's character allowance (`checkpoint.rs`). The agent gets the refusal as a tool result and can try again within the turn's model-call limit.

On success, newt stores the checkpoint and the sequence number it covers. Later prompts start with the rendered checkpoint, then the most recent whole turns that fit in a quarter of the budget, then anything saved during the flush. Raw history rows are never deleted; the checkpoint is a cut point, not a replacement. A failed compaction waits before the next try (ten minutes, doubling, at most a day) and the session keeps running on its long history meanwhile.

## The journal and exactly-once outward calls

A turn can send a message and then crash before it commits. If the session simply reruns, the person gets two messages. Newt avoids this with a per-turn journal in Postgres (`crates/newt-core/src/journal.rs`, `crates/newt-server/src/journal_store.rs`).

The journal records, in order: events that steered the turn, each model response, an intent entry before each outward call, and each tool output. After a crash another runner picks up the session, finds the open turn, and replays it: recorded model responses are used instead of calling the model, recorded tool outputs are used instead of calling the tools, and a call with no recorded output is made again.

That last case is the dangerous one, so outward calls are handled differently from other tools:

- The intent entry is written, and acknowledged by Postgres, before the call. If the write fails, the call is not made.
- Every call carries an idempotency key built from the session, turn id and call id. The turn id and call ids come from the journaled response, so a replayed call has the same key.
- Newt cannot see inside the application. It guarantees the retry carries the same key; the tool's handler must store the key with the effect and treat a repeat as a no-op. A tool that does not declare `newt/idempotent: true` is refused at load time.
- Outward calls run last in a step, one at a time, at most one per `batch`. If one fails, later ones are skipped.

Model responses, steered events and tool outputs are appended without waiting for a durable commit (`synchronous_commit` is off for a batch that carries no intent entry); losing one means a model call or a safe tool call is made again. Only the intent entry waits, and its batch is committed durably. So the journal costs a synchronous wait only before outward calls, in addition to the reads at turn start and the commit at the end.

What this does not give you: exactly-once effects in a system whose handler ignores the key. It also does not make the model's choices deterministic; replay avoids asking again by reusing what was recorded.

## The Postgres runner

Newt is one binary plus Postgres. A session's state, its inbox of unprocessed events, history, checkpoints and the journal all live in Postgres (`migrations/0001_init.sql`), so any instance can run any session.

- **Leases.** A runner claims a session by setting `lease_owner`, bumping `lease_epoch` and a 30 second expiry in one `UPDATE`. A heartbeat renews the leases of running sessions every 10 seconds. Commits take the session row `FOR UPDATE` and check owner and epoch: a runner whose lease has moved gets `Fenced` and its commit is discarded. Journal writes are fenced the same way (`sessions.rs`, `journal_store.rs`).
- **Wakeups.** Accepting an event is one SQL statement that inserts it (deduplicated by event id), creates the session row if needed, and calls `pg_notify` on `newt_inbox`. Each instance holds a `LISTEN` connection and wakes the named session at once (`live.rs`, `runner.rs`).
- **Sweeps.** `NOTIFY` is not durable, so every second each instance runs a sweep that lists sessions with unconsumed events and no live lease, and wakes them. This covers missed notifications, crashed owners (their leases expire) and sessions in retry backoff when it ends.
- **Retries.** A turn that fails without effects is retried with backoff and dead-lettered after five attempts; one that already had effects commits what it did and is retried for the rest.
- **Steering.** Events that arrive during a turn join it between model calls, and interrupt a model call that has not started producing output (`steering.rs`, `turn.rs`).
- **Live feed.** Turn progress is appended to a per-session table and followed over SSE, with `NOTIFY` to wake other instances.

## Not in the design

No cross-session memory, no sandboxed code execution, no multi-agent orchestration, no scheduler for timers, no policing of what an added MCP server says. The crates are not published to crates.io. What 1.x keeps stable is listed under Stability in the README.
