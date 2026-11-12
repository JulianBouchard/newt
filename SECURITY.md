# Security

## Reporting a vulnerability

Use GitHub private vulnerability reporting on this repository: open the Security tab and choose "Report a vulnerability". Please do not open a public issue or pull request for a vulnerability.

Include what you found, the version or commit, and steps to reproduce it. Expect an acknowledgement within a few days. There is no bug bounty.

## Supported versions

newt is pre-1.0 and not published to crates.io. Only the `main` branch is supported. Fixes land on `main`; there are no backports.

## Threat model

This is what the code does today. It is not a certification.

### What is trusted

- **The deployment.** The `newt` process, its Postgres database, the config file and the environment variables it names.
- **Context providers.** A provider decides the agent's instructions, model alias, skills, workspace pages and which MCP servers newt connects to (`crates/newt-server/src/context.rs`). Anything a provider returns is treated as the operator's own configuration.
- **MCP servers.** Newt calls the tools they list and believes the `_meta` they return: retention class, invalidations, summaries and pages (`crates/newt-server/src/mcp.rs`).

A compromised provider can point `mcp_servers` at any URL with `auth: "agent_token"`, and newt will send the agent's bearer token there. Keep providers and MCP servers inside your own trust boundary.

### One token per agent

Each `[agents.<name>]` entry names one environment variable (`token_env`). That one token:

- authenticates callers of newt's event API for that agent (`Authorization: Bearer`, `crates/newt-server/src/http.rs`);
- is sent by newt to the agent's context provider on every request (`context.rs`);
- is sent to an MCP server only when its bootstrap entry says `auth: "agent_token"` (`mcp.rs`).

The token is not scoped to a session: it grants access to every session of that agent. Newt compares SHA-256 digests of the tokens in constant time, so the check does not leak the token or its length (`same_token` in `http.rs`). An unknown agent name and a wrong token both return 401. Newt serves plain HTTP and does not terminate TLS; put it behind something that does. Newt refuses to start if a token variable is unset or empty.

### Model output is untrusted

Model output reaches the world only through tools.

- Newt does not check tool arguments against a tool's input schema; it only requires a JSON object (`mcp.rs`). Handlers must validate their own input.
- `constraints` from a provider are text shown to the model for one turn. They do not stop a tool call. Enforce anything that must hold in the tool handler, as the README says.
- Every tool call carries an idempotency key. Outward tools (`newt/retention: "outward"`) must declare `newt/idempotent: true`, or newt refuses to load the tool set (`mcp.rs`, `tool_def`).
- An outward call is journaled before it runs, runs after the batch's other calls, and at most one per batch is accepted. If the journal write fails, the call is not sent. During compaction no outward call runs (`crates/newt-core/src/batch.rs`).
- Newt cannot make a remote effect exactly-once by itself. After a crash it may call an outward tool again with the same key, and the handler must treat a repeated key as a no-op. See `docs/protocol.md`.

### Content capture

- Message content is stored in Postgres: inbox payloads, history, checkpoints and the turn journal hold event text, model output and tool output. Protect the database accordingly. `events.capture_content` does not affect it.
- The JSONL event file (`[events] jsonl`) is off unless a path is set. When it is set, message content is removed from events unless `[events] capture_content = true` (default `false`; `crates/newt-core/src/sink.rs`, `event.rs`).
- OTLP export exists only if a `[tracing]` section is present. Inside that section, `capture_content` defaults to `true`: the messages new to each model call and its response are exported. Set `capture_content = false` to export timings, token counts and tool names only; that setting also hashes the session key used as the conversation id and replaces error text with `failed` (`crates/newt-core/src/otlp/spans.rs`). Library users can add a `Redactor` that runs over captured content.
- The live feed (`/v1/agents/{agent}/sessions/{key}/live`) carries event ids, tool names, counts and error text. It does not carry message text.
- Operator logs (`RUST_LOG`) can include error messages from providers and MCP servers.

### Not covered

- Rate limiting on the event API. Event bodies are capped at 2 MiB (413 above that); put a proxy in front for per-client limits.
- Isolation between agents sharing one database and one process.
- Anything about the model providers' own handling of your data.
