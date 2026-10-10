// SPDX-License-Identifier: Apache-2.0
// Internal preparation boundary only: not exposed by the public Worker router.
const MAX = 8 * 1024 * 1024;
export async function readNativeBundle(bucket, descriptor) {
  if (!descriptor || typeof descriptor.key !== 'string' || !descriptor.key ||
      typeof descriptor.sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(descriptor.sha256)) throw new Error('invalid native descriptor');
  const object = await bucket.get(descriptor.key);
  if (!object || !Number.isSafeInteger(object.size) || object.size < 0 || object.size > MAX) {
    if (object?.body) void object.body.cancel().catch(() => {});
    throw new Error('native bundle unavailable');
  }
  const bytes = await object.arrayBuffer();
  if (!(bytes instanceof ArrayBuffer) || bytes.byteLength !== object.size || bytes.byteLength > MAX)
    throw new Error('native bundle limit');
  const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)),
    byte => byte.toString(16).padStart(2, '0')).join('');
  if (digest !== descriptor.sha256) throw new Error('native bundle digest mismatch');
  return bytes;
}

// Internal service handler; deliberately not installed on the public front door.
// authenticate(request, source) must validate a pre-provisioned service identity
// and source scope. It is mandatory and runs before any R2 access.
export function nativeBundleService(bucket, descriptors, authenticate) {
  if (typeof authenticate !== 'function') throw new Error('service authority required');
  const failure = status => new Response(null, { status, headers: { 'cache-control': 'no-store' } });
  return {
    async fetch(request) {
      const url = new URL(request.url);
      const match = /^\/native\/([a-z][a-z0-9-]{0,63})$/.exec(url.pathname);
      if (!match || url.search || url.hash || request.method !== 'GET') return failure(404);
      const source = match[1];
      try {
        if (await authenticate(request, source) !== true) return failure(403);
        if (!Object.hasOwn(descriptors, source)) return failure(404);
        const bytes = await readNativeBundle(bucket, descriptors[source]);
        return new Response(bytes, { headers: { 'content-type': 'application/octet-stream',
          'content-length': String(bytes.byteLength), 'cache-control': 'no-store' } });
      } catch {
        return failure(503);
      }
    },
  };
}
