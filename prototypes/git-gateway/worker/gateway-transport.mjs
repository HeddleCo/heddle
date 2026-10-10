// SPDX-License-Identifier: Apache-2.0
// Code-only composition seam: env.GATEWAY = gatewayTransport({...}).
// The HTTPS origin and existing service credential come from trusted deployment
// configuration, never a client request. This does not configure TLS or mint access.
import { gitRoute } from './git-route.mjs';
const RESPONSE_LIMIT = 96 * 1024 * 1024;
const FORWARD_HEADERS = ['authorization', 'content-type', 'git-protocol'];

function failure(status) {
  return new Response(null, { status, headers: { 'cache-control': 'no-store' } });
}
function cancel(reader, reason) {
  if (reader) void reader.cancel(reason).catch(() => {});
}
function oversized(headers, limit) {
  const length = headers.get('content-length');
  return length !== null && (!/^(0|[1-9][0-9]*)$/.test(length) || Number(length) > limit);
}
function withAbort(promise, signal) {
  return new Promise((resolve, reject) => {
    const aborted = () => reject(signal.reason);
    signal.addEventListener('abort', aborted, { once: true });
    Promise.resolve(promise).then(resolve, reject).finally(() => signal.removeEventListener('abort', aborted));
    if (signal.aborted) aborted();
  });
}

export function gatewayTransport({ origin, serviceCredential, fetchImpl = globalThis.fetch, timeoutMs = 30_000 } = {}) {
  // Require a bare HTTPS origin, optionally with one trailing slash. Check the raw
  // spelling too: URL normalization must not hide credentials, paths or delimiters.
  if (typeof origin !== 'string' || !/^https:\/\/[^\s/?#@\\]+\/?$/.test(origin))
    throw new Error('Gateway HTTPS origin required');
  const configured = new URL(origin);
  if (configured.protocol !== 'https:' || !configured.hostname || configured.username || configured.password ||
      configured.pathname !== '/' || configured.search || configured.hash)
    throw new Error('Gateway HTTPS origin required');
  if (typeof serviceCredential !== 'string' || !/^[A-Za-z0-9_-]{32,256}$/.test(serviceCredential))
    throw new Error('Existing gateway service credential required');
  if (typeof fetchImpl !== 'function' || !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 120_000)
    throw new Error('Invalid gateway transport configuration');
  const targetOrigin = configured.origin;

  return Object.freeze({
    async fetch(request) {
      if (!(request instanceof Request)) return failure(400);
      // Only the placeholder URL emitted by index.mjs is accepted. The caller
      // cannot select another host, path, pin spelling, query or fragment.
      const route = gitRoute(request, 'https://gateway.invalid');
      if (route.error) { cancel(request.body); return failure(route.error); }
      const { path, limit: requestLimit } = route;
      if (oversized(request.headers, requestLimit)) { cancel(request.body); return failure(413); }

      const abort = new AbortController();
      let inputReader, outputReader, responseController;
      let timer;
      const finish = () => {
        clearTimeout(timer);
        request.signal.removeEventListener('abort', aborted);
      };
      const stop = reason => {
        if (!abort.signal.aborted) abort.abort(reason);
        cancel(inputReader || (!request.body?.locked && request.body), reason);
        cancel(outputReader, reason);
        responseController?.error(reason);
        finish();
      };
      const aborted = () => stop(new Error('Gateway request aborted'));
      request.signal.addEventListener('abort', aborted, { once: true });
      timer = setTimeout(() => stop(new Error('Gateway transport deadline exceeded')), timeoutMs);
      if (request.signal.aborted) aborted();
      try {
        if (abort.signal.aborted) throw abort.signal.reason;
        const chunks = []; let total = 0;
        if (request.body) {
          inputReader = request.body.getReader();
          while (true) {
            const { done, value } = await withAbort(inputReader.read(), abort.signal);
            if (done) break;
            if (!(value instanceof Uint8Array)) throw new Error('Invalid gateway request bytes');
            total += value.byteLength;
            if (total > requestLimit) {
              stop(new Error('Gateway request body limit'));
              return failure(413);
            }
            chunks.push(value);
          }
          inputReader.releaseLock(); inputReader = undefined;
        }
        const body = new Uint8Array(total); let offset = 0;
        for (const chunk of chunks) { body.set(chunk, offset); offset += chunk.byteLength; }
        const headers = new Headers();
        for (const name of FORWARD_HEADERS) {
          if (request.headers.has(name)) headers.set(name, request.headers.get(name));
        }
        // The loopback native host rejects chunked request framing. Derive the
        // length from bounded bytes; never trust caller framing headers.
        if (request.method === 'POST') headers.set('content-length', String(total));
        headers.set('x-gateway-service-authorization', `Bearer ${serviceCredential}`);
        const outgoing = new Request(targetOrigin + path, {
          method: request.method, headers, body: request.method === 'POST' ? body : undefined,
          redirect: 'manual', credentials: 'omit', cache: 'no-store', referrerPolicy: 'no-referrer', signal: abort.signal,
        });
        const pending = Promise.resolve().then(() => fetchImpl(outgoing));
        // A faulty injected fetch may ignore abort and resolve after the deadline.
        void pending.then(response => { if (abort.signal.aborted) cancel(response.body); }, () => {});
        const upstream = await withAbort(pending, abort.signal);
        outputReader = upstream.body?.getReader();
        if (upstream.redirected || upstream.status < 200 || upstream.status >= 600 ||
            (upstream.status >= 300 && upstream.status < 400) || oversized(upstream.headers, RESPONSE_LIMIT))
          throw new Error('Invalid gateway response');
        const responseHeaders = new Headers({ 'cache-control': 'no-store' });
        if (upstream.headers.has('content-type')) responseHeaders.set('content-type', upstream.headers.get('content-type'));
        if (upstream.status === 503 && upstream.headers.has('x-heddle-recovery')) responseHeaders.set('x-heddle-recovery', upstream.headers.get('x-heddle-recovery'));
        if (!outputReader) {
          finish();
          return new Response(null, { status: upstream.status, headers: responseHeaders });
        }
        let received = 0;
        const stream = new ReadableStream({
          start(controller) { responseController = controller; },
          async pull(controller) {
            try {
              const { done, value } = await withAbort(outputReader.read(), abort.signal);
              if (done) {
                controller.close(); outputReader.releaseLock(); outputReader = undefined;
                responseController = undefined; finish(); return;
              }
              if (!(value instanceof Uint8Array)) throw new Error('Invalid gateway response bytes');
              received += value.byteLength;
              if (received > RESPONSE_LIMIT) throw new Error('Gateway response body limit');
              controller.enqueue(value);
            } catch {
              stop(new Error('Gateway response unavailable'));
            }
          },
          cancel() { responseController = undefined; stop(new Error('Gateway response cancelled')); },
        }, { highWaterMark: 0 });
        return new Response(stream, { status: upstream.status, headers: responseHeaders });
      } catch {
        stop(new Error('Gateway unavailable'));
        return failure(502);
      }
    },
  });
}
