import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";
import starlightLlmsTxt from "starlight-llms-txt";

const site = process.env.SITE_URL ?? "http://localhost:4321";
const base = process.env.BASE_PATH ?? "/";
const ogImage = new URL(`${base.replace(/\/$/, "")}/og.png`, site).href;

export default defineConfig({
  site,
  base,
  trailingSlash: "always",
  build: { inlineStylesheets: "always" },
  vite: process.env.WATCH_POLL ? { server: { watch: { usePolling: true, interval: 250 } } } : {},
  integrations: [
    starlight({
      title: "newt",
      description: "An agent runtime for products: a long-lived agent per customer, your backend as its world.",
      favicon: "/favicon.svg",
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/JulianBouchard/newt" }],
      lastUpdated: true,
      head: [
        { tag: "meta", attrs: { property: "og:image", content: ogImage } },
        { tag: "meta", attrs: { property: "og:image:width", content: "1200" } },
        { tag: "meta", attrs: { property: "og:image:height", content: "630" } },
        { tag: "meta", attrs: { name: "twitter:card", content: "summary_large_image" } },
        { tag: "meta", attrs: { name: "twitter:image", content: ogImage } },
        { tag: "meta", attrs: { name: "author", content: "Julian Bouchard" } },
      ],
      customCss: [
        "@fontsource-variable/bricolage-grotesque",
        "@fontsource-variable/inter",
        "@fontsource-variable/jetbrains-mono",
        "./src/styles/theme.css",
      ],
      components: {
        Head: "./src/components/Head.astro",
        SiteTitle: "./src/components/SiteTitle.astro",
        TableOfContents: "./src/components/TableOfContents.astro",
        Search: "./src/components/Search.astro",
        PageTitle: "./src/components/PageTitle.astro",
        Footer: "./src/components/Footer.astro",
      },
      routeMiddleware: "./src/routeData.ts",
      plugins: [
        starlightLlmsTxt({
          projectName: "newt",
          description:
            "newt is an agent runtime for products: one long-lived agent per customer, your backend as its world, and a database's discipline about context. One Rust binary plus Postgres.",
          details: [
            "Every page is also served as Markdown at its path plus `.md`, for example `/quickstart/` at `/quickstart.md`.",
            "Start with Overview and Quickstart. Protocol and Configuration are the complete reference: when a guide and the Protocol differ, the Protocol is right.",
          ].join("\n\n"),
          promote: ["index", "overview", "quickstart", "how-a-turn-works", "home-server", "tools"],
          demote: ["changelog", "contributing", "running-tests", "benchmark", "security"],
          exclude: ["404"],
          customSets: [
            {
              label: "Build an app for newt",
              description: "what an application implements: the home MCP server, added servers, tools, events",
              paths: ["home-server", "servers", "tools", "skills", "memory", "events", "protocol"],
            },
            {
              label: "Run newt",
              description: "configuration, tracing, replay, load testing and what each major version keeps stable",
              paths: ["configuration", "tracing", "replay", "load-testing", "benchmark", "stability", "security"],
            },
          ],
          optionalLinks: [{ label: "Source code", url: "https://github.com/JulianBouchard/newt" }],
        }),
      ],
      sidebar: [
        { label: "Start", items: ["overview", "quickstart", "how-a-turn-works", "design"] },
        { label: "Build", items: ["home-server", "servers", "tools", "skills", "memory", "events"] },
        { label: "Run", items: ["configuration", "tracing", "replay", "load-testing", "benchmark"] },
        { label: "Reference", items: ["protocol", "stability", "changelog"] },
        { label: "Project", items: ["contributing", "running-tests", "security"] },
      ],
    }),
  ],
});
