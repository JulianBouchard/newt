import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import http from "node:http";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { Client, defaultMessageReducer } from "eve/client";
import { createEveHandlers } from "./server.ts";

const newtUrl = process.env.NEWT_URL ?? "http://127.0.0.1:7878";
const agent = process.env.NEWT_AGENT ?? "bench";
const token = process.env.NEWT_TOKEN ?? "bench-token";
const eveVersion = JSON.parse(
  readFileSync(new URL("node_modules/eve/package.json", import.meta.url), "utf8"),
).version;

const handlers = createEveHandlers({ newtUrl, agent, token });
const server = http.createServer(async (req, res) => {
  const abort = new AbortController();
  res.on("close", () => abort.abort());
  const body = req.method === "GET" ? undefined : Readable.toWeb(req);
  const response = await handlers.handle(
    new Request(`http://${req.headers.host}${req.url}`, {
      method: req.method,
      headers: req.headers,
      body,
      duplex: "half",
      signal: abort.signal,
    }),
  );
  res.writeHead(response.status, Object.fromEntries(response.headers));
  if (response.body) {
    await pipeline(Readable.fromWeb(response.body), res).catch(() => {});
  } else {
    res.end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const host = `http://127.0.0.1:${server.address().port}`;

const reducer = defaultMessageReducer();
const client = new Client({ host });
const started = performance.now();

async function runTurn(sessionOrFirst, text) {
  const { session, response } =
    typeof sessionOrFirst === "string"
      ? await client.sessions.create({ message: sessionOrFirst })
      : { session: sessionOrFirst, response: await sessionOrFirst.send(text) };
  let state = reducer.initial();
  const events = [];
  const snapshots = [];
  const toolStates = new Map();
  let endedAt;
  for await (const event of response) {
    events.push(event);
    state = reducer.reduce(state, event);
    const assistant = state.messages.findLast((message) => message.role === "assistant");
    const textNow = (assistant?.parts ?? [])
      .filter((part) => part.type === "text")
      .map((part) => part.text)
      .join("");
    snapshots.push({ at: performance.now() - started, text: textNow, type: event.type });
    for (const part of assistant?.parts ?? []) {
      if (part.type === "dynamic-tool") {
        const seen = toolStates.get(part.toolCallId) ?? [];
        if (seen.at(-1) !== part.state) {
          seen.push(part.state);
        }
        toolStates.set(part.toolCallId, seen);
        if (part.state === "input-streaming" && part.inputText) {
          seen.partialInput = true;
        }
      }
    }
    if (event.type === "turn.completed") {
      endedAt = performance.now() - started;
    }
  }
  return { session, events, snapshots, toolStates, endedAt, state };
}

const first = await runTurn("load message 1-1: anything new today?");
const { session, events, snapshots, toolStates } = first;

const growing = [];
for (const snapshot of snapshots) {
  if (snapshot.at < first.endedAt && snapshot.text !== growing.at(-1)) {
    growing.push(snapshot.text);
  }
}
const types = events.map((event) => event.type);
console.log(`eve ${eveVersion}, ${events.length} events over ${first.endedAt.toFixed(0)} ms`);
console.log(`event types: ${[...new Set(types)].join(", ")}`);
console.log(`assistant text took ${growing.length} distinct values before turn.completed, last: ${JSON.stringify(growing.at(-1)?.slice(0, 80))}`);
console.log(`${toolStates.size} tool calls, ${[...toolStates.values()].filter((states) => states.partialInput).length} with partial input visible`);
for (const [callId, states] of [...toolStates].slice(0, 3)) {
  console.log(`tool ${callId}: ${states.join(" -> ")}`);
}

assert.ok(growing.length >= 3, "assistant text grows across several snapshots before the turn ends");
assert.ok(
  growing.every((value, index) => index === 0 || value.startsWith(growing[index - 1])),
  "assistant text only grows",
);
assert.ok(toolStates.size > 0, "the turn has tool parts");
for (const [callId, states] of toolStates) {
  const wanted = ["input-streaming", "input-available", "output-available"];
  assert.deepEqual(
    states.filter((state) => wanted.includes(state)),
    wanted,
    `tool ${callId} goes input-streaming, input-available, output-available`,
  );
}
assert.ok([...toolStates.values()].some((states) => states.partialInput), "a tool input was visible while it streamed");
assert.deepEqual(types.slice(0, 2), ["turn.started", "message.received"], "turn starts with the user message");
assert.equal(types.at(-1), "session.waiting", "the turn ends at session.waiting");
const asked = events.find((event) => event.type === "message.received");
assert.equal(asked.data.message, "load message 1-1: anything new today?", "message.received carries the user's text");
const results = events.filter((event) => event.type === "action.result");
assert.ok(
  results.some((event) => typeof event.data.result.output === "string" && event.data.result.output.length > 0),
  "tool results carry the tool's output",
);

const second = await runTurn(session, "load message 1-2: and the other one?");
assert.equal(second.events[0].type, "turn.started", "the second turn starts at its own turn.started");
assert.notEqual(second.events[0].data.turnId, events[0].data.turnId, "the second turn is a new turn");

const all = (await session.snapshot()).events;
const ids = all.map((event) => event.meta.id);
assert.equal(new Set(ids).size, ids.length, "meta.id is unique across the session");
assert.deepEqual(ids, [...ids].sort(), "meta.id sorts in stream order");
assert.deepEqual(all.slice(0, events.length).map((event) => event.meta.id), events.map((event) => event.meta.id), "turn one's live events are the stream's first events");
assert.deepEqual(all.slice(events.length).map((event) => event.meta.id), second.events.map((event) => event.meta.id), "turn two's live events are the stream's next events");

const cut = 10;
const abort = new AbortController();
const live = [];
for await (const event of session.stream({ startIndex: 0, signal: abort.signal })) {
  live.push(event);
  if (live.length === cut) {
    abort.abort();
  }
}
const resumed = [];
for await (const event of session.stream({ startIndex: cut, follow: false })) {
  resumed.push(event);
}
const joined = [...live, ...resumed].map((event) => event.meta.id);
assert.deepEqual(joined, ids, "dropping at an index and resuming from startIndex gives no duplicates and no gaps");

const tailRead = [];
for await (const event of session.stream({ startIndex: -3 })) {
  tailRead.push(event);
  if (tailRead.length === 3) {
    break;
  }
}
assert.deepEqual(
  tailRead.map((event) => event.meta.id),
  ids.slice(-3),
  "startIndex -3 reads the last three events",
);

const tailHeader = await fetch(`${host}/eve/v1/session/${session.state.sessionId}/stream?startIndex=0&follow=false`);
const tailIndex = tailHeader.headers.get("x-eve-stream-tail-index");
await tailHeader.text();
assert.equal(Number(tailIndex), ids.length - 1, "x-eve-stream-tail-index is the last event's index");

console.log(`reconnect at ${cut}: ${live.length} live + ${resumed.length} resumed = ${joined.length} events, ${ids.length} in the stream`);
console.log(`second turn: ${second.events.length} events, session has ${ids.length}`);
console.log("PASS");
server.closeAllConnections();
server.close();
