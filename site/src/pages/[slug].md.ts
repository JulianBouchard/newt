import type { APIRoute, GetStaticPaths } from "astro";
import { type CollectionEntry, getCollection } from "astro:content";
import * as descriptions from "../components/sketch-descriptions";

const SKETCHES: Record<string, string> = {
  ArchitectureSketch: descriptions.ARCHITECTURE_DESCRIPTION,
  NewtPlusMcp: descriptions.NEWT_PLUS_MCP_DESCRIPTION,
  UnifiedModelSketch: descriptions.UNIFIED_MODEL_DESCRIPTION,
  PromptLayersSketch: descriptions.PROMPT_LAYERS_DESCRIPTION,
  RetentionSketch: descriptions.RETENTION_DESCRIPTION,
  WorkspaceSketch: descriptions.WORKSPACE_DESCRIPTION,
  CompactionSketch: descriptions.COMPACTION_DESCRIPTION,
  JournalSketch: descriptions.JOURNAL_DESCRIPTION,
  RunnerSketch: descriptions.RUNNER_DESCRIPTION,
  PiecesSketch: descriptions.PIECES_DESCRIPTION,
};

export const getStaticPaths = (async () => {
  const docs = await getCollection("docs", (entry) => entry.id !== "404");
  return docs.map((entry) => ({ params: { slug: entry.id }, props: { entry } }));
}) satisfies GetStaticPaths;

function mdxToMarkdown(body: string) {
  return body
    .replace(/^import .+$/gm, "")
    .replace(/^<(\w+) \/>$/gm, (_, name: string) => {
      const description = SKETCHES[name];
      if (!description) throw new Error(`[slug].md: no text version for <${name} />`);
      return description;
    })
    .replace(/^<TurnGroup label="(\w+)">$/gm, (_, label: string) => `**${label[0].toUpperCase()}${label.slice(1)}**`)
    .replace(/^<\/TurnGroup>$/gm, "")
    .replace(/<\/?CardGrid>/g, "\n")
    .replace(/^<\/?div[^>]*>$/gm, "")
    .replace(/<Card title="([^"]+)"[^>]*>/g, "### $1\n")
    .replace(/<\/Card>/g, "")
    .replace(/^[ \t]*<LinkCard title="([^"]+)" description="([^"]+)" href="([^"]+)" \/>/gm, "- [$1]($3): $2")
    .replace(/\n{3,}/g, "\n\n");
}

function absolutizeLinks(markdown: string, pageUrl: URL) {
  return markdown.replace(/\]\(([^)\s]+)\)/g, (_, target: string) => `](${new URL(target, pageUrl).href})`);
}

export const GET: APIRoute<{ entry: CollectionEntry<"docs"> }> = ({ props: { entry }, site }) => {
  const base = import.meta.env.BASE_URL.replace(/\/$/, "");
  const pageUrl = new URL(entry.id === "index" ? `${base}/` : `${base}/${entry.id}/`, site);
  const title = entry.data.hero?.title ?? entry.data.title;
  const source = entry.filePath?.endsWith(".mdx") ? mdxToMarkdown(entry.body ?? "") : (entry.body ?? "");
  const body = source.replace(/^<p class="newt-release-date"><time[^>]*>(.+)<\/time><\/p>$/gm, "Released $1.");
  const header = [`# ${title}`, entry.data.description && `> ${entry.data.description}`, `Source: ${pageUrl.href}`, entry.data.sourceFile && `Synced from \`${entry.data.sourceFile}\` in the newt repository.`]
    .filter(Boolean)
    .join("\n\n");
  return new Response(`${header}\n\n${absolutizeLinks(body.trim(), pageUrl)}\n`, {
    headers: { "content-type": "text/markdown; charset=utf-8" },
  });
};
