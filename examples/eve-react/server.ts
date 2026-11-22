import { randomUUID } from "node:crypto";
import type { MessageStreamEvent } from "eve/client";
import { createTranslator, messageMeta, STREAM_VERSION, type LiveFrame } from "./translate.ts";

/** Where newt runs, which agent answers, and that agent's bearer token. The agent needs `live_stream = true`. */
export type EveAdapterConfig = { newtUrl: string; agent: string; token: string };

/** The eve HTTP routes `useEveAgent` calls, as `Request` to `Response` handlers. */
export type EveHandlers = {
  createSession: (request: Request) => Promise<Response>;
  sendMessage: (request: Request, sessionId: string) => Promise<Response>;
  streamSession: (request: Request, sessionId: string) => Promise<Response>;
  /** Routes any request whose path ends in `/eve/v1/session[/{id}[/stream]]`, so it mounts under any prefix. */
  handle: (request: Request) => Promise<Response>;
};

type LiveItem = { seq: number; frame: LiveFrame };

const HEARTBEAT_MS = 5000;
const encoder = new TextEncoder();
const ROUTE = /\/eve\/v1\/session(?:\/([^/]+)(\/stream)?)?\/?$/;

function failure(status: number, error: string): Response {
  return Response.json({ error }, { status });
}

async function readJson(request: Request): Promise<Record<string, unknown>> {
  const raw = await request.text();
  return raw.trim() === "" ? {} : (JSON.parse(raw) as Record<string, unknown>);
}

function messageText(message: unknown): string | undefined {
  if (typeof message === "string") {
    return message;
  }
  if (Array.isArray(message)) {
    const parts = message as { type?: string; text?: string }[];
    if (parts.every((part) => part.type === "text" && typeof part.text === "string")) {
      return parts.map((part) => part.text).join("\n\n");
    }
  }
  return undefined;
}

async function* liveItems(response: Response): AsyncGenerator<LiveItem> {
  const reader = response.body?.getReader();
  if (!reader) {
    throw new Error("newt live feed has no body");
  }
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      return;
    }
    buffer += decoder.decode(value, { stream: true });
    let end = buffer.indexOf("\n\n");
    while (end !== -1) {
      const block = buffer.slice(0, end);
      buffer = buffer.slice(end + 2);
      const id = block.match(/^id:\s*(\d+)/m)?.[1];
      const data = block.match(/^data:\s?(.*)$/m)?.[1];
      if (id !== undefined && data !== undefined) {
        yield { seq: Number(id), frame: JSON.parse(data) as LiveFrame };
      }
      end = buffer.indexOf("\n\n");
    }
  }
}

/** Creates the handlers for one newt agent. An eve session id is the newt session key. */
export function createEveHandlers({ newtUrl, agent, token }: EveAdapterConfig): EveHandlers {
  const newt = (sessionId: string, path: string, init: RequestInit = {}) =>
    fetch(`${newtUrl}/v1/agents/${agent}/sessions/${encodeURIComponent(sessionId)}${path}`, {
      ...init,
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    });

  async function accept(sessionId: string, body: Record<string, unknown>): Promise<Response> {
    const text = messageText(body.message);
    if (body.inputResponses !== undefined || text === undefined || text.trim() === "") {
      return failure(400, "send a text message; input responses and attachments are not supported");
    }
    const deliveryId = randomUUID();
    const response = await newt(sessionId, "/events", {
      method: "POST",
      body: JSON.stringify({
        id: deliveryId,
        text,
        policy: body.turnPolicy === "queue" ? "queue" : "steer",
        meta: messageMeta(text),
      }),
    });
    if (!response.ok) {
      return failure(502, `newt rejected the message (${response.status})`);
    }
    return Response.json({ sessionId, deliveryId }, { status: 202, headers: { "x-eve-session-id": sessionId } });
  }

  async function createSession(request: Request): Promise<Response> {
    const body = await readJson(request);
    const sessionId = `eve-${randomUUID()}`;
    if (body.message === undefined) {
      return Response.json({ ok: true, status: "accepted", sessionId }, { headers: { "x-eve-session-id": sessionId } });
    }
    return accept(sessionId, body);
  }

  async function sendMessage(request: Request, sessionId: string): Promise<Response> {
    const body = await readJson(request);
    if (body.message === undefined && body.inputResponses === undefined) {
      return Response.json({ ok: true, status: "ready", sessionId });
    }
    return accept(sessionId, body);
  }

  async function liveTail(sessionId: string): Promise<number> {
    const response = await newt(sessionId, "");
    if (response.status === 404) {
      return 0;
    }
    if (!response.ok) {
      throw new Error(`newt session status failed (${response.status})`);
    }
    return ((await response.json()) as { live_seq: number }).live_seq;
  }

  async function streamSession(request: Request, sessionId: string): Promise<Response> {
    const query = new URL(request.url).searchParams;
    const startIndex = Number(query.get("startIndex") ?? "0");
    const follow = query.get("follow") !== "false";
    const abort = new AbortController();
    request.signal.addEventListener("abort", () => abort.abort());

    const tail = await liveTail(sessionId);
    const live = await newt(sessionId, "/live?cursor=0", { signal: abort.signal });
    if (!live.ok) {
      return failure(502, `newt live feed failed (${live.status})`);
    }
    const items = liveItems(live);
    const translator = createTranslator();
    const events: MessageStreamEvent[] = [];
    let lastSeq = 0;
    while (lastSeq < tail) {
      const next = await items.next();
      if (next.done) {
        break;
      }
      lastSeq = next.value.seq;
      events.push(...translator.push(next.value.seq, next.value.frame));
    }
    const from = startIndex < 0 ? Math.max(0, events.length + startIndex) : startIndex;
    const line = (event: unknown) => encoder.encode(`${JSON.stringify(event)}\n`);

    const headers = new Headers({
      "content-type": "application/x-ndjson; charset=utf-8",
      "cache-control": "no-store",
      "x-eve-stream-format": "ndjson",
      "x-eve-stream-version": STREAM_VERSION,
      "x-eve-session-id": sessionId,
    });
    if (query.get("includeTailIndex") === "1" || !follow) {
      headers.set("x-eve-stream-tail-index", String(events.length - 1));
    }

    const body = new ReadableStream<Uint8Array>({
      async start(controller) {
        try {
          for (const event of events.slice(from)) {
            controller.enqueue(line(event));
          }
          const nextItem = () => {
            const item = items.next();
            item.catch(() => {});
            return item;
          };
          let pending = nextItem();
          while (follow) {
            let timer: ReturnType<typeof setTimeout> | undefined;
            const heartbeat = new Promise<"heartbeat">((resolve) => {
              timer = setTimeout(() => resolve("heartbeat"), HEARTBEAT_MS);
            });
            const next = await Promise.race([pending, heartbeat]);
            clearTimeout(timer);
            if (next === "heartbeat") {
              controller.enqueue(encoder.encode("\n"));
              continue;
            }
            if (next.done) {
              break;
            }
            pending = nextItem();
            for (const event of translator.push(next.value.seq, next.value.frame)) {
              controller.enqueue(line(event));
            }
          }
          controller.close();
        } catch (error) {
          if (abort.signal.aborted) {
            return;
          }
          controller.error(error);
        }
      },
      cancel() {
        abort.abort();
      },
    });
    return new Response(body, { headers });
  }

  async function handle(request: Request): Promise<Response> {
    const match = new URL(request.url).pathname.match(ROUTE);
    if (!match) {
      return failure(404, "not an eve session route");
    }
    const sessionId = match[1] === undefined ? undefined : decodeURIComponent(match[1]);
    if (request.method === "POST" && sessionId === undefined) {
      return createSession(request);
    }
    if (request.method === "POST" && sessionId !== undefined && match[2] === undefined) {
      return sendMessage(request, sessionId);
    }
    if (request.method === "GET" && sessionId !== undefined && match[2] !== undefined) {
      return streamSession(request, sessionId);
    }
    return failure(404, "route not supported by the newt adapter");
  }

  return { createSession, sendMessage, streamSession, handle };
}
