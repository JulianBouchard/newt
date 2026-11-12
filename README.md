# newt

An agent runtime for products: a long-lived agent per customer, your backend as its world, and a database's discipline about context.

Newts regrow what they lose. So does this: every turn strips the context back and grows it from the workspace and the history, so stale reads never pile up and the prompt cache stays warm.

Early days. In so far: streaming Anthropic and OpenAI clients, the turn loop with retention classes, steering, a crash-safe journal, the Postgres runtime, and the reactive workspace fed by a context provider.

## Run it

```sh
docker compose up -d --wait
export NEWT_DATABASE_URL=postgres://newt:newt@localhost:55432/newt
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=... ASSISTANT_TOKEN=...
NEWT_CONFIG=server.example.toml cargo run -p newt-server
curl localhost:7878/healthz
```

## Context provider

Each agent in `server.toml` names a `context_provider` base URL and a `token_env`. That one token authenticates both directions: callers send it to newt as `Authorization: Bearer`, and newt sends it to the provider. The provider serves three JSON endpoints:

| Call | When | Request | Response |
|---|---|---|---|
| `GET {base}/bootstrap` | first turn, then whenever the version changes | none | `{ version, instructions, model, first_call_effort?, effort?, max_output_tokens?, max_model_calls?, mcp_servers? }` |
| `POST {base}/turn` | start of every turn | `{ session }` | `{ version, pages: [Page] }` |
| `POST {base}/pages` | a read of a page a tool invalidated | `{ session, keys: [PageKey] }` | `{ pages: [Page] }` |

A `Page` is `{ key: ["customer", "profile"], path: "profile.md", version, content }`. Tools invalidate pages by key prefix; the next read refetches them, and a page missing from the response is gone. `model` must be a `[models]` alias from `server.toml`. Unknown fields, an unknown model or a version mismatch fail the turn loudly.

## Tools over MCP

Tools come from the MCP servers the bootstrap lists: `{ name, url, headers?, auth? }`, where `auth: "agent_token"` sends the agent's token as the bearer and the default sends none. Headers are static and must not hold secrets. Newt connects once per agent and bootstrap version over Streamable HTTP, lists the tools, and keeps those connections for every session of that version.

Newt reads and writes a few `_meta` fields; a server that ignores them still works, and its tools count as durable with no invalidation.

| Where | Field | Meaning |
|---|---|---|
| tool | `newt/retention` | `ephemeral` (result dropped at the end of the turn), `durable` (call kept, result cut to its summary) or `outward` (an effect outside your system, journaled before it runs) |
| tool | `newt/invalidates` | page key prefixes every successful call makes stale |
| tool | `newt/idempotent` | must be `true` on outward tools, or newt refuses to load them |
| call | `newt/session` | the session key |
| call | `newt/idempotency_key` | stable across retries of the same call; outward handlers must honour it |
| result | `newt/summary` | what durable history keeps instead of the full result |
| result | `newt/invalidate` | more page key prefixes to mark stale |
| result | `newt/pages` | fresh page data, used without a refetch |

Invalidation only reaches pages newt already has, so a tool that creates a page returns it in `newt/pages`. A result whose newt metadata is malformed marks every page stale. When a call times out or loses its connection, the model is told the outcome is unknown and not to repeat it, and the connections are rebuilt on the next turn.

## License

Apache-2.0 or MIT, at your option.
