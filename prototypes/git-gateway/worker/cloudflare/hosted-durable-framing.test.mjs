// SPDX-License-Identifier: Apache-2.0
// Real workerd Durable Object transport and a real loopback socket. Container lifecycle
// remains a controlled fixture; this does not activate or deploy a Container.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { FRAME_MIME, encodeFrame, decodeFrame } from '../native-frame.mjs';

test('native HTTP frame crosses an actual Durable Object before the fixed-length socket hop', async t => {
  const received = [];
  const socket = createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    received.push({ headers: request.headers, body: Buffer.concat(chunks), path: request.url });
    const frame = encodeFrame({ authorized: true });
    response.writeHead(200, { 'content-type': FRAME_MIME, 'content-length': String(frame.length) });
    response.end(Buffer.from(await new Response(frame.body).arrayBuffer()));
  });
  socket.listen(0, '127.0.0.1'); await once(socket, 'listening');
  t.after(() => new Promise(resolve => socket.close(resolve)));
  const bundled = await build({ stdin: { resolveDir: dirname(fileURLToPath(import.meta.url)),
    sourcefile: 'durable-framing-fixture.mjs', contents: `
    import { DurableObject } from 'cloudflare:workers';
    import { nativeHttp } from '../native-http.mjs';
    import { hostedContainerFetch, nativeContainerBinding } from './hosted-container-runtime.mjs';
    const SERVICE = 'Bearer ' + 'S'.repeat(64);
    export class FixtureNativeContainer extends DurableObject {
      async fetch(request) {
        const config = { ...this.env, NATIVE_SERVICE_AUTHORIZATION: SERVICE,
          HOSTED_REPOSITORIES: JSON.stringify({ toy: {} }),
          HOSTED_NATIVE_CONFIG: JSON.stringify({ schema: 1, scope: { repository: 'toy' } }),
          GATEWAY_HOSTED_BISCUIT: 'PUBLIC_FIXTURE_ONLY', GATEWAY_HOSTED_SIGNER_PEM: 'PUBLIC_FIXTURE_ONLY',
          GATEWAY_HOSTED_SOURCE_AUTHOR_JSON: '{}', GATEWAY_ARTIFACTS_CREDENTIAL: 'PUBLIC_FIXTURE_ONLY' };
        const runtime = { id: this.ctx.id.toString(), running: false,
          startAndWaitForPorts: async () => {},
          containerFetch: (value, port) => { if (port !== 8080) throw new Error('port'); return this.env.SOCKET.fetch(value); } };
        return hostedContainerFetch(request, runtime, config, this.fixtureState ??= {},
          { activation: true, nativeNetwork: true });
      }
    }
    export default { async fetch(request, env) {
      let nativeStatus;
      const actual = nativeContainerBinding(env);
      const rpc = nativeHttp({ binding: { fetch: async request => {
        const response = await actual.fetch(request); nativeStatus = response.status; return response;
      } }, serviceAuthorization: SERVICE });
      try {
        const first = await rpc('authorize', { repository: 'toy', action: 'write', unicode_fixture: 'é☃' }, 'Bearer PUBLIC_USER_FIXTURE_ONLY');
        const bytes = Uint8Array.from({ length: 1048581 }, (_, i) => i % 256);
        const second = await rpc('prepare', { request_part: 'request' }, 'Bearer PUBLIC_USER_FIXTURE_ONLY', [{ name: 'request', bytes }]);
        return Response.json({ authorized: first.payload.authorized && second.payload.authorized });
      } catch { return Response.json({ native_status: nativeStatus }, { status: 502 }); }
    } };
  ` }, bundle: true, write: false, format: 'esm', platform: 'browser', target: 'es2022', external: ['cloudflare:workers'] });
  const mf = new Miniflare(convertV4MiniflareOptions({ name: 'hosted-durable-framing-test',
    script: bundled.outputFiles[0].text, modules: true, compatibilityDate: '2026-10-01',
    host: '127.0.0.1', port: 0, cf: false, telemetry: { enabled: false },
    durableObjects: { NATIVE_CONTAINER: { className: 'FixtureNativeContainer', useSQLite: true } },
    serviceBindings: { SOCKET: { external: { address: `127.0.0.1:${socket.address().port}`, http: {} } } },
    outboundService: () => new Response(null, { status: 502 }) }));
  t.after(() => mf.dispose()); await mf.ready;
  const response = await mf.dispatchFetch('http://fixture.invalid/');
  const result = await response.json();
  assert.equal(response.status, 200, JSON.stringify(result));
  assert.deepEqual(result, { authorized: true });
  assert.equal(received.length, 2);
  for (const item of received) {
    assert.equal(item.path, '/native/v1');
    assert.equal(item.headers.host, 'native-container.invalid:8080');
    assert.equal(item.headers['transfer-encoding'], undefined);
    assert.equal(Number(item.headers['content-length']), item.body.byteLength);
    assert.equal(item.headers['content-type'], FRAME_MIME);
    assert.equal(item.headers['x-gateway-service-authorization'], 'Bearer ' + 'S'.repeat(64));
  }
  const first = await decodeFrame(new Response(received[0].body, { headers: { 'content-type': FRAME_MIME } }));
  assert.deepEqual(first.payload, { action: 'write', method: 'authorize', repository: 'toy', unicode_fixture: 'é☃' });
  const second = await decodeFrame(new Response(received[1].body, { headers: { 'content-type': FRAME_MIME } }));
  assert.deepEqual(second.parts.get('request'), Uint8Array.from({ length: 1048581 }, (_, i) => i % 256));
});
