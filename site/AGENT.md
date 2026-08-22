# svit.everruns.com

Nimbus documentation site for Svit, deployed as Cloudflare static assets.

## Content contract

- `README.md`, `SECURITY.md`, `CHANGELOG.md`, and Markdown under `docs/` are
  canonical. The public site overview comes from `docs/overview.md`; README
  remains repository documentation. Do not edit generated files under
  `src/content/docs/`.
- `pnpm sync:docs` recreates generated documentation and Git ignores it.
- The project index lives at `src/pages/index.astro`. Keep it factual and small:
  no feature marketing, demo reel, hero treatment, or duplicated README copy.
- Repository documentation assets are served through `public/docs-assets`, a
  symlink to `../docs`.
- Fenced `mermaid` blocks are marked during the Nimbus Markdown pass and
  rendered in the browser. Mermaid loads only on pages with diagrams, uses
  strict security mode, and leaves the source block visible on failure.
- Shiki emits light and dark token colors inline so static builds do not depend
  on Nimbus' generated class registry. The site verifier checks every Rust code
  block has complete dual-theme token styles.

## Validation

Run from `site/`:

```sh
pnpm typecheck
pnpm build
pnpm exec nimbus-docs check --json
pnpm exec wrangler deploy --dry-run
```

## Deployment

`wrangler.jsonc` owns the `svit.everruns.com` custom domain and static asset
binding. Cloudflare Workers Builds watches `everruns/svit` on `main`, runs from
`/site`, builds with `pnpm build`, and deploys with
`pnpm exec wrangler deploy`. Preview builds are disabled. Every push to `main`
that matches the unrestricted build watch path starts a production deployment.
The `Cloudflare Workers and Pages` GitHub App must include `everruns/svit` in
its selected repository access or GitHub push events will not reach the build
trigger.

For a deliberate local deployment, use an authenticated Wrangler environment
and run `pnpm deploy` from this directory.
