// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { gatewayTransport } from './gateway-transport.mjs';

// Deliberately public, non-credential vectors; every fetch is an injected double.
const PUBLIC_TEST_VECTOR_SERVICE = 'PUBLIC_TEST_VECTOR_SERVICE_IDENTITY_0001';
const PUBLIC_TEST_VECTOR_READER = 'Bearer PUBLIC_TEST_VECTOR_READER';
const PUBLIC_TEST_VECTOR_PIN = 'a'.repeat(40);
const ORIGIN = 'https://native-gateway.example.invalid:8443';
const BASE = `https://gateway.invalid/views/${PUBLIC_TEST_VECTOR_PIN}.git`;
const DISCOVERY = `${BASE}/info/refs?service=git-upload-pack`;
const UPLOAD = `${BASE}/git-upload-pack`;
const REQUEST_LIMIT = 1024 * 1024;
const RESPONSE_LIMIT = 96 * 1024 * 1024;
function adapter(fetchImpl, options = {}) {
  return gatewayTransport({ origin: ORIGIN, serviceCredential: PUBLIC_TEST_VECTOR_SERVICE, fetchImpl, ...options });
}
function upload(body, headers = {}, options = {}) {
  return new Request(UPLOAD, { method: 'POST', body, duplex: 'half',
    headers: { 'content-type': 'application/x-git-upload-pack-request', ...headers }, ...options });
}
function stream(chunks, onCancel = () => {}) {
  return new ReadableStream({
    pull(controller) {
      if (chunks.length) controller.enqueue(chunks.shift());
      else controller.close();
    },
    cancel: onCancel,
  }, { highWaterMark: 0 });
}

test('configuration requires a bare HTTPS origin and an explicit existing service credential', () => {
  for (const origin of [undefined, '', 'http://localhost:8042', 'ftp://host', 'https://host/path',
    'https://host//', 'https://host/.', 'https://user:pass@host', 'https://@host',
    'https://host?x=1', 'https://host?', 'https://host#x', 'https://host#', 'https://host\\escape',
    ' https://host', 'https://host\n', 'https://', '//host']) {
    assert.throws(() => adapter(async () => new Response(), { origin }), undefined, String(origin));
  }
  for (const serviceCredential of [undefined, '', 'x'.repeat(31), 'x'.repeat(257),
    'x'.repeat(32) + '=', 'x'.repeat(32) + '\n', 'Bearer ' + 'x'.repeat(32), 123]) {
    assert.throws(() => adapter(async () => new Response(), { serviceCredential }));
  }
  for (const timeoutMs of [0, -1, Infinity, 1.5, 120001])
    assert.throws(() => adapter(async () => new Response(), { timeoutMs }));
  assert.throws(() => adapter(null));
  assert.throws(() => gatewayTransport());
});

test('fixed origin, separate service identity, exact allowlists and no cookies or redirect following', async () => {
  let calls = 0;
  const gateway = adapter(async request => {
    calls++;
    assert.equal(request.url, `${ORIGIN}/views/${PUBLIC_TEST_VECTOR_PIN}.git/info/refs?service=git-upload-pack`);
    assert.equal(request.method, 'GET');
    assert.equal(request.redirect, 'manual');
    assert.equal(request.credentials, 'omit');
    assert.equal(request.cache, 'no-store');
    assert.equal(request.referrerPolicy, 'no-referrer');
    assert.deepEqual([...request.headers], [
      ['authorization', PUBLIC_TEST_VECTOR_READER], ['git-protocol', 'version=2'],
      ['x-gateway-service-authorization', `Bearer ${PUBLIC_TEST_VECTOR_SERVICE}`],
    ]);
    return new Response('PUBLIC_TEST_VECTOR_GIT_BYTES', { headers: {
      'content-type': 'application/x-git-upload-pack-advertisement', 'set-cookie': 'private-cookie',
      'location': 'https://elsewhere.invalid', 'x-private': 'private', 'www-authenticate': 'private',
      'content-length': '28', 'cache-control': 'public',
    } });
  }, { origin: ORIGIN + '/' });
  const response = await gateway.fetch(new Request(DISCOVERY, { headers: {
    authorization: PUBLIC_TEST_VECTOR_READER, 'git-protocol': 'version=2',
    'x-gateway-service-authorization': 'Bearer PUBLIC_TEST_VECTOR_FORGED_SERVICE',
    'x-demo-reader': 'forged', 'x-demo-identity': 'forged', 'x-forwarded-for': '127.0.0.1',
    forwarded: 'for=127.0.0.1', cookie: 'cookie', host: 'elsewhere.invalid',
    origin: 'https://attacker.invalid', referer: 'https://attacker.invalid',
  } }));
  assert.equal(response.status, 200);
  assert.equal(await response.text(), 'PUBLIC_TEST_VECTOR_GIT_BYTES');
  assert.deepEqual([...response.headers], [['cache-control', 'no-store'],
    ['content-type', 'application/x-git-upload-pack-advertisement']]);
  assert.equal(calls, 1);
});

test('invalid routes, methods, query strings, fragments and origins never reach fetch', async () => {
  let calls = 0;
  const gateway = adapter(async () => { calls++; return new Response(); });
  for (const [url, method = 'GET'] of [
    [DISCOVERY, 'POST'], [DISCOVERY, 'HEAD'], [DISCOVERY, 'PUT'], [UPLOAD], [UPLOAD, 'DELETE'],
    [`${BASE}/info/refs`], [`${BASE}/info/refs?service=git-receive-pack`],
    [`${BASE}/info/refs?service=git-upload-pack&extra=1`], [`${BASE}/info/refs?service=git-upload-pack&service=git-upload-pack`],
    [`${BASE}/info/refs?service=git%2dupload-pack`], [`${UPLOAD}?`, 'POST'],
    [`${UPLOAD}?service=git-upload-pack`, 'POST'], [`${BASE}/git-receive-pack`, 'POST'],
    [`${BASE}/objects/aa`], [DISCOVERY + '#'], [DISCOVERY + '#fragment'],
    [DISCOVERY.replace('gateway.invalid', 'elsewhere.invalid')], [DISCOVERY.replace('https:', 'http:')],
    [DISCOVERY.replace(PUBLIC_TEST_VECTOR_PIN, 'A'.repeat(40))],
    [DISCOVERY.replace('/info/refs', '/info%2frefs')], [DISCOVERY.replace('/info/refs', '/info/refs/')],
  ]) {
    const response = await gateway.fetch(new Request(url, { method }));
    assert.ok([404, 405].includes(response.status), `${method} ${url}`);
  }
  assert.equal((await gateway.fetch('https://gateway.invalid')).status, 400);
  assert.equal(calls, 0);
});

test('POST bytes and exactly 1 MiB body pass with a computed content-length', async () => {
  const body = new Uint8Array(REQUEST_LIMIT).fill(37);
  const gateway = adapter(async request => {
    assert.equal(request.headers.get('content-length'), String(REQUEST_LIMIT));
    assert.equal(request.headers.get('content-type'), 'application/x-git-upload-pack-request');
    assert.deepEqual(new Uint8Array(await request.arrayBuffer()), body);
    return new Response('0000', { headers: { 'content-type': 'application/x-git-upload-pack-result' } });
  });
  const response = await gateway.fetch(upload(body, { 'content-length': String(REQUEST_LIMIT) }));
  assert.equal(response.status, 200); assert.equal(await response.text(), '0000');
});

test('POST framing is computed from actual bytes, ignoring caller length and transfer-encoding', async () => {
  const bytes = new TextEncoder().encode('PUBLIC_TEST_VECTOR_UPLOAD');
  let calls = 0;
  const gateway = adapter(async request => {
    calls++;
    assert.equal(request.headers.get('content-length'), String(bytes.byteLength));
    assert.equal(request.headers.get('transfer-encoding'), null);
    assert.deepEqual(new Uint8Array(await request.arrayBuffer()), bytes);
    return new Response(null, { status: 204 });
  });
  for (const headers of [{}, { 'content-length': '0' }, { 'content-length': '1' },
    { 'content-length': '1000' }, { 'content-length': '1', 'transfer-encoding': 'chunked' }]) {
    assert.equal((await gateway.fetch(upload(bytes, headers))).status, 204);
  }
  assert.equal(calls, 5);
  const empty = adapter(async request => {
    assert.equal(request.headers.get('content-length'), '0');
    assert.equal((await request.arrayBuffer()).byteLength, 0);
    return new Response(null, { status: 204 });
  });
  assert.equal((await empty.fetch(upload(null))).status, 204);
});

test('invalid or oversized declared requests and compressed/wrongly typed requests fail closed', async () => {
  let calls = 0;
  const gateway = adapter(async () => { calls++; return new Response(); });
  for (const length of [String(REQUEST_LIMIT + 1), '999999999999999999999999', '-1', 'NaN', '2, 2', '01'])
    assert.equal((await gateway.fetch(upload('x', { 'content-length': length }))).status, 413);
  for (const headers of [{ 'content-encoding': 'gzip' }, { 'content-encoding': 'identity' },
    { 'content-type': 'text/plain' }, { 'content-type': 'application/x-git-upload-pack-request; charset=utf-8' }])
    assert.equal((await gateway.fetch(upload('x', headers))).status, 415);
  assert.equal(calls, 0);
});

test('chunked and dishonestly short declared bodies stop at 1 MiB and cancel the source', async () => {
  let calls = 0;
  const gateway = adapter(async () => { calls++; return new Response(); });
  for (const headers of [{}, { 'content-length': '1' }]) {
    let cancelled = 0;
    const body = stream([new Uint8Array(REQUEST_LIMIT), new Uint8Array(1), new Uint8Array(1)], () => { cancelled++; });
    assert.equal((await gateway.fetch(upload(body, headers))).status, 413);
    assert.equal(cancelled, 1);
  }
  assert.equal(calls, 0);
});

test('every 3xx response is rejected, cancelled and never followed', async () => {
  for (const status of [300, 301, 302, 303, 304, 305, 307, 308, 399]) {
    let calls = 0, cancelled = 0;
    const gateway = adapter(async request => {
      calls++; assert.equal(request.redirect, 'manual');
      return new Response(status === 304 ? null : stream([new Uint8Array(1)], () => { cancelled++; }),
        { status, headers: { location: 'https://attacker.invalid', 'set-cookie': 'private' } });
    });
    const response = await gateway.fetch(new Request(DISCOVERY));
    assert.equal(response.status, 502); assert.equal(await response.text(), '');
    assert.deepEqual([...response.headers], [['cache-control', 'no-store']]);
    assert.equal(calls, 1); assert.equal(cancelled, status === 304 ? 0 : 1);
  }
});

test('malformed and oversized response content lengths reject and cancel before streaming', async () => {
  for (const length of [String(RESPONSE_LIMIT + 1), '-1', 'NaN', '2, 2', '01', '99999999999999999999999']) {
    let cancelled = 0;
    const gateway = adapter(async () => new Response(stream([new Uint8Array(1)], () => { cancelled++; }),
      { headers: { 'content-length': length } }));
    assert.equal((await gateway.fetch(new Request(DISCOVERY))).status, 502);
    assert.equal(cancelled, 1);
  }
});

test('exactly 96 MiB streams; excess chunk fails without being forwarded and cancels upstream', async () => {
  // Reuse a public zero-filled block: exercise the full production limit without
  // allocating a 96 MiB test body or buffering the returned response.
  const block = new Uint8Array(1024 * 1024);
  for (const extra of [false, true]) {
    let cancelled = 0, chunksRead = 0;
    const chunks = Array.from({ length: 96 }, () => block);
    if (extra) chunks.push(new Uint8Array(1), new Uint8Array(1));
    const gateway = adapter(async () => new Response(stream(chunks, () => { cancelled++; }),
      { headers: { 'content-length': '1' } }));
    const response = await gateway.fetch(new Request(DISCOVERY));
    const reader = response.body.getReader();
    let total = 0;
    const consume = async () => {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        chunksRead++; total += value.byteLength;
      }
    };
    if (extra) await assert.rejects(consume(), /Gateway response unavailable/);
    else await consume();
    assert.equal(total, RESPONSE_LIMIT); assert.equal(chunksRead, 96);
    assert.equal(cancelled, extra ? 1 : 0);
  }
});

test('consumer cancellation cancels upstream and aborts the network request', async () => {
  let cancelled = 0, signal;
  const gateway = adapter(async request => {
    signal = request.signal;
    return new Response(stream([new Uint8Array(1), new Uint8Array(1)], () => { cancelled++; }));
  });
  const response = await gateway.fetch(new Request(DISCOVERY));
  await response.body.cancel();
  assert.equal(cancelled, 1); assert.equal(signal.aborted, true);
});

test('network errors and error responses fail closed without leaking details', async () => {
  for (const fetchImpl of [async () => { throw new Error(PUBLIC_TEST_VECTOR_SERVICE); }, async () => Response.error()]) {
    const response = await adapter(fetchImpl).fetch(new Request(DISCOVERY));
    assert.equal(response.status, 502); assert.equal(await response.text(), '');
  }
});

test('deadline aborts stalled fetch even if the injected implementation ignores abort', async () => {
  let signal, resolveFetch, cancelled = 0;
  const gateway = adapter(request => {
    signal = request.signal;
    return new Promise(resolve => { resolveFetch = resolve; });
  }, { timeoutMs: 10 });
  assert.equal((await gateway.fetch(new Request(DISCOVERY))).status, 502);
  assert.equal(signal.aborted, true);
  resolveFetch(new Response(stream([new Uint8Array(1)], () => { cancelled++; })));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(cancelled, 1);
});

test('deadline cancels a stalled request body before making any network call', async () => {
  let calls = 0, cancelled = 0;
  const body = new ReadableStream({ pull() { return new Promise(() => {}); }, cancel() { cancelled++; } });
  const gateway = adapter(async () => { calls++; return new Response(); }, { timeoutMs: 10 });
  assert.equal((await gateway.fetch(upload(body))).status, 502);
  assert.equal(cancelled, 1); assert.equal(calls, 0);
});

test('deadline remains active through a stalled response and signals stream failure', async () => {
  let cancelled = 0, signal;
  const gateway = adapter(async request => {
    signal = request.signal;
    return new Response(new ReadableStream({ pull() { return new Promise(() => {}); }, cancel() { cancelled++; } }));
  }, { timeoutMs: 10 });
  const response = await gateway.fetch(new Request(DISCOVERY));
  await assert.rejects(response.text(), /Gateway/);
  assert.equal(cancelled, 1); assert.equal(signal.aborted, true);
});

test('caller abort propagates before fetch and while streaming the response', async () => {
  let calls = 0, cancelled = 0, signal;
  const gateway = adapter(async request => {
    calls++; signal = request.signal;
    return new Response(stream([new Uint8Array(1)], () => { cancelled++; }));
  });
  const alreadyAborted = new AbortController(); alreadyAborted.abort();
  assert.equal((await gateway.fetch(new Request(DISCOVERY, { signal: alreadyAborted.signal }))).status, 502);
  assert.equal(calls, 0);
  const abort = new AbortController();
  const response = await gateway.fetch(new Request(DISCOVERY, { signal: abort.signal }));
  abort.abort();
  await assert.rejects(response.text(), /Gateway/);
  assert.equal(signal.aborted, true); assert.equal(cancelled, 1);
});

test('non-byte body streams fail closed rather than bypassing byte accounting', async () => {
  let calls = 0, cancelled = 0;
  const gateway = adapter(async () => { calls++; return new Response(); });
  const response = await gateway.fetch(upload(stream(['PUBLIC_TEST_VECTOR_INVALID_BYTES'], () => { cancelled++; })));
  assert.equal(response.status, 502); assert.equal(calls, 0); assert.equal(cancelled, 1);
  cancelled = 0;
  const badResponse = await adapter(async () => new Response(stream(['PUBLIC_TEST_VECTOR_INVALID_BYTES'],
    () => { cancelled++; }))).fetch(new Request(DISCOVERY));
  await assert.rejects(badResponse.text(), /Gateway response unavailable/);
  assert.equal(cancelled, 1);
});

test('already aborted upload cancels its unread input without a network call', async () => {
  let calls = 0, cancelled = 0;
  const abort = new AbortController(); abort.abort();
  const gateway = adapter(async () => { calls++; return new Response(); });
  const response = await gateway.fetch(upload(stream([new Uint8Array(1)], () => { cancelled++; }), {}, { signal: abort.signal }));
  assert.equal(response.status, 502); assert.equal(calls, 0); assert.equal(cancelled, 1);
});

test('no-body responses preserve their status and complete without a leaked deadline', async () => {
  let signal;
  const gateway = adapter(async request => { signal = request.signal; return new Response(null, { status: 204 }); }, { timeoutMs: 10 });
  const response = await gateway.fetch(new Request(DISCOVERY));
  assert.equal(response.status, 204); assert.equal(await response.text(), '');
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(signal.aborted, false);
});

test('deadline cancels an unconsumed response as well as an actively read response', async () => {
  let cancelled = 0;
  const gateway = adapter(async () => new Response(stream([new Uint8Array(1)], () => { cancelled++; })), { timeoutMs: 10 });
  const response = await gateway.fetch(new Request(DISCOVERY));
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(cancelled, 1);
  await assert.rejects(response.text(), /Gateway/);
});
