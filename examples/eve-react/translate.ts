import type { MessageStreamEvent } from "eve/client";

/** One frame of newt's live feed (docs/protocol.md 5.3), as parsed from the SSE `data:` line. */
export type LiveFrame = { type: string; at?: string; turn?: string; [field: string]: unknown };

/** Stream version eve clients check in `x-eve-stream-version`. */
export const STREAM_VERSION = "26";

type Received = { id: string; text: string };

type Turn = {
  id: string;
  deliveryIds: string[];
  step: number;
  stepText: string;
  tools: Map<string, { name: string; step: number }>;
  answered: Set<string>;
};

/** Builds the event `meta` that carries the user's text onto the live `received` frame (needs `live_meta = true`). */
export function messageMeta(text: string): { eve: { message: string } } {
  return { eve: { message: text } };
}

const FINISH_REASONS: Record<string, string> = {
  tool_use: "tool-calls",
  end_turn: "stop",
  max_tokens: "length",
  refusal: "content-filter",
};

function text(frame: LiveFrame, field: string): string {
  const value = frame[field];
  return typeof value === "string" ? value : "";
}

function receivedFrom(frame: LiveFrame): Received {
  const meta = frame.meta as { eve?: { message?: unknown } } | undefined;
  const message = typeof frame.text === "string" ? frame.text : meta?.eve?.message;
  return { id: text(frame, "event_id"), text: typeof message === "string" ? message : "" };
}

function eventId(seq: number, index: number): string {
  return `${String(seq).padStart(12, "0")}-${String(index).padStart(3, "0")}`;
}

/**
 * Creates a translator from newt live frames to eve `MessageStreamEvent`s.
 * Feed it every frame of a session in order, starting from the first: the events
 * and their indexes are a pure function of the frames, so the same frames always
 * give the same stream and `startIndex` resumes land on the same events.
 */
export function createTranslator() {
  let received: Received[] = [];
  let turn: Turn | undefined;
  let sequence = 0;
  let lastAt = new Date(0).toISOString();

  return {
    /** Translates one frame (`seq` is the SSE `id`) into zero or more events, in order. */
    push(seq: number, frame: LiveFrame): MessageStreamEvent[] {
      const out: MessageStreamEvent[] = [];
      const at = frame.at ?? lastAt;
      lastAt = at;

      const emit = (type: MessageStreamEvent["type"], data: Record<string, unknown>) => {
        const meta: Record<string, unknown> = { id: eventId(seq, out.length), at };
        if (turn && turn.deliveryIds.length > 0) {
          meta.deliveryIds = [...turn.deliveryIds];
        }
        out.push({ type, data, meta } as MessageStreamEvent);
      };

      const inTurn = (data: Record<string, unknown>) => ({
        ...data,
        sequence: sequence++,
        turnId: turn?.id,
      });

      const claimReceived = () => {
        for (const item of received) {
          if (item.id) {
            turn?.deliveryIds.push(item.id);
          }
        }
      };

      const announceReceived = () => {
        for (const item of received) {
          if (item.text) {
            emit("message.received", inTurn({ message: item.text, parts: [{ type: "text", text: item.text }] }));
          }
        }
        received = [];
      };

      const endTurn = (type: "turn.completed" | "turn.failed", data: Record<string, unknown>) => {
        emit(type, inTurn(data));
        emit("session.waiting", {
          continuationToken: text(frame, "session").split("/").slice(1).join("/"),
          wait: "next-user-message",
        });
        turn = undefined;
      };

      if (frame.type === "received") {
        received.push(receivedFrom(frame));
        return out;
      }

      if (frame.type === "turn_started") {
        turn = { id: text(frame, "turn"), deliveryIds: [], step: 0, stepText: "", tools: new Map(), answered: new Set() };
        claimReceived();
        emit("turn.started", inTurn({}));
        announceReceived();
        return out;
      }

      if (!turn || turn.id !== frame.turn) {
        return out;
      }
      const stepData = (data: Record<string, unknown>) => inTurn({ stepIndex: turn?.step, ...data });

      switch (frame.type) {
        case "turn_steered":
          claimReceived();
          announceReceived();
          break;
        case "model_call_started":
          turn.step = typeof frame.call === "number" ? frame.call : turn.step + 1;
          turn.stepText = "";
          emit("step.started", stepData({ modelId: text(frame, "model") }));
          break;
        case "text_delta":
          turn.stepText += text(frame, "text");
          emit("message.appended", stepData({ messageDelta: text(frame, "text") }));
          break;
        case "tool_started":
          turn.tools.set(text(frame, "call_id"), { name: text(frame, "name"), step: turn.step });
          emit(
            "action.input.appended",
            stepData({ callId: text(frame, "call_id"), toolName: text(frame, "name"), inputTextDelta: "" }),
          );
          break;
        case "tool_input_delta":
          turn.tools.set(text(frame, "call_id"), { name: text(frame, "name"), step: turn.step });
          emit(
            "action.input.appended",
            stepData({ callId: text(frame, "call_id"), toolName: text(frame, "name"), inputTextDelta: text(frame, "delta") }),
          );
          break;
        case "tool_input":
          turn.tools.set(text(frame, "call_id"), { name: text(frame, "name"), step: turn.step });
          emit(
            "actions.requested",
            stepData({
              actions: [{ kind: "tool-call", callId: text(frame, "call_id"), toolName: text(frame, "name"), input: frame.input }],
            }),
          );
          break;
        case "tool_output":
        case "tool_finished": {
          const failed = frame.is_error === true;
          const callId = text(frame, "call_id");
          if (turn.answered.has(callId)) break;
          turn.answered.add(callId);
          const call = turn.tools.get(callId);
          const output = frame.type === "tool_output" ? text(frame, "output") : { ms: frame.ms };
          const errorMessage = frame.type === "tool_output" ? text(frame, "output") : "the tool call failed";
          emit(
            "action.result",
            stepData({
              stepIndex: call?.step ?? turn.step,
              status: failed ? "failed" : "completed",
              result: {
                kind: "tool-result",
                callId,
                toolName: call?.name ?? text(frame, "name"),
                output,
                ...(failed ? { isError: true } : {}),
              },
              ...(failed ? { error: { code: "TOOL_FAILED", message: errorMessage } } : {}),
            }),
          );
          break;
        }
        case "model_call_finished": {
          const finishReason = FINISH_REASONS[text(frame, "stop")] ?? "other";
          if (turn.stepText !== "") {
            emit("message.completed", stepData({ finishReason, message: turn.stepText }));
          }
          emit("step.completed", stepData({ finishReason }));
          turn.stepText = "";
          break;
        }
        case "turn_completed":
          endTurn("turn.completed", {});
          break;
        case "turn_failed":
          endTurn("turn.failed", { code: "TURN_FAILED", message: text(frame, "error"), terminal: false });
          break;
        default:
          break;
      }
      return out;
    },
  };
}
