// SPDX-License-Identifier: Apache-2.0
// Only the trusted configured Weft origin receives the public PoP challenge/exchange.
export function sessionAuthorityBinding(env, fetchImpl = fetch) {
  return { async fetch(input, init) {
    const request = new Request(input, init), path = new URL(request.url).pathname;
    if (request.url !== `https://authority.invalid${path}` || !['/git/auth/challenge', '/git/auth/exchange'].includes(path) ||
        request.method !== 'POST' || request.headers.get('content-type') !== 'application/x-protobuf') throw new Error('Invalid session authority route');
    if (typeof env.HOSTED_NATIVE_CONFIG !== 'string' || env.HOSTED_NATIVE_CONFIG.length > 64 * 1024) throw new Error('Native authority unconfigured');
    const origin = new URL(JSON.parse(env.HOSTED_NATIVE_CONFIG).authority_origin);
    if (origin.protocol !== 'https:' || origin.username || origin.password || origin.pathname !== '/' || origin.search || origin.hash || origin.port)
      throw new Error('Exact Weft HTTPS origin required');
    return fetchImpl(new Request(origin.origin + path, { method: 'POST', headers: { 'content-type': 'application/x-protobuf' },
      body: request.body, duplex: 'half', redirect: 'manual', signal: request.signal }));
  } };
}
