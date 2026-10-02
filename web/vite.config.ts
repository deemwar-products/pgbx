import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// The build is embedded in the pgbx binary (cli/src/serve.rs includes web/dist/index.html,
// assets/index.js and assets/index.css by name), so the file names are fixed: no hashes, one chunk.
export default defineConfig({
  plugins: [react()],
  base: '/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    assetsInlineLimit: 0,
    cssCodeSplit: false,
    modulePreload: false,
    sourcemap: false,
    rollupOptions: {
      output: {
        entryFileNames: 'assets/index.js',
        chunkFileNames: 'assets/index.js',
        assetFileNames: (a) => (a.names?.some((n) => n.endsWith('.css')) ? 'assets/index.css' : 'assets/[name][extname]'),
        codeSplitting: false,
      },
    },
  },
  server: {
    // `npm run dev`: proxy the API to a running `pgbx serve --listen 127.0.0.1:8433`
    proxy: { '/api': 'http://127.0.0.1:8433' },
  },
});
