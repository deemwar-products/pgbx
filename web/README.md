# web/ — the `pgbx serve` front end

Vite + React + TypeScript. Two runtime dependencies (`react`, `react-dom`). No CDN, no web fonts, no telemetry:
the app only calls the same-origin `/api/` of the `pgbx serve` that served it, and the server's CSP
(`connect-src 'self'`) enforces that.

## How it gets into the binary: the build is committed

`web/dist/` is **committed**. `cli/src/serve.rs` embeds `web/dist/index.html`, `web/dist/assets/index.js` and
`web/dist/assets/index.css` with `include_bytes!`, so `cargo build` (and the Docker image build, and a release
build) never needs node. Vite writes those fixed names (no hashes, one chunk; see `vite.config.ts`).

We chose this over a `build.rs` that embeds `dist` "when present": with that, a checkout without node would
build a binary whose `pgbx serve` silently has no app. A committed build always ships the same app.

**So: whenever you change anything in `web/src`, rebuild and commit `web/dist` in the same commit.**

```sh
cd web
npm ci
npm run build        # tsc --noEmit (typecheck), then vite build -> dist/
git add dist
```

`npm run typecheck` runs only the type check.

## Develop against a real server

```sh
cargo run --manifest-path cli/Cargo.toml -- serve --listen 127.0.0.1:8433 --no-open --profile X
cd web && npm run dev          # Vite proxies /api to 127.0.0.1:8433
```

Open the Vite URL with the token from the line `pgbx serve` printed: `http://localhost:5173/#token=...`.

## Layout

- `src/api.ts` — the only network code: token (taken from the `#token=` fragment, kept in sessionStorage for
  the tab), profile, `api()`; the response shapes.
- `src/App.tsx` — header (connection switcher, read-only / safe-actions badge, theme), hash routes.
- `src/screens/` — Overview, Db (database detail), Restore (restore helper), Query, Health.
- `src/ui.tsx` — formatting, tables that turn into cards at phone width, copy buttons, the confirm dialog.
- `src/styles.css` — the docs site palette (`site/src/product.ts`), light and dark.
