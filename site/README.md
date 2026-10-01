# pgbx docs site

Astro Starlight. Pages live in `src/content/docs/` (Markdown/MDX); the sidebar is in `astro.config.mjs`.

## Dev

```sh
cd site
npm ci
npm run dev        # http://localhost:4321/pgbx/
```

## Build

```sh
npm run build      # static site + Pagefind search index in dist/
npm run preview
```

`site` and `base` come from env:

| var | default |
|---|---|
| `PUBLIC_SITE_URL` | `https://example.github.io` |
| `PUBLIC_BASE_PATH` | `/pgbx` |

## Deploy (GitHub Pages)

`.github/workflows/pages.yml` builds `site/` on every push to `main` that touches `site/**`
(or by hand via *Run workflow*) and deploys with `actions/deploy-pages`. It sets both env vars from the
repository owner and name.

Once, in the repo: **Settings → Pages → Source: GitHub Actions**.
For a custom domain, set `PUBLIC_SITE_URL=https://docs.example.com` and `PUBLIC_BASE_PATH=/` in the workflow.

Edit links are off until the repo exists: add `editLink: { baseUrl: 'https://github.com/<org>/pgbx/edit/main/site/' }`
to the Starlight config.

## Keep it true

Every function, setting and command here must match `src/lib.rs`, `cli/src/main.rs` and the README.
Change the code, change the page in the same PR.
