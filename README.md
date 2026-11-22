<h1 align="center"><img src="assets/logo/newt-lockup.svg" alt="newt" width="260"></h1>

An agent runtime for products. newt runs one long-lived agent for each of your customers. It reads your app fresh every turn, remembers what it did, and runs on one Rust binary and Postgres.

Newts regrow what they lose, and so does this. Every turn drops what the agent only read and reads it again from your app next time, so stale data never piles up and the prompt cache stays warm.

Your agent is an MCP server, written with any MCP SDK. newt runs it.

**Docs: [newt.julianbouchard.com](https://newt.julianbouchard.com)**

## Try it

All you need is Docker. A fake model answers, so you don't need an API key.

```sh
docker compose -f deploy/compose.yaml up --build -d
curl -X POST localhost:7878/v1/agents/assistant/sessions/ana/events \
  -H "Authorization: Bearer demo-token" -H 'content-type: application/json' \
  -d '{"id":"m1","text":"Remind me to call the bank tomorrow."}'
curl -N localhost:7878/v1/agents/assistant/sessions/ana/live -H "Authorization: Bearer demo-token"
```

The [Quickstart](https://newt.julianbouchard.com/quickstart/) explains what just happened, and how to run it from source.

## Where to go next

- [What is newt](https://newt.julianbouchard.com/overview/) and [why it exists](https://newt.julianbouchard.com/motivation/)
- [Build your agent](https://newt.julianbouchard.com/agent-core/): your agent is an MCP server
- [How newt works](https://newt.julianbouchard.com/design/): why it works the way it does
- [What's stable](https://newt.julianbouchard.com/stability/): and what's still experimental
- Reference: [protocol](docs/protocol.md), [configuration](docs/config.md), [benchmark](docs/benchmark.md)
- [Contributing](CONTRIBUTING.md) and [security](SECURITY.md)

## License

Apache-2.0 or MIT, at your option. See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
