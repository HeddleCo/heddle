// SPDX-License-Identifier: Apache-2.0
// Concrete bounded Worker -> Rust gateway_host bridge. No actor, signer, billing owner, native
// receipt or Git success is accepted from caller headers. The fixed service independently verifies
// the opaque user grant against Weft and the exact source proof against the shared Rust receiver.
import { abortable } from './abortable.mjs';
import { nativeServiceSecret } from './native-service.mjs';
import { FRAME_MIME, MAX_FRAME_REQUEST, MAX_FRAME_RESPONSE, encodeFrame, decodeFrame } from './native-frame.mjs';
import { gitRoute } from './git-route.mjs';
import { PublicationCoordinator, operationId, MAX_NATIVE_BYTES, MAX_PUBLICATION_METADATA } from './publication.mjs';
import { R2NativeStaging } from './native-staging.mjs';
import { hostedGateway } from './hosted-gateway.mjs';
const MAX_PROOF = 17 * 1024 * 1024;
const check = (v, message) => { if (!v) throw new Error(message); };
async function readBounded(response, limit, signal) {
  const declared = response.headers.get('content-length');
  check(declared === null || /^(0|[1-9][0-9]*)$/.test(declared) && Number(declared) <= limit, 'Bridge length limit');
  const reader = response.body?.getReader(); if (!reader) return new Uint8Array();
  let total = 0; const chunks = [];
  try {
    while (true) {
      const { done, value } = await abortable(reader.read(), signal); if (done) break;
      check(value instanceof Uint8Array && (total += value.length) <= limit, 'Bridge body limit'); chunks.push(value);
    }
  } catch (error) { void reader.cancel().catch(() => {}); throw error; }
  finally { reader.releaseLock(); }
  const bytes = new Uint8Array(total); let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; } return bytes;
}
export function nativeHttp({ binding, serviceAuthorization, timeoutMs = 30000 }) {
  nativeServiceSecret(serviceAuthorization);
  check(typeof binding?.fetch === 'function' && typeof serviceAuthorization === 'string' && serviceAuthorization.length > 0 &&
    serviceAuthorization.length <= 4096 && !/[\r\n]/.test(serviceAuthorization) &&
    Number.isSafeInteger(timeoutMs) && timeoutMs >= 1 && timeoutMs <= 120000, 'Native HTTP binding required');
  return async (method, payload, credential = '', parts = []) => {
    check(typeof credential === 'string' && credential.length <= 4096 && !/[\r\n]/.test(credential), 'Bridge authorization limit');
    const frame = encodeFrame({ ...payload, method }, parts, MAX_FRAME_REQUEST);
    const controller = new AbortController(), timeout = setTimeout(() => controller.abort(new Error('Native HTTP timeout')), timeoutMs);
    let response, streaming = false;
    try {
      response = await abortable(binding.fetch(new Request('http://native-container.invalid:8080/native/v1', { method: 'POST', redirect: 'manual',
        headers: { 'content-type': FRAME_MIME, 'content-length': String(frame.length), authorization: credential, 'x-gateway-service-authorization': serviceAuthorization },
        body: frame.body, duplex: 'half', signal: controller.signal })), controller.signal, late => { void late?.body?.cancel().catch(() => {}); });
      check(response instanceof Response && !response.redirected && response.status === 200, 'Native HTTP denied');
      const value = await decodeFrame(response, { maximum: MAX_FRAME_RESPONSE, streamOutput: method === 'project', signal: controller.signal });
      if (value.output) {
        const reader = value.output.getReader(); let finished = false, streamController;
        const finish = () => { if (!finished) { finished = true; clearTimeout(timeout); controller.signal.removeEventListener('abort', abort); } };
        const abort = () => { if (!finished) { finish(); void reader.cancel().catch(() => {}); streamController.error(controller.signal.reason); } };
        value.output = new ReadableStream({ start(output) { streamController = output; controller.signal.addEventListener('abort', abort, { once: true }); if (controller.signal.aborted) abort(); },
          async pull(output) { try { const item = await abortable(reader.read(), controller.signal); if (finished) return;
            if (item.done) { finish(); output.close(); } else output.enqueue(item.value);
          } catch (error) { if (!finished) { finish(); output.error(error); void reader.cancel().catch(() => {}); } } },
          async cancel(reason) { finish(); await reader.cancel(reason).catch(() => {}); },
        }, { highWaterMark: 0 }); streaming = true;
      }
      return value;
    } finally { if (!streaming) { clearTimeout(timeout); if (response?.body && !response.body.locked) void response.body.cancel().catch(() => {}); } }
  };
}
function plan(frame) {
  const value = frame.payload, proof = frame.parts.get(value?.proof_part); let total = 0;
  check(value?.proof_part === 'proof' && proof instanceof Uint8Array && proof.length <= MAX_PROOF, 'Native proof absent');
  check(Array.isArray(value?.artifacts) && value.artifacts.length <= 256, 'Bridge artifact count');
  const artifacts = value.artifacts.map(a => {
    const bytes = frame.parts.get(a.bytes_part); check(bytes instanceof Uint8Array, 'Native artifact absent'); total += bytes.length;
    check(total <= MAX_NATIVE_BYTES && /^[0-9a-f]{64}$/.test(a.sha256), 'Bridge native limit'); return { sha256: a.sha256, bytes };
  });
  return { intent: value.intent, proof, artifacts };
}
const wirePlan = (intent, proof, artifacts) => ({ payload: { intent, proof_part: 'proof',
  artifacts: artifacts.map(a => ({ sha256: a.sha256, bytes_part: `artifact/${a.sha256}` })) },
  parts: [{ name: 'proof', bytes: proof }, ...artifacts.map(a => ({ name: `artifact/${a.sha256}`, bytes: a.bytes }))] });

// Call once per repository Durable Object. All configuration comes from bindings, never the HTTP
// request. bootstrapHeads and catalogNames are exact trusted repository maps; absence denies.
export function composeHostedGateway({ storage, bucket, artifacts, catalogNames, bootstrapHeads, binding,
  serviceAuthorization, timeoutMs }) {
  const rpc = nativeHttp({ binding, serviceAuthorization, timeoutMs }); let staging, coordinator;
  const plain = async (method, payload, credential, parts = []) => (await rpc(method, payload, credential, parts)).payload;
  const sendPlan = (method, intent, proof, sources, credential, extra = {}) => {
    const wire = wirePlan(intent, proof, sources); return plain(method, { ...wire.payload, ...extra }, credential, wire.parts);
  };
  const authorize = async (intent, credential, { mode = 'write', receipt = null } = {}) => {
    const proof = await staging.readProof(intent);
    return plain('authorize', { intent, mode, receipt, ...(proof ? { proof_part: 'proof' } : {}) }, credential, proof ? [{ name: 'proof', bytes: proof }] : []);
  };
  staging = new R2NativeStaging({ bucket,
    submit: (intent, proof, sources, credential) => sendPlan('submit', intent, proof, sources, credential),
    verifyBootstrap: (intent, proof, sources, credential) => sendPlan('bootstrap', intent, proof, sources, credential),
    verifyRefresh: (intent, proof, sources, credential) => sendPlan('refresh', intent, proof, sources, credential),
    inspectReconciliation: (intent, operation, observed, receipt, proof, credential) => plain('reconcile-inspect',
      { intent, operation, receipt, expected_native: observed.expected_native, expected_generation: observed.expected_generation,
        expected_catalog: observed.expected_catalog, ...(observed.outcome ? { outcome: observed.outcome } : {}),
        proof_part: 'proof' }, credential, [{ name: 'proof', bytes: proof }]),
    validatePlan: async (intent, proof, sources, credential, { mode = 'write' } = {}) =>
      (await sendPlan('validate-plan', intent, proof, sources, credential, { mode })).valid === true,
    authorize: async (intent, credential, { mode = 'write' } = {}) => { await coordinator.check(intent, credential, null, mode); return true; },
    resolveIntent: async receipt => {
      const complete = await storage.get(`receipt:${receipt.operation}`);
      if (complete?.operation === receipt.operation) return complete.intent;
      // Durable Object is per-repository; retain no caller-supplied global repository lookup.
      for (const repository of Object.keys(catalogNames || {})) {
        const pending = await storage.get(`pending:${repository}`);
        if (pending?.operation === receipt.operation) return pending.intent;
      }
      throw new Error('Native receipt journal unavailable');
    },
  });
  const catalog = {
    async publish({ repository, expected, operation, manifest }, credential) {
      check(Object.hasOwn(catalogNames || {}, repository), 'Catalog repository unregistered');
      const document = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(manifest));
      check(document.operation === operation && document.intent?.scope?.repository === repository, 'Catalog intent differs');
      const loaded = await staging.load(document.intent, operation, credential);
      const wire = wirePlan(document.intent, loaded.proof, loaded.artifacts);
      const value = await plain('catalog-publish', { repository, expected: expected ?? '0'.repeat(40), operation,
        manifest_part: 'manifest', proof_part: 'proof', artifacts: wire.payload.artifacts }, credential,
        [...wire.parts, { name: 'manifest', bytes: manifest }]);
      check(/^[0-9a-f]{40}$/.test(value.pin), 'Invalid catalog pin'); return value.pin;
    },
    async resolve(repository, pin) {
      check(typeof artifacts?.get === 'function' && Object.hasOwn(catalogNames || {}, repository) && /^[0-9a-f]{40}$/.test(pin), 'Catalog read unavailable');
      const handle = await artifacts.get(catalogNames[repository]);
      try {
        const blob = await handle.readFile({ ref: pin, path: 'manifest.json' });
        check(blob instanceof Blob && blob.size > 0 && blob.size <= MAX_PUBLICATION_METADATA, 'Catalog metadata limit');
        return new Uint8Array(await blob.arrayBuffer());
      } finally { handle?.[Symbol.dispose]?.(); }
    },
  };
  coordinator = new PublicationCoordinator({ storage, native: staging, bucket, catalog, authorize });
  const native = {
    async bootstrapPlan(repository, credential) {
      check(Object.hasOwn(bootstrapHeads || {}, repository), 'Initial native head unconfigured');
      return plan(await rpc('bootstrap-plan', { repository, expected_native: bootstrapHeads[repository] }, credential));
    },
    async refreshPlan(repository, published, expected_native, credential) {
      return plan(await rpc('refresh-plan', { repository, published, expected_native }, credential));
    },
    async prepare(bytes, published, credential) {
      return plan(await rpc('prepare', { repository: published.intent.scope.repository, published, request_part: 'request' }, credential, [{ name: 'request', bytes }]));
    },
    async project(request, published, credential) {
      const route = gitRoute(request); check(!route.error && !route.write, 'Invalid native read route');
      const loaded = await staging.load(published.intent, await operationId(published.intent), credential, { mode: 'read' });
      const body = await readBounded(request, route.limit, request.signal);
      const wire = wirePlan(published.intent, loaded.proof, loaded.artifacts);
      const frame = await rpc('project', { published, proof_part: 'proof', artifacts: wire.payload.artifacts,
        git: { method: request.method, endpoint: route.endpoint, query: new URL(request.url).search.slice(1),
          ...(request.headers.get('git-protocol') ? { protocol: request.headers.get('git-protocol') } : {}), request_part: 'request' } }, credential,
        [...wire.parts, { name: 'request', bytes: body }]);
      if (frame.payload.status !== 200 || !frame.output) { void frame.output?.cancel().catch(() => {}); throw new Error('Native Git projection failed'); }
      return new Response(frame.output, { headers: { 'content-type': frame.payload.content_type } });
    },
  };
  return hostedGateway({ coordinator, staging, native, authorizeRequest: async (repository, action, credential) =>
    (await plain('authorize', { repository, action }, credential)).authorized === true });
}
