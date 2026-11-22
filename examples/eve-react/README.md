# eve React chat on newt

Serves the HTTP API that eve's `useEveAgent` hook (`eve/react`) talks to, backed by a newt agent. Assistant text and tool-call input stream as the model writes them.

- `translate.ts`: newt live frames to eve `MessageStreamEvent`s. Pure and stateful: the same frames always give the same events and `meta.id`s, which is what makes `startIndex` resume work.
- `server.ts`: `createEveHandlers({ newtUrl, agent, token })` returns `Request` to `Response` handlers for the eve session routes.
- `check.mjs`, run by `examples/check.sh eve-react`: starts newt on the repo's fake model and drives the handlers with eve's own `Client` and `defaultMessageReducer`.

## newt agent config

```toml
[agents.chat]
live_stream = true
```

`live_stream` puts the user's text, the model's text, tool input as it streams, and each tool's result on the live feed, which is all eve's stream needs.

## Routes

| Route | Does |
|---|---|
| `POST /eve/v1/session` | With `{ message }`, creates a session (the newt key is `eve-<uuid>`) and posts the message as a newt event. Without one, only returns a session id. |
| `POST /eve/v1/session/{id}` | Posts `{ message, turnPolicy }` as a newt event (`steer` or `queue`). Without a message, answers the prewarm check. |
| `GET /eve/v1/session/{id}/stream` | Reads newt's live feed, translates it, serves NDJSON from `startIndex` (negative counts back from the tail). `follow=false` stops at the tail. |

## Mount in Next.js

```ts
// app/eve/v1/[...path]/route.ts
import { createEveHandlers } from "./server";

const { handle } = createEveHandlers({
  newtUrl: process.env.NEWT_URL!,
  agent: "chat",
  token: process.env.NEWT_TOKEN!,
});

export const GET = handle;
export const POST = handle;
export const dynamic = "force-dynamic";
```

```tsx
const agent = useEveAgent({ host: "" });
```

Copy `server.ts` and `translate.ts` next to the route. They import each other with `.ts` extensions so Node runs them unbuilt; set `allowImportingTsExtensions` in your `tsconfig.json`.

The handlers do not authenticate callers. Check your own session in the route before calling `handle`, and derive the session id from the user if sessions must not be guessable.

## Limits

- `action.result` carries the tool output as the model sees it, cut to its first 4,000 characters like newt's `tool_output` frame.
- No cancel, clear, compact, reset, input requests or attachments: those routes answer 404, and a message must be text.
- Each stream request re-reads the session's live feed from its first frame, because indexes are counted from there. Frames older than `[retention] live_days` are gone, so older sessions start mid-way.

## Run

```
examples/check.sh eve-react
npm test --prefix examples/eve-react
```
