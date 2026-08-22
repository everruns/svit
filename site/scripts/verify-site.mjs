import { access, readFile } from "node:fs/promises";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const pages = ["", "overview", "vision", "control-protocol", "security", "changelog"];
const failures = [];

async function read(path) {
  try {
    return await readFile(resolve(root, "dist", path), "utf8");
  } catch {
    failures.push(`missing dist/${path}`);
    return "";
  }
}

for (const slug of pages) {
  const directory = slug ? `${slug}/` : "";
  const html = await read(`${directory}index.html`);
  const markdown = await read(`${directory}index.md`);
  const canonical = `https://svit.everruns.com/${directory}`;

  for (const marker of [
    "<title>",
    'name="description"',
    `rel="canonical" href="${canonical}"`,
    'type="text/markdown"',
  ]) {
    if (!html.includes(marker)) failures.push(`${directory || "/"} lacks ${marker}`);
  }
  if (!markdown.startsWith("---\n")) {
    failures.push(`${directory || "/"} lacks Markdown frontmatter`);
  }
}

const robots = await read("robots.txt");
if (!robots.includes("Sitemap: https://svit.everruns.com/sitemap.xml")) {
  failures.push("robots.txt lacks the canonical sitemap URL");
}

for (const path of ["llms.txt", "llms-full.txt", "sitemap.xml"]) {
  await read(path);
}

const sitemap = await read("sitemap.xml");
const sitemapUrls = [...sitemap.matchAll(/<loc>[^<]+<\/loc>/g)].length;
const sitemapDates = [...sitemap.matchAll(/<lastmod>\d{4}-\d{2}-\d{2}<\/lastmod>/g)].length;
if (sitemapUrls === 0 || sitemapDates !== sitemapUrls) {
  failures.push(`sitemap.xml has ${sitemapUrls} URLs but ${sitemapDates} valid lastmod dates`);
}

for (const path of ["sitemap-index.xml", "sitemap-0.xml"]) {
  try {
    await access(resolve(root, "dist", path));
    failures.push(`dist/${path} should not be published`);
  } catch {
    // Expected: the build publishes one canonical sitemap.
  }
}

const combined = (await Promise.all(
  pages.map((slug) => read(`${slug ? `${slug}/` : ""}index.html`)),
)).join("\n");
for (const marker of ["example.com", "CHANGE_ME", "sitemap-index.xml"]) {
  if (combined.includes(marker)) failures.push(`built pages contain ${marker}`);
}

if (failures.length) {
  console.error(failures.map((failure) => `- ${failure}`).join("\n"));
  process.exitCode = 1;
} else {
  console.log(`Verified ${pages.length} HTML pages, Markdown twins, and discovery files.`);
}
