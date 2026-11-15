import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, posix } from "node:path";
import { fileURLToPath } from "node:url";
import { RELEASE_HIGHLIGHTS } from "./release-highlights.mjs";

const SITE = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = join(SITE, "..");
const OUT = join(SITE, "src/content/docs/from-repo");
const ASSETS_OUT = join(SITE, "src/assets/generated");
const GITHUB = "https://github.com/JulianBouchard/newt";

const REFERENCE_FILES = [
  { slug: "design", file: "docs/design.md" },
  { slug: "protocol", file: "docs/protocol.md" },
  { slug: "configuration", file: "docs/config.md" },
  { slug: "benchmark", file: "docs/benchmark.md" },
  { slug: "contributing", file: "CONTRIBUTING.md" },
  { slug: "security", file: "SECURITY.md" },
  { slug: "changelog", file: "CHANGELOG.md" },
];

const SLUG_BY_FILE = Object.fromEntries(REFERENCE_FILES.map(({ slug, file }) => [file, slug]));
const TITLE_BY_SLUG = {};

function readRepoFile(file) {
  const path = join(REPO, file);
  if (!existsSync(path)) throw new Error(`sync-docs: ${file} is missing from the repo`);
  return readFileSync(path, "utf8");
}

function lastCommitDate(file) {
  const date = execFileSync("git", ["log", "-1", "--format=%cs", "--", file], { cwd: REPO, encoding: "utf8" }).trim();
  return date || undefined;
}

function resolveLink(target, sourceFile) {
  if (/^([a-z]+:|#|\/)/i.test(target)) return target;
  const [pathPart, hash = ""] = target.split("#");
  const repoPath = posix.normalize(posix.join(posix.dirname(sourceFile), pathPart));
  const anchor = hash ? `#${hash}` : "";
  if (repoPath === "README.md") return hash === "stability" ? "../stability/" : "../overview/";
  if (SLUG_BY_FILE[repoPath]) return `../${SLUG_BY_FILE[repoPath]}/${anchor}`;
  const absolute = join(REPO, repoPath);
  if (!existsSync(absolute)) throw new Error(`sync-docs: ${sourceFile} links to ${target}, which does not exist`);
  const kind = statSync(absolute).isDirectory() ? "tree" : "blob";
  return `${GITHUB}/${kind}/main/${repoPath}${anchor}`;
}

function linkFileMentions(line) {
  return line
    .replace(/`((?:docs\/[a-z-]+|CONTRIBUTING|SECURITY|CHANGELOG)\.md)`/g, (mention, file) =>
      SLUG_BY_FILE[file] ? `[${TITLE_BY_SLUG[SLUG_BY_FILE[file]]}](../${SLUG_BY_FILE[file]}/)` : mention,
    )
    .replace(/under Stability in the README/g, "on the [Stability](../stability/) page");
}

function transformBody(lines, sourceFile) {
  let inFence = false;
  return lines.map((line) => {
    if (/^\s*```/.test(line)) inFence = !inFence;
    if (inFence) return line;
    const linked = line.replace(/\]\(([^)\s]+)\)/g, (_, target) => `](${resolveLink(target, sourceFile)})`);
    return linkFileMentions(linked);
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
  const sentence = (plain.match(/^.+?\.(\s|$)/)?.[0].trim() ?? plain).replace(/:$/, ".");
  return sentence.length > 200 ? `${sentence.slice(0, 197)}...` : sentence;
}

function formatReleaseDate(date) {
  return new Date(`${date}T00:00:00Z`).toLocaleDateString("en-GB", { day: "numeric", month: "long", year: "numeric", timeZone: "UTC" });
}

function shapeChangelog(lines) {
  const out = [];
  lines.forEach((line, index) => {
    if (line === "## [Unreleased]") {
      const next = lines.slice(index + 1).find((candidate) => candidate.trim());
      if (!next || next.startsWith("## ")) return;
    }
    const release = line.match(/^## \[(\d+\.\d+\.\d+)\] - (\d{4}-\d{2}-\d{2})$/);
    if (!release) return out.push(line);
    const [, version, date] = release;
    out.push(`## ${version}`, "", `<p class="newt-release-date"><time datetime="${date}">${formatReleaseDate(date)}</time></p>`);
    const highlight = RELEASE_HIGHLIGHTS[version];
    if (highlight) out.push("", `:::tip[${highlight.title}]`, highlight.body, ":::");
  });
  const unknown = Object.keys(RELEASE_HIGHLIGHTS).filter((version) => !out.includes(`## ${version}`));
  if (unknown.length) throw new Error(`sync-docs: release highlights for versions not in CHANGELOG.md: ${unknown.join(", ")}`);
  return out;
}

function syncReferenceFile({ slug, file }) {
  const lines = readRepoFile(file).split("\n");
  const titleLine = lines.findIndex((line) => line.startsWith("# "));
  if (titleLine === -1) throw new Error(`sync-docs: ${file} has no # title`);
  const transformed = transformBody(lines.slice(titleLine + 1), file);
  const body = slug === "changelog" ? shapeChangelog(transformed) : transformed;
  const frontmatter = [`title: ${JSON.stringify(TITLE_BY_SLUG[slug])}`, `slug: ${slug}`];
  const description = firstSentence(body);
  if (description) frontmatter.push(`description: ${JSON.stringify(description)}`);
  const updated = lastCommitDate(file);
  if (updated) frontmatter.push(`lastUpdated: ${updated}`);
  frontmatter.push(`sourceFile: ${JSON.stringify(file)}`);
  if (slug === "changelog") frontmatter.push("tableOfContents:", "  maxHeadingLevel: 2");
  writeFileSync(join(OUT, `${slug}.md`), `---\n${frontmatter.join("\n")}\n---\n\n${body.join("\n").trim()}\n`);
}

function writeLogoVariants() {
  const lockup = readRepoFile("assets/logo/newt-lockup.svg").replace(/<style>.*?<\/style>/s, "");
  if (!lockup.includes('class="w"')) throw new Error("sync-docs: newt-lockup.svg no longer marks its wordmark with class=\"w\"");
  mkdirSync(ASSETS_OUT, { recursive: true });
  writeFileSync(join(ASSETS_OUT, "lockup-light.svg"), lockup.replaceAll('class="w"', 'stroke="#17181D"'));
  writeFileSync(join(ASSETS_OUT, "lockup-dark.svg"), lockup.replaceAll('class="w"', 'stroke="#F3EEE9"'));
  writeFileSync(join(ASSETS_OUT, "mark.svg"), readRepoFile("assets/logo/newt.svg"));
}

for (const { slug, file } of REFERENCE_FILES) {
  const title = readRepoFile(file).match(/^# (.+)$/m)?.[1].trim();
  if (!title) throw new Error(`sync-docs: ${file} has no # title`);
  TITLE_BY_SLUG[slug] = title;
}

rmSync(OUT, { recursive: true, force: true });
mkdirSync(OUT, { recursive: true });
REFERENCE_FILES.forEach(syncReferenceFile);
writeLogoVariants();
console.log(`sync-docs: wrote ${REFERENCE_FILES.length} reference pages`);
