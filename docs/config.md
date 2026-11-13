# Configuration

Newt reads one TOML file. The path is the `NEWT_CONFIG` environment variable, or `server.toml` in the working directory. `server.example.toml` in the repository root is a complete working example; a test parses it, so it stays valid.

Every table rejects unknown keys. Secrets are never in the file: the file holds the names of environment variables, and newt refuses to start if one is unset. A bad file fails at startup with a message naming the key.

Other environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `NEWT_CONFIG` | `server.toml` | Path to the config file. |
| `RUST_LOG` | `info` | Log filter (`tracing` env-filter syntax). |
| `HOSTNAME` | `newt` | Prefix of the runner's lease owner id, which is `{HOSTNAME}-{pid}-{uuid}`. Shows up as `lease_owner` in the session status. |

At startup newt connects to Postgres and applies the files in `migrations/`, then starts listening.

## `[server]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `listen` | socket address | required | Address the HTTP API binds to, for example `"127.0.0.1:7878"`. Plain HTTP; terminate TLS in front. |

## `[database]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `url_env` | string | required | Name of the environment variable that holds the Postgres connection URL. |
| `max_connections` | integer | `10` | Size of the connection pool. |

The pool serves event accepts, turn starts and commits, journal writes, the sweeper and live feeds.

## `[events]`

Optional. The JSONL event file, used for local runs, sims and `newt-report`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `jsonl` | path | none | Append one JSON line per event to this file. No file is written when it is unset. |
| `capture_content` | boolean | `false` | Keep message content (model input and output) in the file. When `false`, content is removed and only fingerprints, sizes, timings and usage remain. |

## `[runner]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `max_sessions` | integer, at least 1 | `64` | Sessions this process runs turns for at once. Others wait. |
| `cache_key_buckets` | integer, at least 1, or `"auto"` | `"auto"` | How many prompt cache keys the sessions of one agent share. See below. |

### `cache_key_buckets`

For `openai-responses` routes newt sends `prompt_cache_key = "{agent}/{n}"`. Anthropic routes do not use it.

Sessions of one agent share the same system prompt and tools. With one key, a brand-new session can reuse the cached system prompt that every other session has already written, so its first call is not a full miss. The cost: OpenAI spreads one key across more machines once it receives more than roughly 15 requests a minute, and a request that lands on another machine misses the cache. So each key should carry as much traffic as it can without passing that rate.

With `"auto"` newt counts each agent's model calls over the last minute and keeps every key under 12 calls a minute: it adds keys as soon as the rate needs them, up to 64, and gives them up only after the rate has stayed low for 10 minutes, so a quiet minute does not reshuffle sessions. An agent starts with one key. A number fixes the count instead. Either way `n` comes from the session key by jump consistent hashing: a session keeps its key while the count holds, and going from n to n + 1 keys moves only one session in n + 1.

### Fixed runner settings

These are constants in `RunnerConfig::default()` (`crates/newt-server/src/runner.rs`), not config keys:

| Setting | Value |
|---|---|
| Lease TTL | 30 s. A heartbeat renews running leases every 10 s. |
| Sweep interval | 1 s. Each sweep wakes up to 100 sessions with pending events and no live lease. |
| Retry backoff after a failed turn | 2 s doubling per failed attempt, capped at 300 s. |
| Attempts before dead-lettering a turn that had no effects | 5 |
| Shutdown grace period | 8 s, then running turns are cancelled and given 5 s more. |

## `[routes.<name>]`

A route is one way to reach one model.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `protocol` | `"anthropic-messages"`, `"openai-responses"` or `"openai-chat"` | required | The wire protocol. `openai-chat` is Chat Completions, which vLLM, Ollama, OpenRouter, Together, Groq and most other hosts speak; it carries no reasoning between calls and sends no `prompt_cache_key`. |
| `model` | string | required | The provider's model id, sent in the request. |
| `base_url` | string | required | Must start with `http://` or `https://`. Newt appends `/v1/messages` for `anthropic-messages` (so use `https://api.anthropic.com`), `/responses` for `openai-responses` (so use `https://api.openai.com/v1`) and `/chat/completions` for `openai-chat` (for example `http://localhost:11434/v1` for Ollama, `https://openrouter.ai/api/v1` for OpenRouter). |
| `key_env` | string | required | Name of the environment variable that holds the API key. |
| `api_key_header` | string | none | Header that carries the key. By default `anthropic-messages` sends `x-api-key` and `openai-responses` sends `Authorization: Bearer`. Set it for gateways that use another header, such as `api-key`. |
| `max_concurrent` | integer, at least 1 | required | Model calls in flight on this route. Further calls wait. Calls from background-priority turns may use at most `max_concurrent - 1` of the slots (at least 1), which keeps a slot free for interactive turns. |

Anthropic requests also send `anthropic-version: 2023-06-01`. Connect timeout is 10 s, a stream that goes silent for 300 s fails, and any one model call has a 10 minute deadline. These are constants.

## `[models.<alias>]`

A model alias is the name a bootstrap uses in its `model` field.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `chain` | array of route names | required | Routes to try, in order. Must be non-empty, and every name must be a route. |

Each call tries the first route; a retryable failure (a network error, a timeout or deadline, HTTP 408, 409, 429 or 5xx, or an `overloaded`, `api`, `rate_limit` or `server` error from the provider) that happens before any output has streamed gets one more try on the same route after a 200 ms pause, then moves to the next route. A failure after output has started streaming is not retried, because the live feed has already shown it.

## `[agents.<name>]`

An agent is the name in the event API path (`/v1/agents/{name}/...`).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `context_provider` | string | one of `context_provider` and `mcp` | Base URL of the agent's context provider; must start with `http://` or `https://`. See `docs/protocol.md`. |
| `mcp` | string | one of `context_provider` and `mcp` | Streamable HTTP URL of the one MCP server the agent is made of: its instructions, tools, skills and resources (`docs/protocol.md` 2.7). Must start with `http://` or `https://`. |
| `model` | string | required with `mcp`, not allowed with `context_provider` | A `[models]` alias. A context provider names its model in the bootstrap instead. |
| `token_env` | string | required | Name of the environment variable that holds the agent's token. It must be set and non-empty. The same token authenticates callers of the event API, and newt sends it to the context provider, to MCP servers that ask for `auth: "agent_token"`, and to an `mcp` agent's server. |

## `[tracing]`

Optional. Without this table, no traces are exported. With it, newt exports one trace per turn over OTLP/HTTP as JSON.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `endpoint` | string | required | OTLP traces URL; must start with `http://` or `https://`. |
| `profile` | `"generic"` or `"langfuse"` | `"generic"` | `"langfuse"` also sets Langfuse's session, observation types and usage details. |
| `headers_env` | string | none | Name of an environment variable holding request headers as `name=value,name=value`. For Langfuse: `Authorization=Basic <base64 public:secret>,x-langfuse-ingestion-version=4`. Must be set if named. |
| `capture_content` | boolean | `true` | Export the messages new to each model call and its response. When `false`, only timings, token counts and tool names are exported, the session key in `gen_ai.conversation.id` is hashed, and error text becomes `failed`. Note the default differs from `[events] capture_content`. |
| `service_name` | string | `"newt"` | The `service.name` resource attribute. |

Span ids derive from the session, turn and call, so exporting the same turn twice overwrites rather than duplicates. Export runs off the turn's path and drops events when it falls behind.

## Example

```toml
server = { listen = "127.0.0.1:7878" }
database = { url_env = "NEWT_DATABASE_URL", max_connections = 10 }
events = { jsonl = "newt-events.jsonl" }
runner = { max_sessions = 64, cache_key_buckets = "auto" }

[routes.openai]
protocol = "openai-responses"
model = "gpt-6.1-terra"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"
max_concurrent = 6

[routes.anthropic]
protocol = "anthropic-messages"
model = "claude-opus-5-5"
base_url = "https://api.anthropic.com"
key_env = "ANTHROPIC_API_KEY"
max_concurrent = 6

[models.default]
chain = ["openai", "anthropic"]

[agents.assistant]
context_provider = "http://localhost:4000/assistant"
token_env = "ASSISTANT_TOKEN"

[agents.tasks]
mcp = "http://localhost:4000/tasks/mcp"
model = "default"
token_env = "TASKS_TOKEN"

[tracing]
endpoint = "https://cloud.langfuse.com/api/public/otel/v1/traces"
profile = "langfuse"
headers_env = "NEWT_OTLP_HEADERS"
capture_content = true
```
