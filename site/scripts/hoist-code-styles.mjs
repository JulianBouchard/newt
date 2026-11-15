import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const DIST = join(dirname(fileURLToPath(import.meta.url)), "../dist");
const CODE_STYLESHEET = /<link rel="stylesheet" href="[^"]*\/ec\.[\w-]+\.css">/g;

let hoisted = 0;
for (const file of readdirSync(DIST, { recursive: true })) {
  if (!file.endsWith(".html")) continue;
  const path = join(DIST, file);
  const html = readFileSync(path, "utf8");
  const links = html.match(CODE_STYLESHEET);
  if (!links) continue;
  if (!html.includes("</head>")) throw new Error(`hoist-code-styles: ${file} has no </head>`);
  writeFileSync(path, html.replace(CODE_STYLESHEET, "").replace("</head>", `${links[0]}</head>`));
  hoisted++;
}
console.log(`hoist-code-styles: moved the code block stylesheet into <head> on ${hoisted} pages`);
