import { defineRouteMiddleware } from "@astrojs/starlight/route-data";

const AUTHOR = { "@type": "Person", name: "Julian Bouchard" };

export const onRequest = defineRouteMiddleware((context) => {
  const route = context.locals.starlightRoute;
  const { entry } = route;
  if (entry.id === "404") return;
  const base = import.meta.env.BASE_URL.replace(/\/$/, "");
  const pageUrl = new URL(context.url.pathname, context.site).href;
  const homeUrl = new URL(`${base}/`, context.site).href;

  route.head.push({
    tag: "link",
    attrs: { rel: "alternate", type: "text/markdown", href: `${base}/${entry.id}.md`, title: `${entry.data.title} as Markdown` },
  });

  const structuredData =
    entry.id === "index"
      ? {
          "@context": "https://schema.org",
          "@type": "SoftwareSourceCode",
          name: "newt",
          description: entry.data.description,
          url: homeUrl,
          codeRepository: "https://github.com/JulianBouchard/newt",
          programmingLanguage: "Rust",
          license: ["https://spdx.org/licenses/Apache-2.0", "https://spdx.org/licenses/MIT"],
          author: AUTHOR,
        }
      : {
          "@context": "https://schema.org",
          "@type": "TechArticle",
          headline: entry.data.title,
          description: entry.data.description,
          url: pageUrl,
          dateModified: entry.data.lastUpdated instanceof Date ? entry.data.lastUpdated.toISOString() : undefined,
          author: AUTHOR,
          isPartOf: { "@type": "WebSite", name: "newt docs", url: homeUrl },
          encoding: { "@type": "MediaObject", encodingFormat: "text/markdown", contentUrl: new URL(`${base}/${entry.id}.md`, context.site).href },
        };
  route.head.push({ tag: "script", attrs: { type: "application/ld+json" }, content: JSON.stringify(structuredData) });
});
