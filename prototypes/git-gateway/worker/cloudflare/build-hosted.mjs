// SPDX-License-Identifier: Apache-2.0
import { build } from 'esbuild';
// Local JavaScript bundle only; this never calls Wrangler, Docker or Cloudflare APIs.
await build({ entryPoints: ['hosted-deploy.mjs'], outfile: 'dist/hosted-worker.mjs', bundle: true,
  format: 'esm', keepNames: true, platform: 'browser', target: 'es2022', external: ['cloudflare:workers'], logLevel: 'warning' });
console.log('Activation-disabled hosted Worker bundle written; no image build or deployment performed');
