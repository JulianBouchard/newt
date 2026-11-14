import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";
import starlightLlmsTxt from "starlight-llms-txt";

export default defineConfig({
  site: process.env.SITE_URL ?? "http://localhost:4321",
  base: process.env.BASE_PATH ?? "/",
  trailingSlash: "always",
  integrations: [
    starlight({
      title: "newt",
      description: "An agent runtime for products: a long-lived agent per customer, your backend as its world.",
      favicon: "/favicon.svg",
      logo: {
        light: "./src/assets/generated/lockup-light.svg",
        dark: "./src/assets/generated/lockup-dark.svg",
        replacesTitle: true,
      },
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/JulianBouchard/newt" }],
      customCss: [
        "@fontsource-variable/bricolage-grotesque",
        "@fontsource-variable/inter",
        "@fontsource-variable/jetbrains-mono",
        "./src/styles/theme.css",
      ],
      plugins: [starlightLlmsTxt()],
      sidebar: [
        { label: "Start", items: ["overview", "quickstart", "design"] },
        { label: "Build", items: ["context-provider", "tools", "skills", "memory"] },
        { label: "Run", items: ["configuration", "tracing", "replay", "load-testing", "benchmark"] },
        { label: "Reference", items: ["protocol", "changelog"] },
        { label: "Project", items: ["contributing", "running-tests", "security"] },
      ],
    }),
  ],
});
