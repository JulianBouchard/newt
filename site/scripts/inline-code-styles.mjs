import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const DIST = join(dirname(fileURLToPath(import.meta.url)), "../dist");
const CODE_STYLESHEET = /<link rel="stylesheet" href="([^"]*\/(ec\.[\w-]+\.css))">/g;

let inlined = 0;
for (const file of readdirSync(DIST, { recursive: true })) {
  if (!file.endsWith(".html")) continue;
  const path = join(DIST, file);
  const html = readFileSync(path, "utf8");
  const links = [...html.matchAll(CODE_STYLESHEET)];
  if (links.length === 0) continue;
  if (!html.includes("</head>")) throw new Error(`inline-code-styles: ${file} has no </head>`);
  const css = readFileSync(join(DIST, "_astro", links[0][2]), "utf8");
  writeFileSync(path, html.replace(CODE_STYLESHEET, "").replace("</head>", `<style>${css}</style></head>`));
  inlined++;
}
console.log(`inline-code-styles: inlined the code block styles into <head> on ${inlined} pages`);
