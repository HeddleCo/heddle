// SPDX-License-Identifier: Apache-2.0
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readNativeBundle } from './native-source.mjs';
const bytes = new TextEncoder().encode('synthetic native fixture');
const descriptor = { key: 'native/synthetic.tar', sha256: createHash('sha256').update(bytes).digest('hex') };
test('documented R2 get response contract; pinned key and bytes', async () => {
  const bucket = { async get(key) { assert.equal(key, descriptor.key); return { size: bytes.length, async arrayBuffer() { return bytes.buffer; } }; } };
  assert.deepEqual(new Uint8Array(await readNativeBundle(bucket, descriptor)), bytes);
});
test('R2 missing, oversized, corrupt and inconsistent responses refused', async () => {
  for (const object of [null, { size: 9 * 1024 * 1024 },
      { size: 1, async arrayBuffer() { return bytes.buffer; } },
      { size: 1, async arrayBuffer() { return new Uint8Array([0]).buffer; } }]) {
    await assert.rejects(readNativeBundle({ async get() { return object; } }, descriptor));
  }
});

import { nativeBundleService } from './native-source.mjs';
test('internal native service authorizes source before R2 read', async () => {
  let reads = 0;
  const bucket = { async get() { reads++; return { size: bytes.length, async arrayBuffer() { return bytes.buffer; } }; } };
  const service = nativeBundleService(bucket, { 'native-demo': descriptor }, async (request, source) =>
    source === 'native-demo' && request.headers.get('authorization') === 'Bearer PUBLIC_TEST_VECTOR');
  const url = 'https://internal.invalid/native/native-demo';
  assert.equal((await service.fetch(new Request(url))).status, 403);
  assert.equal(reads, 0);
  const response = await service.fetch(new Request(url, { headers: { authorization: 'Bearer PUBLIC_TEST_VECTOR' } }));
  assert.equal(response.status, 200);
  assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.deepEqual(new Uint8Array(await response.arrayBuffer()), bytes);
  assert.equal(reads, 1);
  assert.throws(() => nativeBundleService(bucket, {}, null));
});

test('oversized R2 metadata cancels its unread stream before buffering', async () => {
  let cancelled = false, buffered = false;
  const object = { size: 8 * 1024 * 1024 + 1,
    body: new ReadableStream({ cancel() { cancelled = true; } }),
    async arrayBuffer() { buffered = true; return bytes.buffer; } };
  await assert.rejects(readNativeBundle({ get: async () => object }, descriptor));
  assert.equal(cancelled, true); assert.equal(buffered, false);
});

test('R2 adapter rejects coerced digests and non-ArrayBuffer response shapes', async () => {
  let read = false;
  await assert.rejects(readNativeBundle({ get: async () => { read = true; } },
    { ...descriptor, sha256: { toString: () => descriptor.sha256 } }));
  assert.equal(read, false);
  await assert.rejects(readNativeBundle({ get: async () => ({ size: bytes.length, arrayBuffer: async () => bytes }) }, descriptor));
});

test('internal native service failures are uncacheable and unknown routes avoid authority/storage', async () => {
  let authenticated = false, read = false;
  const service = nativeBundleService({ get: async () => { read = true; return null; } },
    { 'native-demo': descriptor }, async () => { authenticated = true; return false; });
  for (const suffix of ['/native/native-demo#fragment', '/native/native-demo?key=other', '/native/../private']) {
    const response = await service.fetch(new Request('https://internal.invalid' + suffix));
    assert.equal(response.status, 404); assert.equal(response.headers.get('cache-control'), 'no-store');
  }
  assert.equal(authenticated, false); assert.equal(read, false);
  const denied = await service.fetch(new Request('https://internal.invalid/native/native-demo'));
  assert.equal(denied.status, 403); assert.equal(denied.headers.get('cache-control'), 'no-store');
  assert.equal(read, false);
});
