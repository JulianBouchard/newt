import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, posix } from "node:path";
import { fileURLToPath } from "node:url";

const SITE = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = join(SITE, "..");
const OUT = join(SITE, "src/content/docs");
const ASSETS_OUT = join(SITE, "src/assets/generated");
const GITHUB = "https://github.com/JulianBouchard/newt";

const README_SECTIONS = [
  { slug: "quickstart", heading: "Try it: a personal assistant", title: "Quickstart" },
  { slug: "context-provider", heading: "Context provider", title: "Context provider" },
  { slug: "tools", heading: "Tools over MCP", title: "Tools over MCP" },
  { slug: "skills", heading: "Skills", title: "Skills" },
  { slug: "memory", heading: "Memory and compaction", title: "Memory and compaction" },
  { slug: "replay", heading: "Replay and fork", title: "Replay and fork" },
  { slug: "tracing", heading: "Tracing", title: "Tracing" },
  { slug: "load-testing", heading: "Load testing without paying for tokens", title: "Load testing" },
  { slug: "running-tests", heading: "Running the tests", title: "Running the tests" },
];

const WHOLE_FILES = [
  { slug: "design", file: "docs/design.md" },
  { slug: "protocol", file: "docs/protocol.md" },
  { slug: "configuration", file: "docs/config.md" },
  { slug: "benchmark", file: "docs/benchmark.md" },
  { slug: "contributing", file: "CONTRIBUTING.md" },
  { slug: "security", file: "SECURITY.md" },
  { slug: "changelog", file: "CHANGELOG.md" },
];

const SLUG_BY_FILE = Object.fromEntries(WHOLE_FILES.map(({ slug, file }) => [file, slug]));

function githubSlug(heading) {
  return heading
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");
}

const README_SLUG_BY_ANCHOR = Object.fromEntries(
  README_SECTIONS.map(({ slug, heading }) => [githubSlug(heading), slug]),
);

function readRepoFile(file) {
  const path = join(REPO, file);
  if (!existsSync(path)) throw new Error(`sync-docs: ${file} is missing from the repo`);
  return readFileSync(path, "utf8");
}

function resolveLink(target, sourceFile) {
  if (/^([a-z]+:|#|\/)/i.test(target)) return target;
  const [pathPart, hash = ""] = target.split("#");
  const repoPath = posix.normalize(posix.join(posix.dirname(sourceFile), pathPart));
  const anchor = hash ? `#${hash}` : "";
  if (repoPath === "README.md") {
    const sectionSlug = README_SLUG_BY_ANCHOR[hash];
    return sectionSlug ? `../${sectionSlug}/` : `../overview/${anchor}`;
  }
  if (SLUG_BY_FILE[repoPath]) return `../${SLUG_BY_FILE[repoPath]}/${anchor}`;
  const absolute = join(REPO, repoPath);
  if (!existsSync(absolute)) throw new Error(`sync-docs: ${sourceFile} links to ${target}, which does not exist`);
  const kind = statSync(absolute).isDirectory() ? "tree" : "blob";
  return `${GITHUB}/${kind}/main/${repoPath}${anchor}`;
}

function transformBody(lines, sourceFile, { promoteHeadings }) {
  let inFence = false;
  return lines.map((line) => {
    if (/^\s*```/.test(line)) inFence = !inFence;
    if (inFence) return line;
    let out = line.replace(/\]\(([^)\s]+)\)/g, (_, target) => `](${resolveLink(target, sourceFile)})`);
    if (promoteHeadings) out = out.replace(/^#(#{2,5}) /, "$1 ");
    return out;
  });
}

function firstSentence(lines) {
  let inFence = false;
  const paragraph = lines.find((line) => {
    if (/^\s*```/.test(line)) {
      inFence = !inFence;
      return false;
    }
    return !inFence && /^[A-Za-z`*]/.test(line.trim());
  });
  if (!paragraph) return undefined;
  const plain = paragraph
    .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1")
    .replace(/[`*]/g, "")
    .trim();
  const sentence = plain.match(/^.+?\.(\s|$)/)?.[0].trim() ?? plain;
  return sentence.length > 200 ? `${sentence.slice(0, 197)}...` : sentence;
}

function writePage(slug, title, bodyLines) {
  const description = firstSentence(bodyLines);
  const frontmatter = [`title: ${JSON.stringify(title)}`];
  if (description) frontmatter.push(`description: ${JSON.stringify(description)}`);
  const body = bodyLines.join("\n").trim();
  writeFileSync(join(OUT, `${slug}.md`), `---\n${frontmatter.join("\n")}\n---\n\n${body}\n`);
}

function splitReadme() {
  const lines = readRepoFile("README.md").split("\n");
  const sectionStarts = [];
  let inFence = false;
  lines.forEach((line, index) => {
    if (/^\s*```/.test(line)) inFence = !inFence;
    if (!inFence && line.startsWith("## ")) sectionStarts.push({ heading: line.slice(3).trim(), index });
  });
  if (sectionStarts.length === 0) throw new Error("sync-docs: README.md has no ## sections");

  const intro = lines.slice(1, sectionStarts[0].index);
  writePage("overview", "Overview", transformBody(intro, "README.md", { promoteHeadings: true }));

  for (const { slug, heading, title } of README_SECTIONS) {
    const position = sectionStarts.findIndex((start) => start.heading === heading);
    if (position === -1) throw new Error(`sync-docs: README.md has no "## ${heading}" section for /${slug}/`);
    const end = sectionStarts[position + 1]?.index ?? lines.length;
    const body = lines.slice(sectionStarts[position].index + 1, end);
    writePage(slug, title, transformBody(body, "README.md", { promoteHeadings: true }));
  }
}

function copyWholeFiles() {
  for (const { slug, file } of WHOLE_FILES) {
    const lines = readRepoFile(file).split("\n");
    const titleLine = lines.findIndex((line) => line.startsWith("# "));
    if (titleLine === -1) throw new Error(`sync-docs: ${file} has no # title`);
    const title = lines[titleLine].slice(2).trim();
    writePage(slug, title, transformBody(lines.slice(titleLine + 1), file, { promoteHeadings: false }));
  }
}

function writeLogoVariants() {
  const lockup = readRepoFile("assets/logo/newt-lockup.svg").replace(/<style>.*?<\/style>/s, "");
  if (!lockup.includes('class="w"')) throw new Error("sync-docs: newt-lockup.svg no longer marks its wordmark with class=\"w\"");
  mkdirSync(ASSETS_OUT, { recursive: true });
  writeFileSync(join(ASSETS_OUT, "lockup-light.svg"), lockup.replaceAll('class="w"', 'stroke="#17181D"'));
  writeFileSync(join(ASSETS_OUT, "lockup-dark.svg"), lockup.replaceAll('class="w"', 'stroke="#F3EEE9"'));
  writeFileSync(join(ASSETS_OUT, "mark.svg"), readRepoFile("assets/logo/newt.svg"));
}

for (const entry of readdirSync(OUT)) {
  if (entry !== "index.mdx") rmSync(join(OUT, entry), { recursive: true });
}
splitReadme();
copyWholeFiles();
writeLogoVariants();
console.log(`sync-docs: wrote ${README_SECTIONS.length + WHOLE_FILES.length + 1} pages`);
