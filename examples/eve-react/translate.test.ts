import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { defaultMessageReducer } from "eve/client";
import { createTranslator, type LiveFrame } from "./translate.ts";

type Recorded = { seq: number; frame: LiveFrame };

const recorded: Recorded[] = JSON.parse(
  readFileSync(new URL("fixtures/turn.json", import.meta.url), "utf8"),
);

function translate(frames: Recorded[]) {
  const translator = createTranslator();
  return frames.flatMap(({ seq, frame }) => translator.push(seq, frame));
}

function frame(type: string, extra: Record<string, unknown> = {}): LiveFrame {
  return { type, at: "2026-01-01T00:00:00Z", session: "agent/key", turn: "t1", ...extra };
}

function run(frames: LiveFrame[]) {
  return translate(frames.map((entry, index) => ({ seq: index + 1, frame: entry })));
}

const received = (id: string, message?: string): LiveFrame => ({
  type: "received",
  event_id: id,
  inbox_id: 1,
  ...(message === undefined ? {} : { text: message }),
});

test("a recorded turn becomes eve events in order", () => {
  const events = translate(recorded);
  const types = events.map((event) => event.type);
  assert.deepEqual(types.slice(0, 11), [
    "turn.started",
    "message.received",
    "step.started",
    "action.input.appended",
    "action.input.appended",
    "action.input.appended",
    "actions.requested",
    "step.completed",
    "action.result",
    "step.started",
    "action.input.appended",
  ]);
  assert.deepEqual(types.slice(-5), [
    "actions.requested",
    "step.completed",
    "action.result",
    "turn.completed",
    "session.waiting",
  ]);
  const received = events.find((event) => event.type === "message.received");
  assert.equal(received?.data.message, "load message 1-1: anything new today?");
});

test("the first tool event is an empty delta, then the streamed input, then the call", () => {
  const events = translate(recorded).filter(
    (event) =>
      (event.type === "action.input.appended" && event.data.callId === "call_0000000000000001") ||
      event.type === "actions.requested",
  );
  const first = events
    .slice(0, 4)
    .map((event) => (event.type === "action.input.appended" ? event.data.inputTextDelta : event.type));
  assert.deepEqual(first, ["", '{"n', 'ame":"writing"}', "actions.requested"]);
});

test("a step with text completes its message with the text streamed in it, then the step", () => {
  const events = translate(recorded);
  const completed = events.findIndex((event) => event.type === "message.completed");
  const message = events[completed];
  assert.equal(message.type === "message.completed" && message.data.finishReason, "stop");
  const deltas = events
    .filter((event) => event.type === "message.appended" && event.data.stepIndex === 15)
    .map((event) => (event.type === "message.appended" ? event.data.messageDelta : ""));
  assert.equal(deltas.length, 8);
  assert.equal(message.type === "message.completed" && message.data.message, deltas.join(""));
  assert.equal(events[completed + 1].type, "step.completed");
});

test("a step without text has no message.completed", () => {
  const events = translate(recorded.filter(({ seq }) => seq <= 9));
  assert.ok(!events.some((event) => event.type === "message.completed"));
  const done = events.find((event) => event.type === "step.completed");
  assert.equal(done?.type === "step.completed" && done.data.finishReason, "tool-calls");
});

test("every event gets a sortable meta.id from the frame seq and its index in the frame", () => {
  const events = translate(recorded);
  const ids = events.map((event) => event.meta.id);
  assert.equal(new Set(ids).size, ids.length);
  assert.deepEqual(ids, [...ids].sort());
  assert.deepEqual(ids.slice(0, 2), ["000000000002-000", "000000000002-001"]);
});

test("replaying the same frames gives identical events, so any startIndex resumes on the same event", () => {
  const first = translate(recorded);
  const second = translate(recorded);
  assert.deepEqual(second, first);
  const split = createTranslator();
  const resumed = recorded.flatMap(({ seq, frame: entry }) => split.push(seq, entry));
  assert.deepEqual(resumed.slice(5), first.slice(5));
});

test("every event of a turn carries the delivery ids the turn consumed", () => {
  const events = translate(recorded);
  assert.ok(events.every((event) => event.meta.deliveryIds?.[0] === "m-1"));
});

test("eve's reducer shows the tool as input-streaming on the empty delta, then input-available, then output", () => {
  const reducer = defaultMessageReducer();
  const events = translate(recorded);
  let state = reducer.initial();
  const states: string[] = [];
  for (const event of events) {
    state = reducer.reduce(state, event);
    const assistant = state.messages.find((message) => message.role === "assistant");
    const tool = assistant?.parts.find((part) => part.type === "dynamic-tool" && part.toolCallId === "call_0000000000000001");
    if (tool && tool.type === "dynamic-tool" && states.at(-1) !== tool.state) {
      states.push(tool.state);
    }
  }
  assert.deepEqual(states, ["input-streaming", "input-available", "output-available"]);
  const assistant = state.messages.find((message) => message.role === "assistant");
  const text = assistant?.parts.find((part) => part.type === "text");
  assert.ok(text && text.type === "text" && text.text.startsWith("Berlin meeting"));
});

test("a failed tool call becomes a failed action.result that eve shows as an error", () => {
  const events = run([
    received("m1", "hi"),
    frame("turn_started"),
    frame("model_call_started", { call: 0, model: "default" }),
    frame("tool_started", { call_id: "c1", name: "send" }),
    frame("tool_input", { call_id: "c1", name: "send", input: { to: "x" } }),
    frame("tool_finished", { call_id: "c1", name: "send", is_error: true, ms: 12 }),
  ]);
  const result = events.at(-1);
  assert.equal(result?.type, "action.result");
  assert.equal(result?.type === "action.result" && result.data.status, "failed");
  const reducer = defaultMessageReducer();
  let state = reducer.initial();
  for (const event of events) {
    state = reducer.reduce(state, event);
  }
  const tool = state.messages.flatMap((message) => message.parts).find((part) => part.type === "dynamic-tool");
  assert.equal(tool?.type === "dynamic-tool" && tool.state, "output-error");
});

test("turn_failed becomes turn.failed then session.waiting", () => {
  const events = run([received("m1", "hi"), frame("turn_started"), frame("turn_failed", { error: "model unavailable" })]);
  const types = events.map((event) => event.type);
  assert.deepEqual(types.slice(-2), ["turn.failed", "session.waiting"]);
  const failed = events.at(-2);
  assert.equal(failed?.type === "turn.failed" && failed.data.message, "model unavailable");
  assert.equal(events.at(-1)?.data && "continuationToken" in events.at(-1)!.data && events.at(-1)!.data.continuationToken, "key");
});

test("a message sent while a turn runs is announced when the turn steers it in", () => {
  const events = run([
    received("m1", "first"),
    frame("turn_started"),
    received("m2", "second"),
    frame("turn_steered", { events: 1 }),
    frame("turn_completed"),
  ]);
  const types = events.map((event) => event.type);
  assert.deepEqual(types, ["turn.started", "message.received", "message.received", "turn.completed", "session.waiting"]);
  assert.deepEqual(events[0].meta.deliveryIds, ["m1"]);
  assert.deepEqual(events[2].meta.deliveryIds, ["m1", "m2"]);
});

test("a queued message waits for the next turn and starts it", () => {
  const events = run([
    received("m1", "first"),
    frame("turn_started"),
    received("m2", "second"),
    frame("turn_completed"),
    frame("turn_started", { turn: "t2" }),
    frame("turn_completed", { turn: "t2" }),
  ]);
  const second = events.filter((event) => event.data && "turnId" in event.data && event.data.turnId === "t2");
  assert.deepEqual(second.map((event) => event.type), ["turn.started", "message.received", "turn.completed"]);
  assert.deepEqual(second[0].meta.deliveryIds, ["m2"]);
  assert.deepEqual(events.find((event) => event.type === "session.waiting")?.meta.deliveryIds, ["m1"]);
});

test("an event without text from another app still marks its turn but shows no user message", () => {
  const events = run([received("sig-1"), frame("turn_started"), frame("turn_completed")]);
  assert.deepEqual(events.map((event) => event.type), ["turn.started", "turn.completed", "session.waiting"]);
  assert.deepEqual(events[0].meta.deliveryIds, ["sig-1"]);
});

test("frames of a turn the translator never saw start are ignored", () => {
  const events = run([frame("text_delta", { text: "tail of an older turn" }), frame("turn_completed")]);
  assert.deepEqual(events, []);
});
