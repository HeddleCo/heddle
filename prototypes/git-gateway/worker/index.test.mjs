// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { validateManifest } from './catalog.mjs';
const contract = JSON.parse(readFileSync(new URL('../catalog-fixtures.json', import.meta.url)));
import worker, { ArtifactsCatalog, createFrontDoor } from './index.mjs';
import { gatewayTransport } from './gateway-transport.mjs';
const pin = 'a'.repeat(40);
const path = `/views/${pin}.git/info/refs?service=git-upload-pack`;
function fixtures() {
  const calls = []; let disposed = 0;
  const env = {
    CATALOG_REPO: 'catalog', PUBLISHED_CATALOG_PINS: JSON.stringify([pin]),
    ARTIFACTS: { async get(name) {
      calls.push(['get', name]);
      return { async readFile(args) { calls.push(['readFile', args]); return new Blob([contract.valid]); },
        [Symbol.dispose]() { disposed++; } };
    } },
    AUTH: { async fetch(url, init) { calls.push(['authorize', JSON.parse(init.body)]); return new Response(null, { status: 200 }); } },
    GATEWAY: { async fetch(request) { calls.push(['gateway', request]); return new Response('git bytes', { headers: { 'content-type': 'application/x-git-upload-pack-advertisement', 'set-cookie': 'should-not-leak' } }); } },
  };
  return { env, calls, disposed: () => disposed };
}
test('official binding read surface resolves exact commit and disposes capability', async () => {
  const f = fixtures(); await new ArtifactsCatalog(f.env.ARTIFACTS, 'catalog', [pin]).resolve(pin);
  assert.deepEqual(f.calls, [['get', 'catalog'], ['readFile', { ref: pin, path: 'manifest.json' }]]);
  assert.equal(f.disposed(), 1);
});
test('front door authorizes pinned catalog before read; strips untrusted identity headers', async () => {
  const f = fixtures(); const response = await worker.fetch(new Request('https://demo.invalid' + path,
    { headers: { authorization: 'test-fixture-only', 'x-demo-reader': 'forged', 'git-protocol': 'version=2' } }), f.env);
  assert.equal(response.status, 200); assert.equal(await response.text(), 'git bytes');
  assert.deepEqual(f.calls[0], ['authorize', { catalog: 'catalog', pin }]);
  const forwarded = f.calls.at(-1)[1];
  assert.equal(forwarded.headers.get('x-demo-reader'), null);
  assert.equal(forwarded.headers.get('authorization'), 'test-fixture-only');
  assert.equal(response.headers.get('cache-control'), 'no-store'); assert.equal(response.headers.get('set-cookie'), null);
});
test('denial avoids catalog and native source calls', async () => {
  const f = fixtures(); f.env.AUTH.fetch = async () => new Response(null, { status: 403 });
  assert.equal((await worker.fetch(new Request('https://demo.invalid' + path), f.env)).status, 403);
  assert.equal(f.calls.length, 0);
});
test('front door preserves caller cancellation for the gateway transport', async () => {
  const f = fixtures(); const controller = new AbortController();
  f.env.GATEWAY.fetch = async request => {
    assert.equal(request.signal.aborted, false);
    controller.abort();
    assert.equal(request.signal.aborted, true);
    return new Response(null, { status: 503 });
  };
  const response = await worker.fetch(new Request('https://demo.invalid' + path, { signal: controller.signal }), f.env);
  assert.equal(response.status, 503);
});
test('receive-pack and arbitrary routes never reach bindings', async () => {
  const f = fixtures();
  for (const suffix of ['/git-receive-pack', '/info/refs?service=git-receive-pack', '/objects/aa']) {
    const response = await worker.fetch(new Request(`https://demo.invalid/views/${pin}.git${suffix}`), f.env);
    assert.ok([404, 405].includes(response.status));
  }
  assert.equal(f.calls.length, 0);
});
test('missing integration and oversized actual body fail closed', async () => {
  assert.equal((await worker.fetch(new Request('https://demo.invalid' + path), {})).status, 503);
  const f = fixtures();
  const request = new Request(`https://demo.invalid/views/${pin}.git/git-upload-pack`, {
    method: 'POST', body: new Uint8Array(1048577), headers: { 'content-type': 'application/x-git-upload-pack-request' },
  });
  assert.equal((await worker.fetch(request, f.env)).status, 413);
  assert.equal(f.calls.some(c => c[0] === 'gateway'), false);
});
test('oversized catalog blob disposed and refused', async () => {
  const f = fixtures(); let disposed = false;
  f.env.ARTIFACTS.get = async () => ({ readFile: async () => new Blob(['x'.repeat(16385)]), [Symbol.dispose]() { disposed = true; } });
  await assert.rejects(new ArtifactsCatalog(f.env.ARTIFACTS, 'catalog', [pin]).resolve(pin)); assert.equal(disposed, true);
});

test('shared canonical contract rejects malformed, duplicate, ambiguous and unsafe manifests', () => {
  assert.equal(validateManifest(contract.valid).repository, 'synthetic-demo');
  for (const invalid of contract.invalid) assert.throws(() => validateManifest(invalid));
});
test('nested Thread names are logical names, never traversal or source URLs', () => {
  const original = JSON.parse(contract.valid);
  const encode = thread => JSON.stringify({ ...original, thread }) + '\n';
  assert.equal(validateManifest(encode('demo/priority-sort')).thread, 'demo/priority-sort');
  for (const name of ['../main', 'demo/../main', 'demo//main', '/main', 'demo%2fmain', 'demo\\main', 'a'.repeat(256)])
    assert.throws(() => validateManifest(encode(name)));
});
test('unpublished or non-commit pins cannot trigger a binding call', async () => {
  const f = fixtures(); const reader = new ArtifactsCatalog(f.env.ARTIFACTS, 'catalog', [pin]);
  for (const bad of ['main', 'HEAD', '../escape', 'c'.repeat(40)]) await assert.rejects(reader.resolve(bad));
  assert.equal(f.calls.length, 0);
});
test('missing file, errors and malformed documented Blob responses fail closed and dispose', async () => {
  for (const value of [null, new Blob(['{}']), new Blob(['x'.repeat(16385)])]) {
    let disposed = false;
    const artifacts = { get: async () => ({ readFile: async () => value, [Symbol.dispose]() { disposed = true; } }) };
    await assert.rejects(new ArtifactsCatalog(artifacts, 'catalog', [pin]).resolve(pin));
    assert.equal(disposed, true);
  }
});
test('configured publication snapshot cannot be broadened through the original array', async () => {
  const f = fixtures(); const pins = [pin]; const catalog = new ArtifactsCatalog(f.env.ARTIFACTS, 'catalog', pins);
  pins.push('c'.repeat(40)); await assert.rejects(catalog.resolve('c'.repeat(40)));
});

test('catalog requires exact canonical UTF-8 bytes, including rejecting a BOM', async () => {
  for (const bytes of [new Blob(['\ufeff', contract.valid]),
    new Blob([new Uint8Array([0xff]), contract.valid])]) {
    const f = fixtures(); let disposed = 0;
    f.env.ARTIFACTS.get = async () => ({ readFile: async () => bytes,
      [Symbol.dispose]() { disposed++; } });
    await assert.rejects(new ArtifactsCatalog(f.env.ARTIFACTS, 'catalog', [pin]).resolve(pin));
    assert.equal(disposed, 1);
  }
});
test('Worker refuses compressed bodies and unpublished views even when AUTH returns success', async () => {
  const f = fixtures();
  const compressed = new Request(`https://demo.invalid/views/${pin}.git/git-upload-pack`, {
    method: 'POST', body: 'x', headers: { 'content-encoding': 'gzip', 'content-type': 'application/x-git-upload-pack-request' },
  });
  assert.equal((await worker.fetch(compressed, f.env)).status, 415);
  f.env.PUBLISHED_CATALOG_PINS = '[]';
  assert.equal((await worker.fetch(new Request('https://demo.invalid' + path), f.env)).status, 503);
  assert.equal(f.calls.some(c => c[0] === 'gateway'), false);
});

const upload = (body, headers = {}, extra = {}) => new Request(`https://demo.invalid/views/${pin}.git/git-upload-pack`, {
  method: 'POST', body, duplex: 'half', headers: { 'content-type': 'application/x-git-upload-pack-request', ...headers }, ...extra,
});

test('all front-door rejections are uncacheable and invalid framing avoids bindings', async () => {
  for (const [request, status] of [
    [new Request('https://demo.invalid/private'), 404],
    [new Request('https://demo.invalid' + path, { method: 'HEAD' }), 405],
    [upload('x', { 'content-encoding': 'gzip' }), 415],
    ...['', '-1', '01', '1, 1', '1048577'].map(length => [upload('x', { 'content-length': length }), 413]),
  ]) {
    const f = fixtures(); const response = await worker.fetch(request, f.env);
    assert.equal(response.status, status);
    assert.equal(response.headers.get('cache-control'), 'no-store');
    assert.deepEqual(f.calls, []);
  }
  for (const field of ['AUTH', 'GATEWAY', 'ARTIFACTS', 'CATALOG_REPO', 'PUBLISHED_CATALOG_PINS']) {
    const f = fixtures(); delete f.env[field];
    const response = await worker.fetch(new Request('https://demo.invalid' + path), f.env);
    assert.equal(response.status, 503); assert.equal(response.headers.get('cache-control'), 'no-store');
    assert.deepEqual(f.calls, []);
  }
});

test('front-door credential hops disable redirects and cancel discarded service responses', async () => {
  for (const status of [200, 302, 403]) {
    const f = fixtures(); let cancelled = false;
    f.env.AUTH.fetch = async (_url, init) => {
      assert.equal(init.redirect, 'manual'); assert.ok(init.signal instanceof AbortSignal);
      return new Response(new ReadableStream({ cancel() { cancelled = true; } }), { status });
    };
    const response = await worker.fetch(new Request('https://demo.invalid' + path), f.env);
    assert.equal(response.status, status === 200 ? 200 : 403);
    assert.equal(response.headers.get('cache-control'), 'no-store'); assert.equal(cancelled, true);
    if (status === 200) assert.equal(f.calls.at(-1)[1].redirect, 'manual');
    else assert.deepEqual(f.calls, []);
  }
  for (const status of [301, 302, 307, 308]) {
    const f = fixtures(); let cancelled = false;
    f.env.GATEWAY.fetch = async () => new Response(new ReadableStream({ cancel() { cancelled = true; } }), {
      status, headers: { location: 'https://untrusted.invalid/' },
    });
    const response = await worker.fetch(new Request('https://demo.invalid' + path), f.env);
    assert.equal(response.status, 503); assert.equal(cancelled, true);
    assert.equal(response.headers.get('location'), null);
  }
});

test('front door bounds byte streams without waiting for cancellation and releases the reader', { timeout: 1000 }, async () => {
  for (const [chunk, status] of [[new Uint8Array(1048577), 413], ['not bytes', 503]]) {
    const f = fixtures(); let cancelled = false;
    const body = new ReadableStream({ start(controller) { controller.enqueue(chunk); },
      cancel() { cancelled = true; return new Promise(() => {}); } });
    const request = upload(body, { 'content-length': '1' });
    const response = await worker.fetch(request, f.env);
    assert.equal(response.status, status); assert.equal(cancelled, true);
    assert.equal(body.locked, false); assert.equal(f.calls.some(c => c[0] === 'gateway'), false);
  }
});

test('front door cancels a stalled upload on caller abort without dispatching partial bytes', { timeout: 1000 }, async () => {
  const f = fixtures(); const abort = new AbortController();
  let reading, cancelled = false;
  const ready = new Promise(resolve => { reading = resolve; });
  const body = new ReadableStream({ pull() { reading(); }, cancel() { cancelled = true; } }, { highWaterMark: 0 });
  const pending = worker.fetch(upload(body, {}, { signal: abort.signal }), f.env);
  await ready; abort.abort();
  const response = await pending;
  assert.equal(response.status, 503); assert.equal(cancelled, true); assert.equal(body.locked, false);
  assert.equal(f.calls.some(c => c[0] === 'gateway'), false);
});

test('front door forwards exactly 1 MiB with allowlisted headers and complete bytes', async () => {
  const f = fixtures(); const bytes = new Uint8Array(1048576).fill(42);
  const response = await worker.fetch(upload(bytes, { 'x-forwarded-for': 'spoof', cookie: 'secret', 'content-length': '1' }), f.env);
  assert.equal(response.status, 200);
  const forwarded = f.calls.at(-1)[1];
  assert.deepEqual(new Uint8Array(await forwarded.arrayBuffer()), bytes);
  for (const name of ['cookie', 'x-forwarded-for', 'content-length']) assert.equal(forwarded.headers.has(name), false);
});

test('catalog read exceptions dispose the capability and do not expose service details', async () => {
  const f = fixtures(); let disposed = 0;
  f.env.ARTIFACTS.get = async () => ({ readFile: async () => { throw new Error('private binding diagnostic'); },
    [Symbol.dispose]() { disposed++; } });
  const response = await worker.fetch(new Request('https://demo.invalid' + path), f.env);
  assert.equal(response.status, 503); assert.equal(await response.text(), 'View unavailable');
  assert.equal(response.headers.get('cache-control'), 'no-store'); assert.equal(disposed, 1);
  assert.equal(f.calls.some(c => c[0] === 'gateway'), false);
});

test('front-door deadline is trusted constructor configuration', () => {
  for (const timeoutMs of [0, -1, 120001, 0.5, NaN, '10']) assert.throws(() => createFrontDoor({ timeoutMs }));
});

const shortFront = () => createFrontDoor({ timeoutMs: 10 });
const transport = fetchImpl => gatewayTransport({ origin: 'https://native.invalid',
  serviceCredential: 'PUBLIC_TEST_VECTOR_' + 's'.repeat(32), timeoutMs: 1000, fetchImpl });
const flush = () => new Promise(resolve => setTimeout(resolve, 0));

test('composed front-door deadline bounds stalled authenticated ingress before transport starts', { timeout: 1000 }, async () => {
  const f = fixtures(); let cancelled = false, fetched = false;
  const body = new ReadableStream({ cancel() { cancelled = true; return new Promise(() => {}); } });
  f.env.GATEWAY = transport(async () => { fetched = true; return new Response('unexpected'); });
  const response = await shortFront().fetch(upload(body), f.env);
  assert.equal(response.status, 503); assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.equal(cancelled, true); assert.equal(body.locked, false); assert.equal(fetched, false);
  assert.equal(f.disposed(), 1);
});

test('deadline stops stalled AUTH and cancels its late response without catalog access', { timeout: 1000 }, async () => {
  const f = fixtures(); let complete, signal, cancelled = false, inputCancelled = false;
  f.env.AUTH.fetch = (_url, init) => {
    signal = init.signal; return new Promise(resolve => { complete = resolve; });
  };
  const body = new ReadableStream({ cancel() { inputCancelled = true; return new Promise(() => {}); } });
  const response = await shortFront().fetch(upload(body), f.env);
  assert.equal(response.status, 503); assert.equal(signal.aborted, true); assert.equal(inputCancelled, true);
  complete(new Response(new ReadableStream({ cancel() { cancelled = true; } })));
  await flush();
  assert.equal(cancelled, true); assert.deepEqual(f.calls, []);
});

test('deadline disposes a late Artifacts capability without reading it', { timeout: 1000 }, async () => {
  const f = fixtures(); let complete, disposed = 0, read = false;
  f.env.ARTIFACTS.get = () => new Promise(resolve => { complete = resolve; });
  const response = await shortFront().fetch(new Request('https://demo.invalid' + path), f.env);
  assert.equal(response.status, 503); assert.equal(disposed, 0);
  complete({ readFile() { read = true; }, [Symbol.dispose]() { disposed++; } });
  await flush();
  assert.equal(disposed, 1); assert.equal(read, false);
  assert.equal(f.calls.some(c => c[0] === 'gateway'), false);
});

test('deadline releases acquired catalog capability on stalled file and Blob reads', { timeout: 1000 }, async () => {
  for (const stage of ['file', 'bytes']) {
    const f = fixtures(); let disposed = 0;
    class StalledBlob extends Blob { arrayBuffer() { return new Promise(() => {}); } }
    f.env.ARTIFACTS.get = async () => ({
      readFile: () => stage === 'file' ? new Promise(() => {}) : Promise.resolve(new StalledBlob([contract.valid])),
      [Symbol.dispose]() { disposed++; },
    });
    const response = await shortFront().fetch(new Request('https://demo.invalid' + path), f.env);
    assert.equal(response.status, 503); assert.equal(disposed, 1);
    assert.equal(f.calls.some(c => c[0] === 'gateway'), false);
  }
});

test('front-door deadline aborts composed transport while waiting for upstream headers', { timeout: 1000 }, async () => {
  const f = fixtures(); let signal, complete, cancelled = false;
  f.env.GATEWAY = transport(request => {
    signal = request.signal; return new Promise(resolve => { complete = resolve; });
  });
  const response = await shortFront().fetch(new Request('https://demo.invalid' + path), f.env);
  assert.equal(response.status, 503); assert.equal(signal.aborted, true);
  complete(new Response(new ReadableStream({ cancel() { cancelled = true; } })));
  await flush(); assert.equal(cancelled, true);
});

test('completed ingress clears its timer while transport retains streaming and caller-abort ownership', { timeout: 1000 }, async () => {
  const f = fixtures(); let signal;
  f.env.GATEWAY = transport(request => {
    signal = request.signal;
    return new Response(new ReadableStream({ async start(controller) {
      await new Promise(resolve => setTimeout(resolve, 30));
      controller.enqueue(new TextEncoder().encode('complete')); controller.close();
    } }));
  });
  const response = await shortFront().fetch(new Request('https://demo.invalid' + path), f.env);
  assert.equal(response.status, 200); assert.equal(await response.text(), 'complete');
  assert.equal(signal.aborted, false);

  const abort = new AbortController(); let cancelled = false;
  f.env.GATEWAY = transport(() => new Response(new ReadableStream({ cancel() { cancelled = true; } })));
  const streaming = await shortFront().fetch(new Request('https://demo.invalid' + path, { signal: abort.signal }), f.env);
  abort.abort();
  await assert.rejects(streaming.text()); assert.equal(cancelled, true);
});
