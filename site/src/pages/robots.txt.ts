import type { APIRoute } from "astro";

export const GET: APIRoute = ({ site }) => {
  const base = import.meta.env.BASE_URL.replace(/\/$/, "");
  const url = (path: string) => new URL(`${base}/${path}`, site).href;
  const body = [
    "User-agent: *",
    "Allow: /",
    "",
    `Sitemap: ${url("sitemap-index.xml")}`,
    "",
    `# Every page is also served as Markdown at its path plus .md, for example ${url("quickstart.md")}`,
    `# Index for LLMs: ${url("llms.txt")}`,
    "",
  ].join("\n");
  return new Response(body, { headers: { "content-type": "text/plain; charset=utf-8" } });
};
