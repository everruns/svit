const routeBySource = new Map([
  ["README.md", "/overview/"],
  ["docs/vision.md", "/vision/"],
  ["docs/control-protocol.md", "/control-protocol/"],
  ["SECURITY.md", "/security/"],
  ["CHANGELOG.md", "/changelog/"],
]);

export function normalizeMarkdownLinks(markdown: string): string {
  let normalized = markdown.replace(/^# .+\n+/, "");

  for (const [source, route] of routeBySource) {
    normalized = normalized
      .replaceAll(`](${source})`, `](${route})`)
      .replaceAll(`](../${source})`, `](${route})`);
  }

  return normalized
    .replace(/src="docs\/([^"#]+)"/g, 'src="/docs-assets/$1"')
    .replace(/!\[([^\]]*)\]\(docs\/([^)]+)\)/g, "![$1](/docs-assets/$2)");
}
