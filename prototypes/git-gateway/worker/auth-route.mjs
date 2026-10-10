// SPDX-License-Identifier: Apache-2.0
import { abortable } from './abortable.mjs';
const MAX_AUTH_REQUEST = 64 * 1024;
const MAX_AUTH_RESPONSE = 16 * 1024;
const failure = status => new Response(null, { status, headers: { 'cache-control': 'no-store' } });
const cancel = body => { if (body) void body.cancel().catch(() => {}); };
async function bounded(body, maximum, signal) {
  if (!body) return new Uint8Array();
  const reader = body.getReader(); let size = 0; const chunks = [];
  try {
    while (true) {
      const { done, value } = await abortable(reader.read(), signal); if (done) break;
      if (!(value instanceof Uint8Array) || (size += value.length) > maximum) throw new Error('Authentication message limit');
      chunks.push(value);
    }
  } catch (error) { cancel(reader); throw error; }
  finally { reader.releaseLock(); }
  const result = new Uint8Array(size); let at = 0;
  for (const chunk of chunks) { result.set(chunk, at); at += chunk.length; }
  return result;
}
export async function authenticationRoute(request, authority, timeoutMs) {
  const url = new URL(request.url);
  const reject = status => { cancel(request.body); return failure(status); };
  if (!/^\/git\/auth\/(challenge|exchange)$/.test(url.pathname) || url.search || request.url.includes('#') ||
      request.url.endsWith('?')) return reject(404);
  if (request.method !== 'POST') return reject(405);
  if (request.headers.get('content-type') !== 'application/x-protobuf' || request.headers.has('content-encoding')) return reject(415);
  const length = request.headers.get('content-length');
  if (length !== null && (!/^(0|[1-9][0-9]*)$/.test(length) || Number(length) > MAX_AUTH_REQUEST)) return reject(413);
  if (!authority?.fetch) return reject(503);
  const deadline = new AbortController(), signal = AbortSignal.any([request.signal, deadline.signal]);
  const timer = setTimeout(() => deadline.abort(new Error('Authentication deadline')), timeoutMs);
  try {
    const bytes = await bounded(request.body, MAX_AUTH_REQUEST, signal);
    const response = await abortable(authority.fetch('https://authority.invalid' + url.pathname, {
      method: 'POST', body: bytes, headers: { 'content-type': 'application/x-protobuf' }, redirect: 'manual', signal,
    }), signal, response => cancel(response.body));
    if (response.redirected || response.status !== 200 ||
        response.headers.get('content-type') !== 'application/x-protobuf' || response.headers.has('content-encoding')) {
      cancel(response.body); return failure([400, 401, 403, 429].includes(response.status) ? response.status : 503);
    }
    const result = await bounded(response.body, MAX_AUTH_RESPONSE, signal);
    return new Response(result, { headers: { 'content-type': 'application/x-protobuf', 'cache-control': 'no-store' } });
  } catch { return reject(503); }
  finally { clearTimeout(timer); }
}
