// SPDX-License-Identifier: Apache-2.0
import { ArtifactsCatalog } from './catalog.mjs';
import { abortable } from './abortable.mjs';
export { ArtifactsCatalog } from './catalog.mjs';
import { gitRoute } from './git-route.mjs';
import { authenticationRoute } from './auth-route.mjs';
const failure = (status, body = null) => new Response(body, { status, headers: { 'cache-control': 'no-store' } });
const cancel = body => { if (body) void body.cancel().catch(() => {}); };
// AUTH and GATEWAY are explicit service integration seams, not provided services.
export function createFrontDoor({ timeoutMs = 30_000 } = {}) {
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 120_000)
    throw new Error('Invalid front-door deadline');
  return {
    async fetch(request, env) {
      const url = new URL(request.url);
      if (url.pathname.startsWith('/git/auth/')) return authenticationRoute(request, env.AUTH, timeoutMs);
      const reject = (status, body) => { cancel(request.body); return failure(status, body); };
      const route = gitRoute(request);
      if (route.error) return reject(route.error);
      const { kind, target, write, limit: requestLimit } = route;
      const length = request.headers.get('content-length');
      if (length !== null && (!/^(0|[1-9][0-9]*)$/.test(length) || Number(length) > requestLimit)) return reject(413);
      if (!env.AUTH || !env.GATEWAY || !env.ARTIFACTS || !env.CATALOG_REPO ||
          (kind === 'views' && !env.PUBLISHED_CATALOG_PINS)) return reject(503, 'Integration not configured');
      const deadline = new AbortController();
      const signal = AbortSignal.any([request.signal, deadline.signal]);
      const timer = setTimeout(() => deadline.abort(new Error('Front-door deadline exceeded')), timeoutMs);
      try {
        signal.throwIfAborted();
        // Authenticate before catalog access; authorization service must validate the real
        // client credential and publication membership. No identity comes from a demo header.
        const auth = await abortable(env.AUTH.fetch(`https://authority.invalid/authorize-${kind === 'views' ? 'view' : 'repository'}`, {
          method: 'POST', redirect: 'manual', signal,
          headers: { authorization: request.headers.get('authorization') || '',
            'content-type': 'application/json' },
          body: JSON.stringify(kind === 'views' ? { catalog: env.CATALOG_REPO, pin: target } :
            { catalog: env.CATALOG_REPO, repository: target, operation: write ? 'write' : 'read' }),
        }), signal, response => cancel(response.body));
        cancel(auth.body);
        if (auth.status !== 200 || auth.redirected) return reject(403);
        if (kind === 'views') {
          await new ArtifactsCatalog(env.ARTIFACTS, env.CATALOG_REPO, JSON.parse(env.PUBLISHED_CATALOG_PINS)).resolve(target, signal);
        }
        // Stable routes resolve inside the authenticated native gateway. Receive discovery
        // must repair any durable accepted-but-unpublished journal before advertising a head.
        // A static edge pin snapshot must never prevent that recovery or grant write access.
        // Native gateway independently resolves the same pin, validates current authority,
        // policy epoch and source OID. This metadata is not an authority grant.
        // resolve() enforces the full shared manifest contract and publication set.
        const headers = new Headers();
        for (const name of ['authorization', 'content-type', 'git-protocol']) {
          if (request.headers.has(name)) headers.set(name, request.headers.get(name));
        }
        // Bound even chunked request bodies before forwarding.
        const chunks = []; let total = 0;
        if (request.body) {
          const reader = request.body.getReader();
          const aborted = () => cancel(reader);
          signal.addEventListener('abort', aborted, { once: true });
          try {
            if (signal.aborted) aborted();
            while (true) {
              const { done, value } = await abortable(reader.read(), signal); if (done) break;
              if (!(value instanceof Uint8Array)) throw new Error('Invalid request bytes');
              total += value.byteLength;
              if (total > requestLimit) { cancel(reader); return failure(413); }
              chunks.push(value);
            }
          } catch (error) {
            cancel(reader); throw error;
          } finally {
            signal.removeEventListener('abort', aborted);
            reader.releaseLock();
          }
        }
        signal.throwIfAborted();
        const body = new Uint8Array(total); let offset = 0;
        for (const chunk of chunks) { body.set(chunk, offset); offset += chunk.length; }
        const upstream = await abortable(env.GATEWAY.fetch(new Request('https://gateway.invalid' + url.pathname + url.search,
          { method: request.method, headers, body: request.method === 'POST' ? body : undefined,
            redirect: 'manual', signal })), signal, response => cancel(response.body));
        // Credentials must never follow a service redirect; neither may clients
        // be sent to another source by a redirect response from this fixed hop.
        if (upstream.redirected || (upstream.status >= 300 && upstream.status < 400)) {
          cancel(upstream.body); return failure(503, 'View unavailable');
        }
        const responseHeaders = new Headers({ 'cache-control': 'no-store' });
        if (upstream.headers.has('content-type')) responseHeaders.set('content-type', upstream.headers.get('content-type'));
        if (upstream.status === 503 && upstream.headers.has('x-heddle-recovery')) responseHeaders.set('x-heddle-recovery', upstream.headers.get('x-heddle-recovery'));
        return new Response(upstream.body, { status: upstream.status, headers: responseHeaders });
      } catch {
        return reject(503, 'View unavailable');
      } finally {
        // Ingress/control work is complete. The transport owns its response-stream
        // deadline; the combined signal still propagates a later caller abort.
        clearTimeout(timer);
      }
    }
  };
}
export default createFrontDoor();
