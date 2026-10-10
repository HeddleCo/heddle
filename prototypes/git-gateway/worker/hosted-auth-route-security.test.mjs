// SPDX-License-Identifier: Apache-2.0
// Independent tests of public PoP exchange routing. Cryptographic admission is
// the Weft verifier's responsibility; the Worker must not synthesize authority.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createFrontDoor } from './index.mjs';

const call = (path = '/git/auth/exchange', body = new Uint8Array([8, 1, 18, 0]), headers = {}, options = {}) =>
  new Request(`https://edge.test.invalid${path}`, { method: 'POST', body, duplex: 'half',
    headers: { 'content-type': 'application/x-protobuf', ...headers }, ...options });
const ok = body => new Response(body, { headers: { 'content-type': 'application/x-protobuf' } });

test('PoP routes forward exact bounded proof bytes only to the fixed authority service', async () => {
  for (const path of ['/git/auth/challenge', '/git/auth/exchange']) {
    const bytes = new Uint8Array(65536).fill(19), calls = [];
    const response = await createFrontDoor().fetch(call(path, bytes, {
      authorization: 'attacker-header', 'x-gateway-service-authorization': 'attacker-service',
      'x-heddle-actor': 'victim', cookie: 'session=attacker', 'content-length': '1',
    }), { AUTH: { async fetch(url, init) { calls.push({ url, init }); return ok(new Uint8Array([1, 2, 3])); } } });
    assert.equal(response.status, 200); assert.deepEqual(new Uint8Array(await response.arrayBuffer()), new Uint8Array([1, 2, 3]));
    assert.equal(calls.length, 1); assert.equal(calls[0].url, `https://authority.invalid${path}`);
    assert.equal(calls[0].init.redirect, 'manual');
    assert.deepEqual(calls[0].init.headers, { 'content-type': 'application/x-protobuf' });
    assert.deepEqual(calls[0].init.body, bytes);
    assert.equal(response.headers.get('cache-control'), 'no-store');
  }
});

test('auth routing rejects query, fragment, compressed or confused method input before service calls', async () => {
  const requests = [
    call('/git/auth/exchange?'), call('/git/auth/exchange?actor=owner'), call('/git/auth/exchange#'),
    call('/git/auth/exchange#fragment'), call('/git/auth/exchange/'), call('/git/auth/%65xchange'),
    call('/git/auth/challenge', 'bytes', { 'content-encoding': 'gzip' }),
    call('/git/auth/challenge', 'bytes', { 'content-type': 'application/json' }),
    call('/git/auth/challenge', 'bytes', { 'content-length': '65537' }),
    new Request('https://edge.test.invalid/git/auth/challenge'),
  ];
  for (const request of requests) {
    let calls = 0;
    const response = await createFrontDoor().fetch(request, { AUTH: { async fetch() { calls++; return ok('unexpected'); } } });
    assert.ok(response.status >= 400, request.url); assert.equal(calls, 0, request.url);
    assert.equal(response.headers.get('cache-control'), 'no-store');
  }
});

test('oversized actual PoP request and response cannot pass spoofed short content lengths', async () => {
  let calls = 0;
  const requestTooLarge = await createFrontDoor().fetch(call('/git/auth/exchange', new Uint8Array(65537), { 'content-length': '1' }), {
    AUTH: { async fetch() { calls++; return ok('unexpected'); } },
  });
  assert.ok(requestTooLarge.status >= 400); assert.equal(calls, 0);
  const responseTooLarge = await createFrontDoor().fetch(call(), {
    AUTH: { async fetch() { return new Response(new Uint8Array(16385), { headers: { 'content-type': 'application/x-protobuf', 'content-length': '1' } }); } },
  });
  assert.ok(responseTooLarge.status >= 400);
});

test('redirect and non-protobuf auth responses do not expose service cookies, locations or credential bytes', async () => {
  for (const [status, type, encoding] of [[302, 'application/x-protobuf', null], [200, 'text/html', null], [200, 'application/x-protobuf', 'gzip']]) {
    let cancelled = false;
    const headers = new Headers({ 'content-type': type, location: 'https://attacker.invalid', 'set-cookie': 'transport=secret' });
    if (encoding) headers.set('content-encoding', encoding);
    const response = await createFrontDoor().fetch(call(), { AUTH: { async fetch() {
      return new Response(new ReadableStream({ cancel() { cancelled = true; } }), { status, headers });
    } } });
    assert.ok(response.status >= 400); assert.equal(cancelled, true);
    assert.equal(response.headers.get('location'), null); assert.equal(response.headers.get('set-cookie'), null);
    assert.equal(await response.text(), '');
  }
});

test('stalled challenge/exchange work is cancelled and cannot issue a late response as success', { timeout: 1000 }, async () => {
  let resolve, cancelled = false, authoritySignal;
  const front = createFrontDoor({ timeoutMs: 10 });
  const response = await front.fetch(call(), { AUTH: { async fetch(_url, init) {
    authoritySignal = init.signal; return new Promise(done => { resolve = done; });
  } } });
  assert.equal(response.status, 503); assert.equal(authoritySignal.aborted, true);
  resolve(new Response(new ReadableStream({ cancel() { cancelled = true; } }), { headers: { 'content-type': 'application/x-protobuf' } }));
  await new Promise(done => setTimeout(done, 0));
  assert.equal(cancelled, true);
});
