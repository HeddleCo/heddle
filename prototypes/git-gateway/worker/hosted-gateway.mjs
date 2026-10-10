// SPDX-License-Identifier: Apache-2.0
// Hosted Git request composition. The Native binding is the Rust source preparation/projection
// host, not git-receive-pack. Only the genuine receiver+durable catalog path produces a Git ACK.
import { gitRoute } from './git-route.mjs';
import { abortable } from './abortable.mjs';
const encoder = new TextEncoder();
const failure = status => new Response(null, { status, headers: { 'cache-control': 'no-store' } });
const cancel = body => { if (body) void body.cancel().catch(() => {}); };
function packets(lines, advertisement = false) {
  let body = advertisement ? '001f# service=git-receive-pack\n0000' : '';
  for (const line of lines) body += (encoder.encode(line).length + 4).toString(16).padStart(4, '0') + line;
  return body + '0000';
}
function response(body, kind) {
  return new Response(body, { headers: { 'content-type': `application/x-git-receive-pack-${kind}`, 'cache-control': 'no-store' } });
}
async function boundedRequest(request, limit) {
  const length = request.headers.get('content-length');
  if (length !== null && (!/^(0|[1-9][0-9]*)$/.test(length) || Number(length) > limit)) throw new Error('Receive size');
  const chunks = []; let total = 0;
  if (request.body) {
    const reader = request.body.getReader();
    try {
      while (true) { request.signal.throwIfAborted(); const { done, value } = await abortable(reader.read(), request.signal); if (done) break;
        if (!(value instanceof Uint8Array) || (total += value.length) > limit) throw new Error('Receive size'); chunks.push(value); }
    } catch (error) { cancel(reader); throw error; } finally { reader.releaseLock(); }
  }
  const bytes = new Uint8Array(total); let at = 0;
  for (const chunk of chunks) { bytes.set(chunk, at); at += chunk.length; }
  return bytes;
}
export function hostedGateway({ coordinator, staging, native, authorizeRequest }) {
  if (!coordinator?.publish || !coordinator?.current || !staging?.stage || !native?.prepare || !native?.project ||
      typeof authorizeRequest !== 'function') throw new Error('Complete hosted Git integrations required');
  return Object.freeze({ async fetch(request) {
    const route = gitRoute(request);
    if (route.error || route.kind !== 'repositories') { cancel(request.body); return failure(route.error || 404); }
    const credential = request.headers.get('authorization') || '';
    if (!credential) { cancel(request.body); return new Response(null, { status: 401, headers: { 'cache-control': 'no-store', 'www-authenticate': 'Basic realm="Heddle Git"' } }); }
    try {
      // Exact configured repository->canonical tenant/spool/thread scope is resolved by the
      // real authority before looking at pending state, native storage or a projection cache.
      if (await authorizeRequest(route.target, route.write ? 'write' : 'read', credential) !== true) {
        cancel(request.body); return failure(403);
      }
      if (route.endpoint === 'native-reconcile') {
        const bytes = await boundedRequest(request, route.limit);
        const observed = JSON.parse(new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes));
        const outcome = await coordinator.reconcile(route.target, observed, credential);
        return Response.json({ kind: outcome.kind, acceptance: outcome.acceptance, operation: outcome.operation,
          catalog: outcome.inspected.catalog_pin, native_state: outcome.inspected.current_native,
          next: `/repositories/${route.target}.git/native-refresh` }, { headers: { 'cache-control': 'no-store' } });
      }
      if (route.endpoint === 'native-refresh') {
        const bytes = await boundedRequest(request, route.limit);
        const input = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
        if (!input || Object.keys(input).sort().join(',') !== 'expected_catalog,expected_native' ||
            !/^[0-9a-f]{40}$/.test(input.expected_catalog) || !/^hs-[0-9a-z]{52}$/.test(input.expected_native)) throw new Error('Explicit native refresh fence required');
        const prior = await coordinator.refreshBase(route.target);
        if (prior.pin !== input.expected_catalog || typeof native.refreshPlan !== 'function') throw new Error('Native refresh catalog changed');
        let prepared = await native.refreshPlan(route.target, prior, input.expected_native, credential);
        if (prepared?.intent?.schema !== 3 || prepared.intent.scope.repository !== route.target ||
            prepared.intent.native_state !== input.expected_native || prepared.intent.expected_catalog !== prior.pin) throw new Error('Native refresh plan differs');
        await staging.stage(prepared.intent, prepared.proof, prepared.artifacts, credential);
        const intent = prepared.intent; prepared = null;
        const refreshed = await coordinator.refresh(intent, credential);
        return Response.json({ kind: 'native-refresh', catalog: refreshed.pin, native_state: refreshed.intent.native_state,
          git_commit: refreshed.intent.new_git }, { headers: { 'cache-control': 'no-store' } });
      }
      let published = await coordinator.current(route.target, credential, { receiveDiscovery: route.write, allowAbsent: true });
      if (!published) {
        // Persistent initialization uses the enrolled publisher and requires an exact write
        // session. A reader cannot trigger source staging or initial catalog publication.
        if (!route.write) { cancel(request.body); return failure(403); }
        if (typeof native.bootstrapPlan !== 'function' || typeof coordinator.bootstrap !== 'function') throw new Error('Native bootstrap unavailable');
        let prepared = await native.bootstrapPlan(route.target, credential);
        if (prepared?.intent?.schema !== 2 || prepared.intent.scope.repository !== route.target) throw new Error('Bootstrap repository differs');
        await staging.stage(prepared.intent, prepared.proof, prepared.artifacts, credential);
        const intent = prepared.intent; prepared = null;
        published = await coordinator.bootstrap(intent, credential);
      }
      if (route.write && request.method === 'GET') {
        return response(packets([`${published.intent.new_git} refs/heads/${published.intent.scope.thread}\0report-status ofs-delta object-format=sha1\n`], true), 'advertisement');
      }
      if (!route.write) {
        // Rust checks exact native packs/proofs, reconstructs a fresh bounded Git projection,
        // and rechecks current full-history authority before emitting any output frame byte.
        // Check our catalog/R2 view first: issuing another native RPC while Rust is streaming
        // an unread Git body can deadlock its deliberately single-request listener.
        await coordinator.verifyPublished(published, credential, 'read');
        const output = await native.project(request, published, credential);
        if (!(output instanceof Response) || output.redirected || output.status !== 200) { cancel(output?.body); return failure(503); }
        const expectedType = request.method === 'GET' ? 'application/x-git-upload-pack-advertisement' : 'application/x-git-upload-pack-result';
        if (output.headers.get('content-type') !== expectedType) { cancel(output.body); return failure(503); }
        return new Response(output.body, { headers: { 'content-type': expectedType, 'cache-control': 'no-store' } });
      }
      let bytes = await boundedRequest(request, route.limit);
      request.signal.throwIfAborted();
      // Rust parses one fast-forward update, quarantines/unpacks under CPU/memory limits,
      // verifies exact imported Git objects, and signs native originals with its explicit
      // enrolled publisher. The returned proof is token-free and checked by staging.
      let prepared = await native.prepare(bytes, published, credential);
      if (prepared?.intent?.scope?.repository !== route.target) throw new Error('Prepared repository differs');
      await staging.stage(prepared.intent, prepared.proof, prepared.artifacts, credential);
      const intent = prepared.intent; prepared = null; bytes = null;
      const accepted = await coordinator.publish(intent, credential);
      // current authority and exact receiver/native generation are checked by publish, including
      // after its final durable transaction. No catch path reports success or rolls native back.
      request.signal.throwIfAborted();
      return response(packets(['unpack ok\n', `ok refs/heads/${accepted.intent.scope.thread}\n`]), 'result');
    } catch { cancel(request.body?.locked ? null : request.body); return new Response(null, { status: 503, headers: { 'cache-control': 'no-store', 'x-heddle-recovery': `Retry receive discovery for transient publication failure. After a competing native writer, an operator must use /repositories/${route.target}.git/native-reconcile with exact journal, native and catalog fences, then native-refresh. With no pending write, use native-refresh.` } }); }
  } });
}
