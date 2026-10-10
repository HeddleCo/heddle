// SPDX-License-Identifier: Apache-2.0
import { build } from 'esbuild';
// Bundle only; never call Wrangler, Docker, Cloudflare APIs, or a remote service.
await build({ entryPoints: ['index.mjs'], outfile: 'dist/worker.mjs', bundle: true,
  format: 'esm', keepNames: true, platform: 'browser', target: 'es2022', external: ['cloudflare:workers'],
  logLevel: 'warning' });
console.log('Disabled Worker bundle written; no deployment or Container build performed');
