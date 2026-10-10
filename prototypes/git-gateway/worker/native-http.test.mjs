const SERVICE = 'Bearer ' + 'S'.repeat(64);
import assert from 'node:assert/strict';
import test from 'node:test';
import { nativeHttp } from './native-http.mjs';
import { FRAME_MIME, encodeFrame, decodeFrame } from './native-frame.mjs';
const framed = payload => { const f = encodeFrame(payload); return new Response(f.body, { headers: { 'content-type': FRAME_MIME, 'content-length': String(f.length) } }); };

test('bridge has fixed internal endpoint and copies only explicit current user and configured service grant', async () => {
  let seen;
  const call = nativeHttp({ serviceAuthorization: SERVICE, binding: { async fetch(request) {
    seen = request; assert.equal(request.method, 'POST');
    assert.deepEqual((await decodeFrame(request)).payload, { method: 'authorize', repository: 'fixture', action: 'write' });
    return framed({ authorized: true });
  } } });
  assert.deepEqual((await call('authorize', { repository: 'fixture', action: 'write' }, 'Bearer test-fixture')).payload, { authorized: true });
  assert.equal(seen.url, 'http://native-container.invalid:8080/native/v1');
  assert.equal(seen.redirect, 'manual');
  assert.deepEqual([...seen.headers.keys()].sort(), ['authorization', 'content-length', 'content-type', 'x-gateway-service-authorization']);
  assert.equal(seen.headers.get('content-length'), String(encodeFrame({ method: 'authorize', repository: 'fixture', action: 'write' }).length));
  assert.equal(seen.headers.get('authorization'), 'Bearer test-fixture');
  assert.equal(seen.headers.get('x-gateway-service-authorization'), SERVICE);
});
test('native HTTP refuses redirects, compressed responses, wrong types and malformed data', async () => {
  for (const response of [new Response(null, { status: 302 }), new Response('{}', { headers: { 'content-type': 'text/plain' } }),
    new Response('{}', { headers: { 'content-type': 'application/json', 'content-encoding': 'gzip' } }),
    new Response('!', { headers: { 'content-type': 'application/json' } }),
    new Response('{}', { headers: { 'content-type': 'application/json', 'content-length': '999999999' } })]) {
    const call = nativeHttp({ serviceAuthorization: SERVICE, binding: { fetch: async () => response } });
    await assert.rejects(call('authorize', {}));
  }
});
test('bridge times out a service that ignores cancellation', async () => {
  const call = nativeHttp({ serviceAuthorization: SERVICE, timeoutMs: 5, binding: { fetch: () => new Promise(() => {}) } });
  await assert.rejects(call('authorize', {}), /timeout/);
});
test('bridge bounds authorization before forwarding any request', async () => {
  let calls = 0;
  const call = nativeHttp({ serviceAuthorization: SERVICE, binding: { fetch: async () => { calls++; return Response.json({}); } } });
  await assert.rejects(call('authorize', {}, 'a'.repeat(4097)));
  await assert.rejects(call('authorize', {}, 'Bearer a\nactor: forged'));
  assert.equal(calls, 0);
});
