// SPDX-License-Identifier: Apache-2.0
// LOCAL INTEGRATION FIXTURE ONLY. All authority, native planning, acceptance,
// projection, and catalog writes go to the real Rust /native/v1 listener.
// R2 and Durable Object storage are actual local workerd bindings.
import { preparedHostedFetch, preparedHostedObjectFetch } from '../hosted-index.mjs';
import { HOSTED_CONTAINER_INSTANCE } from '../hosted-container-runtime.mjs';
import { MAX_FRAME_REQUEST } from '../../native-frame.mjs';

function integrations(env) {
  return { ...env, NATIVE_PUBLICATIONS: env.PUBLICATIONS,
    NATIVE_CONTAINER: { getByName(name) {
      if (name !== HOSTED_CONTAINER_INSTANCE) throw new Error('Fixture native instance mismatch');
      return { async fetch(request) {
        const length = request.headers.get('content-length');
        if (!/^[1-9][0-9]*$/.test(length ?? '') || Number(length) > MAX_FRAME_REQUEST || !request.body)
          return new Response(null, { status: 400 });
        // This fixture replaces the Container controller. Apply the same single
        // fixed-length stream at its physical hop, rather than losing Content-Length
        // when workerd forwards a generic stream to the Node service binding.
        const fixed = new FixedLengthStream(Number(length));
        void request.body.pipeTo(fixed.writable).catch(() => {});
        return env.NATIVE_HTTP.fetch(new Request(request.url, {
          method: request.method, headers: request.headers, body: fixed.readable,
        }));
      } };
    } },
    ARTIFACTS: { async get(name) {
      if (name !== env.CATALOG_NAME) throw new Error('Fixture catalog mismatch');
      return { async readFile({ ref, path }) {
        if (!/^[0-9a-f]{40}$/.test(ref) || path !== 'manifest.json') throw new Error('Fixture catalog read refused');
        const response = await env.CATALOG_READ.fetch(`http://catalog-fixture.invalid/manifest/${ref}`);
        if (response.status !== 200 || response.headers.get('content-type') !== 'application/json')
          throw new Error('Fixture catalog read unavailable');
        const bytes = await response.arrayBuffer();
        if (bytes.byteLength < 1 || bytes.byteLength > 65536) throw new Error('Fixture catalog metadata limit');
        return new Blob([bytes]);
      }, [Symbol.dispose]() {} };
    } },
  };
}

export class HostedRuntimeBridgeObject {
  constructor(ctx, env) { this.ctx = ctx; this.env = env; }
  fetch(request) { return preparedHostedObjectFetch(request, this.ctx, integrations(this.env)); }
}
export default { fetch(request, env) { return preparedHostedFetch(request, integrations(env)); } };
