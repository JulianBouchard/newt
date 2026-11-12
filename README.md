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
| `GET {base}/bootstrap` | first turn, then whenever the version changes | none | `{ version, instructions, model, first_call_effort?, effort?, max_output_tokens?, max_model_calls? }` |
| `POST {base}/turn` | start of every turn | `{ session }` | `{ version, pages: [Page] }` |
| `POST {base}/pages` | a read of a page a tool invalidated | `{ session, keys: [PageKey] }` | `{ pages: [Page] }` |

A `Page` is `{ key: ["customer", "profile"], path: "profile.md", version, content }`. Tools invalidate pages by key prefix; the next read refetches them, and a page missing from the response is gone. `model` must be a `[models]` alias from `server.toml`. Unknown fields, an unknown model or a version mismatch fail the turn loudly.

## License

Apache-2.0 or MIT, at your option.
