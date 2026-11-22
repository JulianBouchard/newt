import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { DefaultChatTransport, readUIMessageStream } from "ai";

const base = process.env.NEWT_URL ?? "http://127.0.0.1:7878";
const agent = process.env.NEWT_AGENT ?? "bench";
const token = process.env.NEWT_TOKEN ?? "bench-token";
const text = process.env.NEWT_MESSAGE ?? "load message 1-1: anything new today?";
const aiVersion = JSON.parse(
  readFileSync(new URL("node_modules/ai/package.json", import.meta.url), "utf8"),
).version;

const transport = new DefaultChatTransport({
  api: `${base}/v1/agents/${agent}/sessions/${randomUUID()}/chat`,
  headers: { authorization: `Bearer ${token}` },
});

const message = { id: randomUUID(), role: "user", parts: [{ type: "text", text }] };
const started = performance.now();
const chunks = await transport.sendMessages({
  trigger: "submit-message",
  chatId: "check",
  messageId: undefined,
  messages: [message],
  abortSignal: undefined,
});

const isTool = (part) => part.type.startsWith("tool-") || part.type === "dynamic-tool";
const snapshots = [];
let first;
for await (const snapshot of readUIMessageStream({ stream: chunks })) {
  const at = performance.now() - started;
  const parts = snapshot.parts;
  const streamingInput = parts.filter(
    (part) => isTool(part) && part.state === "input-streaming" && (part.rawInput ?? "") !== "",
  );
  const writtenText = parts.filter((part) => part.type === "text" && part.text !== "");
  snapshots.push({ at, parts: structuredClone(parts), streamingInput: streamingInput.length });
  if (first === undefined && (streamingInput.length > 0 || writtenText.length > 0)) {
    first = at;
  }
}
const ended = performance.now() - started;
const last = snapshots.at(-1);

const textParts = last.parts.filter((part) => part.type === "text");
const toolParts = last.parts.filter(isTool);
const partialInputs = new Set(
  snapshots.flatMap((snapshot) =>
    snapshot.parts
      .filter((part) => isTool(part) && part.state === "input-streaming" && part.rawInput)
      .map((part) => `${part.toolCallId}:${part.rawInput}`),
  ),
);
const streamedTools = new Set([...partialInputs].map((entry) => entry.split(":")[0]));
const earlyText = snapshots.filter((snapshot) => snapshot.at < ended - 50);

console.log(`ai ${aiVersion}, ${snapshots.length} message snapshots over ${ended.toFixed(0)} ms`);
console.log(`first text or tool input after ${first.toFixed(0)} ms, turn ended after ${ended.toFixed(0)} ms`);
console.log(
  `final message: ${textParts.length} text parts, ${toolParts.length} tool parts, ` +
    `${partialInputs.size} partial tool inputs seen on ${streamedTools.size} tool calls`,
);
for (const part of toolParts.slice(0, 4)) {
  console.log(`  ${part.type} ${part.state} input=${JSON.stringify(part.input).slice(0, 80)}`);
}
console.log(`  text: ${JSON.stringify(textParts.map((part) => part.text).join(" | ").slice(0, 100))}`);

assert.ok(textParts.some((part) => part.text.length > 0), "the message has a text part with text");
assert.ok(toolParts.length > 0, "the message has tool parts");
assert.ok(
  toolParts.every((part) => part.state !== "input-streaming"),
  "every tool call's input is complete at the end",
);
assert.ok(streamedTools.size > 0, "tool input was visible in partial form while it streamed");
assert.ok(
  [...partialInputs].length > streamedTools.size,
  "at least one tool call showed more than one partial input",
);
assert.ok(first < ended, "something was visible before the turn ended");
assert.ok(earlyText.length > 1, "several snapshots arrived before the turn ended");
assert.ok(
  first < ended * 0.8,
  `first output at ${first.toFixed(0)} ms is not within 80% of the ${ended.toFixed(0)} ms turn`,
);
console.log("PASS");
