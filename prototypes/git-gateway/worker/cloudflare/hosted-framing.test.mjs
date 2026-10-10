// SPDX-License-Identifier: Apache-2.0
// Actual workerd HTTP serialization into a local socket. Container lifecycle is controlled;
// this tests the exact production nativeHttp/controller body pipeline, not a deployed Container.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { build } from 'esbuild';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { FRAME_MIME, encodeFrame, decodeFrame } from '../native-frame.mjs';
import { canonical } from '../publication.mjs';

const root = dirname(fileURLToPath(import.meta.url));
test('actual workerd sends exact Content-Length and canonical UTF-8 through the hosted controller', async t => {
  const seen = [];
  const socket = createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    const body = Buffer.concat(chunks); seen.push({ headers: request.headers, body, path: request.url });
    const frame = encodeFrame({ authorized: true });
    response.writeHead(200, { 'content-type': FRAME_MIME, 'content-length': String(frame.length) }); response.end(Buffer.from(await new Response(frame.body).arrayBuffer()));
  });
  socket.listen(0, '127.0.0.1'); await once(socket, 'listening');
  t.after(() => new Promise(resolve => socket.close(resolve)));
  const bundled = await build({ stdin: { resolveDir: root, sourcefile: 'framing-fixture.mjs', contents: `
    import { nativeHttp } from '../native-http.mjs';
    import { hostedContainerFetch, HOSTED_CONTAINER_INSTANCE } from './hosted-container-runtime.mjs';
    export default { async fetch(request, env) {
      let configuredServiceDigest; const state = {}, config = {
        NATIVE_CONTAINER: { idFromName: () => 'fixture-id' },
        NATIVE_SERVICE_AUTHORIZATION: 'Bearer ' + 'S'.repeat(64),
        HOSTED_REPOSITORIES: JSON.stringify({ toy: {} }),
        HOSTED_NATIVE_CONFIG: JSON.stringify({ schema: 1, scope: { repository: 'toy' } }),
        GATEWAY_HOSTED_BISCUIT: 'PUBLIC_FIXTURE_ONLY', GATEWAY_HOSTED_SIGNER_PEM: 'PUBLIC_FIXTURE_ONLY',
        GATEWAY_HOSTED_SOURCE_AUTHOR_JSON: '{}', GATEWAY_ARTIFACTS_CREDENTIAL: 'PUBLIC_FIXTURE_ONLY'
      };
      const runtime = { id: 'fixture-id', running: false, startAndWaitForPorts: async options => { configuredServiceDigest = JSON.parse(options.startOptions.envVars.GATEWAY_HOSTED_CONFIG_JSON).service_sha256; },
        containerFetch: (value, port) => { if (port !== 8080) throw new Error('port'); return env.SOCKET.fetch(value); } };
      const rpc = nativeHttp({ binding: { fetch: value => hostedContainerFetch(value, runtime, config, state,
        { activation: true, nativeNetwork: true }) }, serviceAuthorization: config.NATIVE_SERVICE_AUTHORIZATION });
      const first = await rpc('authorize', { repository: 'toy', action: 'write', unicode_fixture: 'é☃' }, 'Bearer PUBLIC_USER_FIXTURE_ONLY');
      const binary = Uint8Array.from({ length: 1048581 }, (_, i) => i % 256);
      const second = await rpc('prepare', { request_part: 'request' }, 'Bearer PUBLIC_USER_FIXTURE_ONLY', [{ name: 'request', bytes: binary }]);
      return Response.json({ authorized: first.payload.authorized && second.payload.authorized, service_sha256: configuredServiceDigest });
    } };
  ` }, bundle: true, write: false, format: 'esm', platform: 'browser', target: 'es2022' });
  const mf = new Miniflare(convertV4MiniflareOptions({ name: 'hosted-framing-local-test', script: bundled.outputFiles[0].text,
    modules: true, compatibilityDate: '2026-10-01', host: '127.0.0.1', port: 0, cf: false,
    telemetry: { enabled: false }, serviceBindings: { SOCKET: { external: { address: `127.0.0.1:${socket.address().port}`, http: {} } } },
    outboundService: () => new Response(null, { status: 502 }) }));
  t.after(() => mf.dispose()); await mf.ready;
  const response = await mf.dispatchFetch('http://fixture.invalid/');
  assert.equal(response.status, 200); assert.deepEqual(await response.json(), { authorized: true, service_sha256: 'eafca4c0fae9d1fc15080c4a3b9170bfbecec255164f6328fbf663279481e808' });
  assert.equal(seen.length, 2); const received = seen[0];
  assert.equal(received.path, '/native/v1');
  assert.equal(received.headers.host, 'native-container.invalid:8080');
  assert.equal(received.headers['transfer-encoding'], undefined);
  assert.equal(Number(received.headers['content-length']), received.body.byteLength);
  assert.equal(received.headers['content-type'], FRAME_MIME);
  const parsed = await decodeFrame(new Response(received.body, { headers: { 'content-type': FRAME_MIME } }));
  assert.deepEqual(parsed.payload, { action: 'write', method: 'authorize', repository: 'toy', unicode_fixture: 'é☃' });
  const headerLength = received.body.readUInt32BE(4);
  assert.equal(received.body.subarray(8, 8 + headerLength).toString(), canonical({ payload: parsed.payload, parts: [] }));
  assert.equal(received.headers.authorization, 'Bearer PUBLIC_USER_FIXTURE_ONLY');
  assert.equal(received.headers['x-gateway-service-authorization'], 'Bearer ' + 'S'.repeat(64));
  const binary = seen[1];
  assert.equal(binary.headers['transfer-encoding'], undefined);
  assert.equal(Number(binary.headers['content-length']), binary.body.byteLength);
  const uploaded = await decodeFrame(new Response(binary.body, { headers: { 'content-type': FRAME_MIME } }));
  assert.deepEqual(uploaded.payload, { method: 'prepare', request_part: 'request' });
  assert.deepEqual(uploaded.parts.get('request'), Uint8Array.from({ length: 1048581 }, (_, i) => i % 256));
});
