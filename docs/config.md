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
| `listen` | socket address | required | Address the HTTP API binds to, for example `"127.0.0.1:7878"`. Plain HTTP; terminate TLS in front. `/healthz` and `/metrics` answer without auth on the same address, so keep it off the public internet or route only `/v1` to it. |

## `[database]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `url_env` | string | none | Name of the environment variable that holds the Postgres connection URL. |
| `embedded` | path | none | Run newt's own Postgres with its data in this directory instead. |
| `max_connections` | integer | `10` | Size of the connection pool. newt opens every connection at startup, so a burst of sessions does not wait for the pool to grow. |

Set exactly one of `url_env` and `embedded`. With `embedded`, newt downloads the Postgres 18 binaries for this platform once (zonky's builds from Maven Central, which bring their own libraries; into `~/.theseus/postgresql`, checked against their published hash), creates the database in `<embedded>/data` on the first start, runs the server on a free local port while newt runs and stops it on exit. A server left running by a newt that was killed is stopped and restarted. It is meant for one machine: several newt instances share a Postgres you run, through `url_env`. Postgres refuses to run as root, so neither does `embedded`; the Docker setup in `deploy/` uses its own Postgres container. Builds without the default `embedded-postgres` feature reject `embedded`.

The pool serves event accepts, turn starts and commits, journal writes, the sweeper and live feeds. Accepting an event takes one connection and starting a turn three, so under a burst (hundreds of sessions receiving events in the same second) turns wait for the pool, not the model. Size it for your bursts: roughly one connection per session you expect to start in the same second, up to what your Postgres allows (`max_connections` there is 100 by default, shared by every newt instance).

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
| `reserved_interactive` | integer, less than `max_sessions` | a quarter of `max_sessions`, at least 1 (0 when `max_sessions` is 1) | Slots only interactive turns may use. Turns whose events are all `background` priority (protocol 5.1) share the other `max_sessions - reserved_interactive` slots (at least one), so a burst of background work never fills the process and leaves a live chat waiting. An interactive turn can use every slot. |
| `max_waiting` | integer, at least 1 | `4 * max_sessions` | Woken sessions that may wait for a slot at once. A wake past it is dropped and the session stays in its inbox until the next sweep (every second) finds it again, oldest first, so a burst of notifications cannot pile up unbounded tasks. Counted in `newt_wakes_dropped_total` (protocol 5.10). |
| `cache_key_buckets` | integer, at least 1, or `"auto"` | `"auto"` | How many prompt cache keys the sessions of one agent share. See below. |

### `cache_key_buckets`

For `openai-responses` routes newt sends `prompt_cache_key = "{agent}/{n}"`. Anthropic routes do not use it.

Sessions of one agent share the same system prompt and tools. With one key, a brand-new session can reuse the cached system prompt that every other session has already written, so its first call is not a full miss. The cost: OpenAI spreads one key across more machines once it receives more than roughly 15 requests a minute, and a request that lands on another machine misses the cache. So each key should carry as much traffic as it can without passing that rate.

With `"auto"` newt counts each agent's model calls over the last minute and keeps every key under 12 calls a minute: it adds keys as soon as the rate needs them, up to 256 (about 3,000 calls a minute), and gives them up only after the rate has stayed low for 10 minutes, so a quiet minute does not reshuffle sessions. An agent starts with one key. A number fixes the count instead. Either way `n` comes from the session key by jump consistent hashing: a session keeps its key while the count holds, and going from n to n + 1 keys moves only one session in n + 1. A session that used its key in the last 10 minutes, while the provider still holds its prefix, keeps that key when keys are added, unless the key itself passed 12 calls in the last minute: new, idle and crowded sessions take their hashed key, and no other session moves.

### Fixed runner settings

These are constants in `RunnerConfig::default()` (`crates/newt-server/src/runner.rs`), not config keys:

| Setting | Value |
|---|---|
| Lease TTL | 30 s. A heartbeat renews running leases every 10 s. |
| Sweep interval | 1 s. Each sweep wakes up to 100 sessions with pending events and no live lease. |
| Retry backoff after a failed turn | 2 s doubling per failed attempt, capped at 300 s. |
| Attempts before dead-lettering a turn that had no effects | 5 |
| Shutdown grace period | 8 s, then running turns are cancelled and given 5 s more. |

## `[retention]`

Optional. newt deletes rows that only serve operations once they are old. History and checkpoints are the agent's memory and are never deleted on their own: delete a whole session with `DELETE /v1/agents/{agent}/sessions/{key}` (`docs/protocol.md`, 5.6).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `inbox_days` | integer | `30` | Keep events this many days after the turn that consumed them committed, dead-lettered ones included. A repeat of an event id is recognised for as long as the event is kept. `0` keeps them forever. |
| `live_days` | integer | `7` | Keep live feed frames this many days. `0` keeps them forever. |

Each instance prunes once at start and then every hour, in batches of 5,000 rows.

## `[routes.<name>]`

A route is one way to reach one model.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `protocol` | `"anthropic-messages"`, `"openai-responses"` or `"openai-chat"` | required | The wire protocol. `openai-chat` is Chat Completions, which vLLM, Ollama, OpenRouter, Together, Groq and most other hosts speak; it carries no reasoning between calls and sends no `prompt_cache_key`. |
| `model` | string | required | The provider's model id, sent in the request. |
| `base_url` | string | required | Must start with `http://` or `https://`. Newt appends `/v1/messages` for `anthropic-messages` (so use `https://api.anthropic.com`), `/responses` for `openai-responses` (so use `https://api.openai.com/v1`) and `/chat/completions` for `openai-chat` (for example `http://localhost:11434/v1` for Ollama, `https://openrouter.ai/api/v1` for OpenRouter). |
| `key_env` | string | required | Name of the environment variable that holds the API key. |
| `api_key_header` | string | none | Header that carries the key. By default `anthropic-messages` sends `x-api-key` and `openai-responses` sends `Authorization: Bearer`. Set it for gateways that use another header, such as `api-key`. |
| `headers_env` | string | none | Name of an environment variable holding extra request headers as `name=value,name=value`, sent on every call next to the key, for gateways that admit a request only with a second header such as an internal token. The header that carries the key is refused here; set it with `key_env`. Values are never logged. Must be set if named. |
| `max_concurrent` | integer, at least 1 | required | Model calls in flight on this route. Further calls wait. Calls from background-priority turns may use at most `max_concurrent - 1` of the slots (at least 1), which keeps a slot free for interactive turns. |
| `replay_reasoning` | bool | `true` | `openai-responses` only. Newt asks for encrypted reasoning and sends it back on the next call of the turn. Set `false` for a gateway that can answer two calls from different backends (for example a pool of accounts, or overflow to another provider), which reject each other's encrypted reasoning with `invalid_encrypted_content`. |

Anthropic requests also send `anthropic-version: 2023-06-01`. Connect timeout is 10 s, a stream that goes silent for 300 s fails, and any one model call has a 10 minute deadline. These are constants.

## `[models.<alias>]`

A model alias is the name an agent's home server declares in its `model` setting, and a `newt/turn` result names in its `model` field to pick another model for one turn (`docs/protocol.md` 1.2, 1.3). The home picks the alias; `server.toml` decides which providers it means.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `chain` | array of route names | required | Routes to try, in order. Must be non-empty, and every name must be a route. |

Each call tries the first route; a retryable failure (a network error, a timeout or deadline, HTTP 408, 409, 429 or 5xx, or an `overloaded`, `api`, `rate_limit` or `server` error from the provider) that happens before any output has streamed gets one more try on the same route after a 200 ms pause, then moves to the next route. A failure after output has started streaming is not retried, because the live feed has already shown it.

## `[agents.<name>]`

An agent is the name in the event API path (`/v1/agents/{name}/...`).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `mcp` | string | required | The agent's home MCP server, which is the agent: its instructions, model and settings, tools, skills, workspace pages and the servers it adds (`docs/protocol.md` section 1). `https://…` or `http://…` for one reached over Streamable HTTP, or `stdio:<command>` for one newt runs itself and talks to over stdin and stdout. |
| `token_env` | string | required | Name of the environment variable that holds the agent's token. It must be set and non-empty. The same token authenticates callers of the event API, and newt sends it to the home server over HTTP. It is never sent to the servers the home adds, nor over stdio. |
| `live_meta` | bool | `false` | Put each event's `meta` on its `received` frame in the live feed (protocol 5.3), so an app following the feed can show who sent what before the turn commits. Off by default because the live feed then carries whatever your app put in `meta`. |
| `live_stream` | bool | `false` | Stream the model's output on the live feed (protocol 5.3): the answer text as `text_delta` frames and each tool call's arguments as `tool_input_delta` frames, plus the full `tool_input` of every call and slim `model_call_started` / `model_call_finished` frames that mark each model call. A chat UI can show the answer, or a `send_message` tool's `text`, as it is written instead of when the turn ends. It also enables `POST .../chat` (protocol 5.7). Reasoning is never streamed. Off by default because the live feed then carries message text and tool arguments. |
| `max_pending_events` | integer, at least 1 | `100` | Unconsumed events one session of this agent may hold. `POST .../events` past it answers 429 with `Retry-After` (protocol 5.1) until a turn consumes some; a repeat of an event already held is still answered as a duplicate. |

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
retention = { inbox_days = 30, live_days = 7 }

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
mcp = "http://localhost:4000/mcp"
token_env = "ASSISTANT_TOKEN"

[agents.tasks]
mcp = "stdio:node ./tasks-agent/server.js"
token_env = "TASKS_TOKEN"

[tracing]
endpoint = "https://cloud.langfuse.com/api/public/otel/v1/traces"
profile = "langfuse"
headers_env = "NEWT_OTLP_HEADERS"
capture_content = true
```
