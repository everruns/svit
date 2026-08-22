import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, posix, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const siteRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(siteRoot, "..");
const targetDir = resolve(siteRoot, "src/content/docs");

const pages = [
  {
    source: "docs/overview.md",
    target: "overview.md",
    title: "Overview",
    description: "Public orientation, runtime model, and the shortest Rust path into Svit.",
    order: 1,
  },
  {
    source: "docs/vision.md",
    target: "vision.md",
    title: "Vision",
    description: "The runtime model, research direction, and explicit limits of Svit.",
    order: 5,
  },
  {
    source: "docs/memory.md",
    target: "memory.md",
    title: "Memory",
    description: "Durable process values, paths, transactions, snapshots, and forks.",
    order: 2,
  },
  {
    source: "docs/ports.md",
    target: "ports.md",
    title: "Ports",
    description: "Explicit host integrations available to Svit Lisp through typed contracts.",
    order: 3,
  },
  {
    source: "docs/events.md",
    target: "events.md",
    title: "Events",
    description: "Process commits, canonical reasoning history, messages, and transient observers.",
    order: 4,
  },
  {
    source: "docs/control-protocol.md",
    target: "control-protocol.md",
    title: "Control protocol",
    description: "Versioned atomic state transitions for multiple clients controlling one process.",
    order: 6,
  },
  {
    source: "SECURITY.md",
    target: "security.md",
    title: "Security",
    description: "Supported versions, threat boundaries, and vulnerability reporting.",
    order: 7,
  },
  {
    source: "CHANGELOG.md",
    target: "changelog.md",
    title: "Changelog",
    description: "Published changes to Svit.",
    order: 8,
  },
];

const routeBySource = new Map(
  pages.map((page) => [posix.normalize(page.source), `/${page.target.slice(0, -3)}/`]),
);

function splitTarget(target) {
  const hashIndex = target.indexOf("#");
  return hashIndex === -1
    ? { path: target, hash: "" }
    : { path: target.slice(0, hashIndex), hash: target.slice(hashIndex) };
}
function repositoryPath(source, target) {
  return posix.normalize(posix.join(posix.dirname(source), target));
}

function rewriteImages(markdown, source) {
  return markdown.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (match, alt, target) => {
    if (/^(?:https?:|data:|\/)/.test(target)) return match;
    const { path, hash } = splitTarget(target);
    const resolved = repositoryPath(source, path);
    if (resolved.startsWith("docs/")) {
      return `![${alt}](/docs-assets/${resolved.slice("docs/".length)}${hash})`;
    }
    return `![${alt}](https://raw.githubusercontent.com/everruns/svit/main/${resolved}${hash})`;
  });
}

function rewriteLinks(markdown, source) {
  return markdown.replace(/\]\(([^)\s]+)\)/g, (match, target) => {
    if (/^(?:https?:|mailto:|\/|#)/.test(target)) return match;
    const { path, hash } = splitTarget(target);
    const resolved = repositoryPath(source, path);
    const route = routeBySource.get(resolved);
    if (route) return `](${route}${hash})`;
    return `](https://github.com/everruns/svit/blob/main/${resolved}${hash})`;
  });
}

function forSite(markdown, source) {
  return rewriteLinks(rewriteImages(markdown, source), source);
}

function frontmatter(page) {
  return [
    "---",
    `title: ${JSON.stringify(page.title)}`,
    `description: ${JSON.stringify(page.description)}`,
    "sidebar:",
    `  order: ${page.order}`,
    "---",
    "",
  ].join("\n");
}

await rm(targetDir, { recursive: true, force: true });
await mkdir(targetDir, { recursive: true });

for (const page of pages) {
  const source = await readFile(resolve(repoRoot, page.source), "utf8");
  await writeFile(
    resolve(targetDir, page.target),
    `${frontmatter(page)}${forSite(source, page.source)}`,
  );
}
