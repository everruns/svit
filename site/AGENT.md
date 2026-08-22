# svit.everruns.com

Nimbus documentation site for Svit, deployed as Cloudflare static assets.

## Content contract

- `README.md`, `SECURITY.md`, `CHANGELOG.md`, and Markdown under `docs/` are
  canonical. Do not edit generated files under `src/content/docs/`.
- `pnpm sync:docs` recreates generated documentation and Git ignores it.
- The project index lives at `src/pages/index.astro`. Keep it factual and small:
  no feature marketing, demo reel, hero treatment, or duplicated README copy.
- Repository documentation assets are served through `public/docs-assets`, a
  symlink to `../docs`.

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

For a deliberate local deployment, use an authenticated Wrangler environment
and run `pnpm deploy` from this directory.
