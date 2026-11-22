import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { HookContext } from "eve/hooks";
import { defineTool } from "eve/tools";
import { z } from "zod";
import { readFile } from "node:fs/promises";

const mockUrl = (): string => process.env.BENCH_MOCK_URL ?? "http://127.0.0.1:9200";

type Bootstrap = {
  instructions: string;
  skills: { name: string; description: string; content: string }[];
};

type Page = { path: string; content: string };

export async function bootstrap(): Promise<Bootstrap> {
  const client = await mcp();
  const instructions = client.getInstructions() ?? "";
  const profile = JSON.parse(await readFile(profileFile(), "utf8")) as { skills: string[] };
  const skills = await Promise.all(
    profile.skills.map(async (name) => ({
      name,
      description: `How to handle ${name} work.`,
      content: withoutFrontmatter(await readText(client, `skill://${name}/SKILL.md`)),
    })),
  );
  return { instructions, skills };
}

export async function turnPages(): Promise<Page[]> {
  const client = await mcp();
  const { resources } = await client.listResources();
  return Promise.all(resources.map(async ({ uri, name }) => ({ path: name, content: await readText(client, uri) })));
}

function profileFile(): string {
  const path = process.env.BENCH_PROFILE_FILE;
  if (!path) throw new Error("BENCH_PROFILE_FILE is not set");
  return path;
}

async function readText(client: Client, uri: string): Promise<string> {
  const { contents } = await client.readResource({ uri });
  return contents.map((part) => ("text" in part ? part.text : "")).join("");
}

function withoutFrontmatter(file: string): string {
  return file.replace(/^---\n[\s\S]*?\n---\n\n?/, "");
}

let connected: Promise<Client> | undefined;

function mcp(): Promise<Client> {
  connected ??= (async () => {
    const client = new Client({ name: "bench-eve", version: "1" });
    await client.connect(new StreamableHTTPClientTransport(new URL(`${mockUrl()}/mcp`)));
    return client;
  })();
  return connected;
}

export function mockTool(name: string) {
  return defineTool({
    description: "A tool mocked for load tests.",
    inputSchema: z.object({ text: z.string() }),
    async execute(input) {
      const client = await mcp();
      const result = await client.callTool({ name, arguments: input });
      const text = (result.content as { type: string; text?: string }[])
        .map((part) => part.text ?? "")
        .join("");
      if (result.isError) throw new Error(text);
      return text;
    },
  });
}

export function noopTool() {
  return defineTool({
    description: "A tool that only records that it was called.",
    inputSchema: z.object({ text: z.string() }),
    async execute() {
      return "ok";
    },
  });
}

const pagesLoading = new Map<string, Promise<void>>();

export function startPages(ctx: HookContext): void {
  const loading = (async () => {
    const [pages, sandbox] = await Promise.all([turnPages(), ctx.getSandbox()]);
    await Promise.all(pages.map(({ path, content }) => sandbox.writeTextFile({ path, content })));
  })();
  loading.catch(() => undefined);
  pagesLoading.set(ctx.session.id, loading);
}

export function afterPages<Definition extends { execute: (input: never, ctx: never) => unknown }>(
  definition: Definition,
): Definition {
  return {
    ...definition,
    async execute(input: never, ctx: { session: { id: string } }) {
      await pagesLoading.get(ctx.session.id);
      return definition.execute(input, ctx as never);
    },
  };
}
