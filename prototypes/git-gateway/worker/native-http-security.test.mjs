// SPDX-License-Identifier: Apache-2.0
// Independent trust-boundary tests for the real native-http/staging/coordinator composition.
// Storage, R2, Artifacts, native authority, and native RPC responses here are controlled adapters;
// cloudflare/publication-workerd.test.mjs separately exercises actual local workerd persistence.
import test from 'node:test';
import assert from 'node:assert/strict';
import { composeHostedGateway, nativeHttp } from './native-http.mjs';
import { FRAME_MIME, MAX_FRAME_REQUEST, MAX_FRAME_RESPONSE, encodeFrame, decodeFrame } from './native-frame.mjs';
import { canonical, digest, operationId } from './publication.mjs';

const encoder = new TextEncoder(), decoder = new TextDecoder();
const copy = value => structuredClone(value);
const sameBytes = (left, right) => left instanceof Uint8Array && left.length === right.length && left.every((value, index) => value === right[index]);
const frameResponse = (payload, parts = []) => {
  const frame = encodeFrame(payload, parts, MAX_FRAME_RESPONSE);
  return new Response(frame.body, { headers: { 'content-type': FRAME_MIME, 'content-length': String(frame.length) } });
};
const SESSION = 'Bearer PUBLIC_NATIVE_HTTP_USER_SESSION_TEST_ONLY';
const SERVICE = 'Bearer PUBLIC_NATIVE_HTTP_SERVICE_GRANT_TEST_ONLY';
const state = character => 'hs-' + character.repeat(52);
const request = (service = 'git-upload-pack', extra = {}) => new Request(
  `https://caller-controlled.invalid/repositories/toy.git/info/refs?service=${service}`, {
    headers: { authorization: SESSION, ...extra },
  });

async function fixture() {
  const bytes = encoder.encode('explicit synthetic native source for boundary checks');
  const proof = encoder.encode('explicit synthetic token-free native proof');
  const artifact = { sha256: await digest(bytes), size: bytes.length, kind: 'pack' };
  const intent = { schema: 2,
    scope: { tenant_spool_id: '11111111-1111-4111-8111-111111111111', spool_id: '22222222-2222-4222-8222-222222222222',
      repository: 'toy', repo_path: 'org/toy', thread_id: '3'.repeat(64), thread: 'main', disclosure_audience: 'public' },
    actor: 'user:verified', gateway_signer: '4'.repeat(64), billing_owner: 'account:owner',
    expected_catalog: null, expected_native: null, expected_generation: null, old_git: null,
    new_git: '7'.repeat(40), native_state: state('b'), authority_generation: '8'.repeat(64),
    history: [{ state: state('b'), parents: [], artifacts: [artifact] }] };
  const operation = await operationId(intent), pin = '9'.repeat(40);
  const records = new Map(), objects = new Map(), calls = [], artifactsCalls = [];
  const control = { plan: { intent: copy(intent), proof_part: 'proof',
    artifacts: [{ sha256: artifact.sha256, bytes_part: `artifact/${artifact.sha256}` }] },
    planParts: [{ name: 'proof', bytes: proof.slice() }, { name: `artifact/${artifact.sha256}`, bytes: bytes.slice() }],
    mutateReceipt: null, catalog: null, corruptCatalog: false, artifactThrows: false,
    denyRequest: false, denyAuthority: false, denyProjectionAuthority: false,
    largeProjection: false, nativeOutputStarted: false, largeOutputPulled: 0, largeOutputCancelled: false,
    catalogReadsAtProject: 0, catalogFailure: false, projectStatus: 200, storageReads: 0 };
  const storage = {
    async get(key) { control.storageReads++; return copy(records.get(key)); },
    async transaction(action) {
      const draft = new Map([...records].map(([key, value]) => [key, copy(value)]));
      const result = await action({ get: async key => copy(draft.get(key)),
        put: async (key, value) => { draft.set(key, copy(value)); }, delete: async key => draft.delete(key) });
      records.clear(); for (const [key, value] of draft) records.set(key, value);
      return result;
    },
  };
  const bucket = {
    async get(key) {
      const value = objects.get(key); if (!value) return null;
      return { size: value.length, arrayBuffer: async () => value.slice().buffer };
    },
    async put(key, value, options) {
      assert.deepEqual(options.onlyIf, { etagDoesNotMatch: '*' });
      assert.equal(options.sha256, await digest(value));
      if (!objects.has(key)) objects.set(key, value.slice());
    },
  };
  const artifacts = { async get(...args) {
    artifactsCalls.push(['get', ...args]);
    return { async readFile(...readArgs) {
      artifactsCalls.push(['readFile', ...readArgs]);
      if (control.artifactThrows) throw new Error('Synthetic Artifacts read failed');
      assert.ok(control.catalog, 'catalog read cannot precede native catalog publication');
      const bytes = control.catalog.slice(); if (control.corruptCatalog) bytes[0] ^= 1;
      return new Blob([bytes]);
    }, [Symbol.dispose]() { artifactsCalls.push(['dispose']); } };
  } };
  const binding = { async fetch(req) {
    assert.equal(control.nativeOutputStarted, false, 'A single-request native server cannot answer another RPC while output is unread');
    const raw = new Uint8Array(await req.arrayBuffer());
    const headerLength = new DataView(raw.buffer).getUint32(4, false);
    const headerText = decoder.decode(raw.subarray(8, 8 + headerLength));
    assert.equal(headerText, canonical(JSON.parse(headerText)), 'Rust /native/v1 requires byte-exact canonical frame metadata');
    const { payload, parts } = await decodeFrame(new Response(raw, { headers: req.headers }), { maximum: MAX_FRAME_REQUEST });
    calls.push({ url: req.url, redirect: req.redirect, rawLength: raw.length, parts,
      headers: Object.fromEntries(req.headers), payload });
    const method = payload.method;
    if (method === 'authorize' && payload.repository)
      return frameResponse({ authorized: !control.denyRequest && req.headers.get('authorization') === SESSION && payload.repository === 'toy' });
    if (method === 'authorize') {
      if (control.denyAuthority) return new Response(null, { status: 403 });
      return frameResponse({ scope: copy(intent.scope), actor: intent.actor,
        gateway_signer: intent.gateway_signer, billing_owner: intent.billing_owner,
        authority_generation: intent.authority_generation, native_state: intent.native_state, native_generation: 7 });
    }
    if (method === 'bootstrap-plan') {
      assert.equal(payload.expected_native, intent.native_state);
      return frameResponse(control.plan, control.planParts);
    }
    if (method === 'validate-plan')
      return frameResponse({ valid: sameBytes(parts.get(payload.proof_part), proof) });
    if (method === 'bootstrap') {
      const receipt = { schema: 1, kind: 'native-bootstrap', operation: await operationId(payload.intent),
        native_state: payload.intent.native_state, generation: 7,
        authority_generation: payload.intent.authority_generation,
        history_sha256: await digest(encoder.encode(canonical(payload.intent.history))),
        actor: payload.intent.actor, gateway_signer: payload.intent.gateway_signer, billing_owner: payload.intent.billing_owner };
      control.mutateReceipt?.(receipt);
      return frameResponse(receipt);
    }
    if (method === 'catalog-publish') {
      assert.equal(payload.repository, 'toy'); assert.equal(payload.expected, '0'.repeat(40));
      assert.deepEqual(parts.get(payload.proof_part), proof);
      if (control.catalogFailure) return new Response(null, { status: 503 });
      const manifest = parts.get(payload.manifest_part);
      if (control.catalog) assert.deepEqual(manifest, control.catalog);
      else control.catalog = manifest;
      return frameResponse({ pin });
    }
    if (method === 'project') {
      assert.deepEqual(parts.get(payload.proof_part), proof);
      assert.deepEqual(parts.get(payload.artifacts[0].bytes_part), bytes);
      control.catalogReadsAtProject = artifactsCalls.filter(call => call[0] === 'readFile').length;
      // This controlled adapter represents Rust's final current-authority check after Git
      // construction and before its first response frame byte. Rust owns the real check.
      if (control.denyProjectionAuthority) return new Response(null, { status: 403 });
      if (control.largeProjection) {
        control.nativeOutputStarted = true;
        const length = 16 * 1024 * 1024;
        const header = encoder.encode(canonical({ payload: { status: 200, output_part: 'output',
          content_type: 'application/x-git-upload-pack-advertisement' }, parts: [{ name: 'output', length }] }));
        const prefix = new Uint8Array(8 + header.length); prefix.set([72, 71, 70, 49]);
        new DataView(prefix.buffer).setUint32(4, header.length); prefix.set(header, 8);
        let sentHeader = false, remaining = length;
        return new Response(new ReadableStream({ pull(controller) {
          if (!sentHeader) { sentHeader = true; controller.enqueue(prefix); return; }
          if (!remaining) { control.nativeOutputStarted = false; controller.close(); return; }
          const size = Math.min(remaining, 64 * 1024); remaining -= size;
          control.largeOutputPulled += size; controller.enqueue(new Uint8Array(size));
        }, cancel() { control.largeOutputCancelled = true; control.nativeOutputStarted = false; } }, { highWaterMark: 0 }),
        { headers: { 'content-type': FRAME_MIME, 'content-length': String(prefix.length + length) } });
      }
      return frameResponse({ status: control.projectStatus, output_part: 'output',
        content_type: 'application/x-git-upload-pack-advertisement' },
      [{ name: 'output', bytes: encoder.encode('synthetic projected Git advertisement') }]);
    }
    throw new Error(`Unexpected native method ${method}`);
  } };
  const config = { storage, bucket, artifacts, catalogNames: { toy: 'trusted-artifacts-catalog' },
    bootstrapHeads: { toy: intent.native_state }, binding, serviceAuthorization: SERVICE };
  return { intent, operation, artifact, bytes, proof, pin, records, objects, calls, artifactsCalls, control,
    config, gateway: composeHostedGateway(config) };
}
async function failsClosed(response, status = 503) {
  assert.equal(response.status, status); assert.equal(await response.text(), '');
  assert.equal(response.headers.get('cache-control'), 'no-store');
}
const methods = f => f.calls.map(call => call.payload.method);

test('composed gateway ignores caller identity/routing headers and never sends grants to Artifacts or durable bytes', async () => {
  const f = await fixture();
  const initialized = await f.gateway.fetch(request('git-receive-pack'));
  assert.equal(initialized.status, 200); await initialized.text();
  const response = await f.gateway.fetch(request('git-upload-pack', {
    'x-gateway-service-authorization': 'spoofed-service', 'x-actor': 'user:attacker',
    'x-gateway-signer': 'forged-signer', 'x-billing-owner': 'forged-owner',
    'x-native-url': 'https://attacker.invalid/collect', 'x-catalog-repository': 'attacker-catalog',
    'x-forwarded-host': 'attacker.invalid', 'git-protocol': 'version=2',
  }));
  assert.equal(response.status, 200); assert.equal(await response.text(), 'synthetic projected Git advertisement');
  assert.ok(f.calls.length > 5);
  for (const call of f.calls) {
    assert.equal(call.url, 'http://native-container.invalid:8080/native/v1'); assert.equal(call.redirect, 'manual');
    assert.deepEqual(call.headers, { authorization: SESSION, 'content-type': FRAME_MIME,
      'content-length': String(call.rawLength),
      'x-gateway-service-authorization': SERVICE });
    assert.doesNotMatch(JSON.stringify(call.payload), /spoofed|attacker|forged|PUBLIC_NATIVE_HTTP_/);
  }
  const project = f.calls.find(call => call.payload.method === 'project').payload;
  assert.deepEqual(project.git, { method: 'GET', endpoint: 'info/refs', query: 'service=git-upload-pack',
    protocol: 'version=2', request_part: 'request' });
  assert.equal(f.calls.find(call => call.payload.method === 'project').parts.get('request').length, 0);
  assert.ok(f.artifactsCalls.length > 0);
  for (let index = 0; index < f.artifactsCalls.length; index += 3)
    assert.deepEqual(f.artifactsCalls.slice(index, index + 3), [['get', 'trusted-artifacts-catalog'],
      ['readFile', { ref: f.pin, path: 'manifest.json' }], ['dispose']]);
  assert.doesNotMatch(canonical([...f.records]), /PUBLIC_NATIVE_HTTP_|authorization|Bearer/);
  for (const bytes of f.objects.values()) assert.doesNotMatch(decoder.decode(bytes), /PUBLIC_NATIVE_HTTP_|authorization|Bearer/);
  assert.equal(methods(f).includes('submit'), false, 'bootstrap must never submit a Git acceptance');
});

test('an ordinary reader cannot initialize an absent catalog or stage native source', async () => {
  const f = await fixture();
  await failsClosed(await f.gateway.fetch(request()), 403);
  assert.deepEqual(methods(f), ['authorize']);
  assert.equal(f.records.size, 0); assert.equal(f.objects.size, 0); assert.deepEqual(f.artifactsCalls, []);
});

test('denied initial authority blocks every catalog, journal, staging, and native-plan lookup', async () => {
  const f = await fixture(); f.control.denyRequest = true;
  await failsClosed(await f.gateway.fetch(request()), 403);
  assert.deepEqual(methods(f), ['authorize']); assert.equal(f.control.storageReads, 0);
  assert.equal(f.objects.size, 0); assert.equal(f.records.size, 0); assert.deepEqual(f.artifactsCalls, []);
});

test('cross-scope bootstrap plans cannot replace trusted repository or authority identity', async () => {
  for (const mutate of [intent => intent.scope.repository = 'other', intent => intent.scope.repo_path = 'other/toy',
    intent => intent.scope.tenant_spool_id = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
    intent => intent.scope.spool_id = 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb',
    intent => intent.scope.thread_id = 'c'.repeat(64), intent => intent.scope.thread = 'other',
    intent => intent.actor = 'user:attacker', intent => intent.gateway_signer = 'd'.repeat(64),
    intent => intent.billing_owner = 'account:attacker', intent => intent.authority_generation = 'e'.repeat(64)]) {
    const f = await fixture(); mutate(f.control.plan.intent);
    await failsClosed(await f.gateway.fetch(request('git-receive-pack')));
    assert.equal(f.objects.size, 0); assert.equal(f.records.size, 0); assert.deepEqual(f.artifactsCalls, []);
    assert.equal(methods(f).includes('bootstrap'), false); assert.equal(methods(f).includes('catalog-publish'), false);
    assert.equal(methods(f).includes('project'), false);
  }
});

test('corrupt native frame references, proof, and source cannot reach bootstrap publication', async () => {
  for (const mutate of [control => control.plan.proof_part = 'unknown',
    control => control.planParts[0].bytes = encoder.encode('wrong proof'),
    control => control.planParts[1].bytes = encoder.encode('wrong source'),
    control => control.plan.artifacts[0].sha256 = 'a'.repeat(64), control => control.plan.artifacts = []]) {
    const f = await fixture(); mutate(f.control);
    await failsClosed(await f.gateway.fetch(request('git-receive-pack')));
    assert.equal(f.objects.size, 0); assert.equal(f.records.size, 0); assert.deepEqual(f.artifactsCalls, []);
    assert.equal(methods(f).includes('bootstrap'), false); assert.equal(methods(f).includes('project'), false);
  }
});

test('corrupt native bootstrap receipts cannot publish public source, catalog, or a success response', async () => {
  for (const field of ['kind', 'operation', 'native_state', 'history_sha256', 'actor', 'gateway_signer', 'billing_owner']) {
    const f = await fixture(); f.control.mutateReceipt = receipt => { receipt[field] = 'corrupt'; };
    await failsClosed(await f.gateway.fetch(request('git-receive-pack')));
    assert.equal(f.records.get('pending:toy').stage, 'prepared'); assert.equal(f.records.has('current:toy'), false);
    assert.equal([...f.objects.keys()].some(key => key.startsWith('native/source/')), false);
    assert.equal(methods(f).includes('catalog-publish'), false); assert.equal(methods(f).includes('submit'), false);
    assert.deepEqual(f.artifactsCalls, []);
  }
});

test('pending bootstrap is never mistaken for an absent repository on read or receive discovery', async () => {
  const f = await fixture(); f.control.catalogFailure = true;
  await failsClosed(await f.gateway.fetch(request('git-receive-pack')));
  assert.equal(f.records.get('pending:toy').stage, 'accepted');
  const staged = new Map([...f.objects].map(([key, value]) => [key, value.slice()]));
  const planCalls = methods(f).filter(method => method === 'bootstrap-plan').length;
  await failsClosed(await f.gateway.fetch(request()));
  await failsClosed(await f.gateway.fetch(request('git-receive-pack')));
  assert.equal(methods(f).filter(method => method === 'bootstrap-plan').length, planCalls);
  assert.equal(f.records.has('current:toy'), false); assert.deepEqual(f.objects, staged);
  assert.equal(methods(f).includes('project'), false); assert.equal(methods(f).includes('submit'), false);
});

test('corrupt or unreadable Artifacts catalog fails closed, disposes handles, and never retries bootstrap', async () => {
  for (const failure of ['corruptCatalog', 'artifactThrows']) {
    const f = await fixture(); const first = await f.gateway.fetch(request('git-receive-pack'));
    assert.equal(first.status, 200); await first.text();
    const before = copy([...f.records]); f.control[failure] = true;
    f.calls.length = 0; f.artifactsCalls.length = 0;
    await failsClosed(await f.gateway.fetch(request()));
    assert.equal(methods(f).includes('bootstrap-plan'), false); assert.equal(methods(f).includes('project'), false);
    assert.deepEqual([...f.records], before);
    assert.deepEqual(f.artifactsCalls, [['get', 'trusted-artifacts-catalog'],
      ['readFile', { ref: f.pin, path: 'manifest.json' }], ['dispose']]);
  }
});

test('native final-authority denial before response framing withholds all projected bytes', async () => {
  const f = await fixture(); const first = await f.gateway.fetch(request('git-receive-pack'));
  assert.equal(first.status, 200); await first.text();
  f.control.denyProjectionAuthority = true; f.calls.length = 0; f.artifactsCalls.length = 0;
  await failsClosed(await f.gateway.fetch(request()));
  assert.equal(methods(f).includes('project'), true);
  assert.equal(methods(f).at(-1), 'project');
  assert.equal(f.control.catalogReadsAtProject, 2, 'Both current and final Worker catalog verification precede projection');
});

test('an unread large native output starts after Worker verification and never triggers another native RPC', async () => {
  const f = await fixture(); const first = await f.gateway.fetch(request('git-receive-pack'));
  assert.equal(first.status, 200); await first.text();
  f.control.largeProjection = true; f.calls.length = 0; f.artifactsCalls.length = 0;
  const output = await f.gateway.fetch(request());
  try {
    assert.equal(output.status, 200); assert.equal(methods(f).at(-1), 'project');
    assert.equal(f.control.catalogReadsAtProject, 2);
    assert.equal(f.control.largeOutputPulled, 0, 'Projection remains unread rather than being buffered to make room for another RPC');
    const calls = f.calls.length; await new Promise(resolve => setTimeout(resolve, 5));
    assert.equal(f.calls.length, calls); assert.equal(f.control.largeOutputPulled, 0);
  } finally { await output.body?.cancel(); }
  assert.equal(f.control.largeOutputCancelled, true);
});

test('unregistered trusted catalog configuration fails closed despite valid native authority', async () => {
  const f = await fixture(); const gateway = composeHostedGateway({ ...f.config, catalogNames: {} });
  await failsClosed(await gateway.fetch(request('git-receive-pack')));
  assert.equal(f.records.has('current:toy'), false); assert.deepEqual(f.artifactsCalls, []);
  assert.equal(methods(f).includes('catalog-publish'), false); assert.equal(methods(f).includes('project'), false);
});

test('native bridge cancels an indefinitely streaming response after its timeout', async () => {
  let cancelled = false;
  const rpc = nativeHttp({ serviceAuthorization: SERVICE, timeoutMs: 10, binding: { fetch: async () => new Response(
    new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array([72])); }, cancel() { cancelled = true; } }),
    { headers: { 'content-type': FRAME_MIME } }) } });
  await assert.rejects(rpc('authorize', {}, SESSION), /timeout/);
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(cancelled, true);
});

test('native bridge rejects malformed UTF-8 rather than parsing replacement characters as identity', async () => {
  const header = new Uint8Array([...encoder.encode('{"parts":[],"payload":{"actor":"'), 0xc3, 0x28, ...encoder.encode('"}}\n')]);
  const body = new Uint8Array(8 + header.length); body.set([72, 71, 70, 49]);
  new DataView(body.buffer).setUint32(4, header.length); body.set(header, 8);
  const rpc = nativeHttp({ serviceAuthorization: SERVICE, binding: { fetch: async () => new Response(body,
    { headers: { 'content-type': FRAME_MIME } }) } });
  await assert.rejects(rpc('authorize', {}, SESSION), /encoded data|encoding/i);
});


test('native HTTP emits the exact nested canonical HGF1 header required by the Rust listener', async () => {
  let raw;
  const rpc = nativeHttp({ serviceAuthorization: SERVICE, binding: { fetch: async req => {
    raw = new Uint8Array(await req.arrayBuffer()); return frameResponse({ authorized: true });
  } } });
  const payload = { z: 'last', nested: { z: 2, a: 1 }, array: [{ z: 4, a: 3 }] };
  await rpc('authorize', payload, SESSION);
  assert.deepEqual([...raw.subarray(0, 4)], [72, 71, 70, 49]);
  const headerLength = new DataView(raw.buffer).getUint32(4, false);
  assert.equal(raw.length, headerLength + 8);
  const header = decoder.decode(raw.subarray(8));
  assert.equal(header, canonical({ payload: { ...payload, method: 'authorize' }, parts: [] }));
  assert.ok(header.endsWith('\n'));
});

test('native projection must explicitly succeed before any projected bytes leave the gateway', async () => {
  for (const status of [undefined, null, 0, 204, 302, 403, 503]) {
    const f = await fixture();
    const initialized = await f.gateway.fetch(request('git-receive-pack'));
    assert.equal(initialized.status, 200); await initialized.text();
    f.control.projectStatus = status;
    await failsClosed(await f.gateway.fetch(request()));
    assert.equal(methods(f).includes('project'), true);
  }
});
