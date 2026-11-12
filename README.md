# newt

An agent runtime for products: a long-lived agent per customer, your backend as its world, and a database's discipline about context.

Newts regrow what they lose. So does this: every turn strips the context back and grows it from the workspace and the history, so stale reads never pile up and the prompt cache stays warm.

Early days. Milestone 1 (skeleton) is in: config, Postgres schema, health check, event sink.

## Run it

```sh
docker compose up -d --wait
export NEWT_DATABASE_URL=postgres://newt:newt@localhost:55432/newt
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=... ASSISTANT_TOKEN=...
NEWT_CONFIG=server.example.toml cargo run -p newt-server
curl localhost:7878/healthz
```

## License

Apache-2.0 or MIT, at your option.
