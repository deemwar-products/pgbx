// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// GitHub Pages: set PUBLIC_SITE_URL=https://<org>.github.io and PUBLIC_BASE_PATH=/pgbx
const site = process.env.PUBLIC_SITE_URL || 'https://example.github.io';
const base = process.env.PUBLIC_BASE_PATH || '/pgbx';

export default defineConfig({
  site,
  base,
  trailingSlash: 'always',
  integrations: [
    starlight({
      title: 'pgbx',
      description: "Create a database. It's backed up. Zero-touch Postgres backups to S3.",
      // editLink: add { baseUrl: 'https://github.com/<org>/pgbx/edit/main/site/' } once the repo exists
      sidebar: [
        { label: 'Getting started', items: [{ autogenerate: { directory: 'getting-started' } }] },
        { label: 'Concepts', items: [{ autogenerate: { directory: 'concepts' } }] },
        { label: 'Guides', items: [{ autogenerate: { directory: 'guides' } }] },
        { label: 'Reference', items: [{ autogenerate: { directory: 'reference' } }] },
        { label: 'For AI agents', items: [{ autogenerate: { directory: 'agents' } }] },
        { label: 'Testing & guarantees', slug: 'testing' },
      ],
    }),
  ],
});
