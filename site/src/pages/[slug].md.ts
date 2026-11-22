import type { APIRoute, GetStaticPaths } from "astro";
import { type CollectionEntry, getCollection } from "astro:content";
import { ARCHITECTURE_DESCRIPTION, NEWT_PLUS_MCP_DESCRIPTION, UNIFIED_MODEL_DESCRIPTION } from "../components/sketch-descriptions";

export const getStaticPaths = (async () => {
  const docs = await getCollection("docs", (entry) => entry.id !== "404");
  return docs.map((entry) => ({ params: { slug: entry.id }, props: { entry } }));
}) satisfies GetStaticPaths;

function mdxToMarkdown(body: string) {
  return body
    .replace(/^import .+$/gm, "")
    .replace("<ArchitectureSketch />", ARCHITECTURE_DESCRIPTION)
    .replace("<NewtPlusMcp />", NEWT_PLUS_MCP_DESCRIPTION)
    .replace("<UnifiedModelSketch />", UNIFIED_MODEL_DESCRIPTION)
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
