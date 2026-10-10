// SPDX-License-Identifier: Apache-2.0
// Actual local workerd + persistent R2/SQLite Durable Objects, not object-shaped storage
// substitutes. The Worker fixture truthfully mocks native, authority, and Git catalog APIs.
// No Docker, deployment, real credentials, remote service, or external publication is used.
import test, { after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { canonical, digest, operationId } from '../publication.mjs';

const root = dirname(fileURLToPath(import.meta.url));
const encoder = new TextEncoder();
const writer = suffix => `PUBLIC_MOCK_WRITER_SESSION_${suffix}`;
const reader = 'PUBLIC_MOCK_READER_SESSION_FRESH';
const state = character => 'hs-' + character.repeat(52);
const phaseTotals = Object.create(null);
let observedRuntimeWindows = 0;
const phaseNames = new Set(['r2_get_actual', 'r2_put_actual', 'authority_adapter', 'native_accept_adapter',
  'native_bootstrap_adapter', 'native_refresh_adapter', 'native_source_adapter', 'catalog_cas_adapter',
  'catalog_read_adapter', 'receive_discovery_recovery', 'current_verification']);
after(async () => {
  if (!process.env.HEDDLE_WORKERD_TRACE_PATH) return;
  const evidence = { schema: 1, selection: process.env.HEDDLE_WORKERD_TRACE_SCOPE === 'refresh-only' ?
    'native-refresh regression tests' : 'complete publication recovery regression tests', actual: 'Local workerd R2 and SQLite Durable Object storage',
    controlled_adapters: ['native acceptance/bootstrap/refresh', 'current authority', 'catalog CAS/read'],
    exclusions: ['No Rust runtime traces', 'No live Weft or Artifacts timing', 'No abrupt-crash durability claim'],
    timing: 'workerd Date.now elapsed milliseconds; phases overlap and CPU-only phases may quantize to zero',
    phase_scope: 'Coordinator adapter invocation counts; R2 get/put includes staging. R2 get excludes separate body consumption and hashing. Nested pure mock staging authorization is not separately counted.',
    runtime_windows: observedRuntimeWindows, phases: Object.fromEntries([...phaseNames].sort().map(phase =>
      [phase, phaseTotals[phase] ?? { count: 0, failed: 0, elapsed_ms: 0 }])) };
  await writeFile(process.env.HEDDLE_WORKERD_TRACE_PATH, JSON.stringify(evidence, null, 2) + '\n');
});


async function fixture() {
  const bytes = encoder.encode('receiver-owned native bytes for persistent workerd recovery');
  const artifact = { sha256: await digest(bytes), size: bytes.length, kind: 'pack' };
  const intent = { schema: 1,
    scope: { tenant_spool_id: '11111111-1111-4111-8111-111111111111', spool_id: '22222222-2222-4222-8222-222222222222',
      repository: 'toy', repo_path: 'org/toy', thread_id: '3'.repeat(64), thread: 'main', disclosure_audience: 'public' },
    actor: 'user:verified', gateway_signer: '4'.repeat(64), billing_owner: 'account:owner',
    expected_catalog: '5'.repeat(40), expected_native: state('a'), expected_generation: 7,
    old_git: '6'.repeat(40), new_git: '7'.repeat(40), native_state: state('b'), authority_generation: '8'.repeat(64),
    history: [{ state: state('a'), parents: [], artifacts: [artifact] },
      { state: state('b'), parents: [state('a')], artifacts: [artifact] }] };
  return { bytes: Array.from(bytes), artifact, intent };
}

async function runtime(t) {
  const dir = await mkdtemp(join(tmpdir(), 'heddle-publication-workerd-'));
  const options = convertV4MiniflareOptions({
    name: 'heddle-publication-recovery-test',
    modulesRoot: dirname(root),
    modules: [{ type: 'ESModule', path: join(root, 'fixtures/publication-workerd.mjs') },
      { type: 'ESModule', path: join(root, '../publication.mjs') },
      { type: 'ESModule', path: join(root, '../native-staging.mjs') }],
    compatibilityDate: '2026-10-01',
    durableObjects: { PUBLICATION: { className: 'PublicationRecoveryTestObject', useSQLite: true } },
    r2Buckets: { SOURCE_BYTES: 'heddle-native-source-test' },
    resourcePersistencePath: join(dir, 'persistent'), resourceTmpPath: join(dir, 'temporary'),
    host: '127.0.0.1', port: 0, cf: false, telemetry: { enabled: false },
    outboundService: () => new Response('External network disabled by local test', { status: 502 }),
  });
  let mf = new Miniflare(options);
  async function capturePhases() {
    const response = await mf.dispatchFetch('http://localhost/phase-trace');
    assert.equal(response.status, 200);
    const phases = await response.json(); observedRuntimeWindows++;
    for (const [phase, values] of Object.entries(phases)) {
      assert.ok(phaseNames.has(phase), 'phase evidence must contain allowlisted static labels only');
      assert.deepEqual(Object.keys(values).sort(), ['count', 'elapsed_ms', 'failed']);
      const total = phaseTotals[phase] ??= { count: 0, failed: 0, elapsed_ms: 0 };
      for (const key of ['count', 'failed', 'elapsed_ms']) {
        assert.ok(Number.isSafeInteger(values[key]) && values[key] >= 0);
        total[key] += values[key];
      }
    }
  }
  t.after(async () => {
    try { if (process.env.HEDDLE_WORKERD_TRACE_PATH) await capturePhases(); }
    finally { await mf.dispose(); await rm(dir, { recursive: true, force: true }); }
  });
  await mf.ready;
  return {
    async request(path, { body, credential = writer('ORIGINAL') } = {}) {
      const response = await mf.dispatchFetch(`http://localhost${path}`, {
        method: body === undefined ? 'GET' : 'POST',
        headers: { authorization: credential, 'content-type': 'application/json' },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      return { status: response.status, body: await response.json() };
    },
    async inspect() {
      const response = await this.request('/inspect'); assert.equal(response.status, 200);
      return response.body;
    },
    async restart() {
      if (process.env.HEDDLE_WORKERD_TRACE_PATH) await capturePhases();
      await mf.dispose();
      assert.notEqual((await readdir(join(dir, 'persistent'))).length, 0, 'workerd must leave disk-backed state');
      mf = new Miniflare(options);
      await mf.ready;
    },
    async bucket() { return mf.getR2Bucket('SOURCE_BYTES'); },
  };
}

async function prepare(t, fault, { bootstrap = false, useStaging = false } = {}) {
  const f = await fixture(), rt = await runtime(t);
  if (bootstrap) Object.assign(f.intent, { schema: 2, expected_catalog: null, expected_native: null,
    expected_generation: null, old_git: null });
  assert.equal((await rt.request('/setup', { body: { ...f, fault, useStaging } })).status, 200);
  return { f, rt, operation: await operationId(f.intent), key: `native/source/${f.artifact.sha256}` };
}
async function rejected(response, pattern) {
  assert.equal(response.status, 503);
  assert.match(response.body.error, pattern);
}

for (const phase of ['after-native-commit', 'before-catalog-publish', 'after-catalog-commit']) {
  test(`real workerd restart repairs ${phase} through ordinary receive discovery`, { timeout: 30000 }, async t => {
    const { f, rt, operation, key } = await prepare(t, phase);
    await rejected(await rt.request('/publish', { body: f.intent }), new RegExp(phase));
    const before = await rt.inspect();
    assert.equal(before.records['pending:toy'].operation, operation);
    assert.equal(before.records['pending:toy'].stage, phase === 'after-native-commit' ? 'prepared' : 'accepted');
    assert.equal(before.records['current:toy'], undefined);
    assert.equal(before.records[`receipt:${operation}`], undefined);
    assert.equal(before.mock.nativeMutations, 1);
    assert.equal(before.mock.catalogMutations, phase === 'after-catalog-commit' ? 1 : 0);
    await rejected(await rt.request('/current', { credential: reader }), /recovery/);
    await rt.restart();
    const restored = await rt.inspect();
    assert.notEqual(restored.instance, before.instance, 'a different workerd DO instance must load the journal');
    assert.deepEqual(restored.records, before.records, 'journal must survive process disposal and recreation');
    assert.deepEqual(restored.mock, before.mock, 'controlled native/catalog mock commits also survive restart');
    if (phase !== 'after-native-commit') {
      const source = await (await rt.bucket()).get(key);
      assert.ok(source, 'accepted source bytes must survive the workerd process restart');
      assert.deepEqual(Array.from(new Uint8Array(await source.arrayBuffer())), f.bytes);
    }
    await rejected(await rt.request('/current', { credential: reader }), /recovery/);
    const recovered = await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') });
    assert.equal(recovered.status, 200, JSON.stringify(recovered.body));
    assert.equal(recovered.body.stage, 'published');
    assert.equal(recovered.body.operation, operation);
    const after = await rt.inspect();
    assert.equal(after.records['pending:toy'], undefined);
    assert.deepEqual(after.records['current:toy'], { operation, pin: recovered.body.pin });
    assert.deepEqual(after.records[`receipt:${operation}`], recovered.body);
    assert.equal(after.mock.nativeMutations, 1, 'recovery must not repeat a native commit');
    assert.equal(after.mock.catalogMutations, 1, 'ambiguous catalog retry must not repeat the catalog commit');
    assert.equal(after.mock.acceptCalls, phase === 'after-native-commit' ? 2 : 1);
    assert.equal(after.mock.artifactReads, 1, 'persisted R2 source must not be recopied on recovery');
    const source = await (await rt.bucket()).get(key);
    assert.equal(source.customMetadata.storage_class, 'native-source');
    assert.equal(source.customMetadata.content_sha256, f.artifact.sha256);
    assert.deepEqual(Array.from(new Uint8Array(await source.arrayBuffer())), f.bytes);
    const replay = await rt.request('/publish', { body: f.intent, credential: writer('THIRD') });
    assert.equal(replay.status, 200); assert.deepEqual(replay.body, recovered.body);
    const read = await rt.request('/current', { credential: reader });
    assert.equal(read.status, 200); assert.deepEqual(read.body, recovered.body);
    const final = await rt.inspect();
    assert.deepEqual(final.mock, after.mock, 'completed replay and authenticated read must be non-mutating');
    assert.doesNotMatch(canonical(final.records), /PUBLIC_MOCK_/, 'durable production records must not store sessions');
  });
}

test('persisted accepted journal cannot recover using revoked or read-only mock authority', { timeout: 30000 }, async t => {
  const { f, rt, operation } = await prepare(t, 'before-catalog-publish');
  await rejected(await rt.request('/publish', { body: f.intent }), /before-catalog-publish/);
  assert.equal((await rt.request('/control', { body: { denied: true } })).status, 200);
  await rt.restart();
  for (const credential of [writer('FRESH'), reader, 'unrecognized-session'])
    await rejected(await rt.request('/current?receive-discovery=1', { credential }), /authority denied/);
  const denied = await rt.inspect();
  assert.equal(denied.records['pending:toy'].operation, operation);
  assert.equal(denied.mock.nativeMutations, 1); assert.equal(denied.mock.catalogMutations, 0);
  assert.equal(denied.records['current:toy'], undefined);
  assert.equal((await rt.request('/control', { body: { denied: false } })).status, 200);
  await rejected(await rt.request('/current?receive-discovery=1', { credential: reader }), /authority denied/);
  assert.equal((await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') })).status, 200);
});

test('actual persisted R2 corruption blocks recovery and is never overwritten', { timeout: 30000 }, async t => {
  const { f, rt, operation, key } = await prepare(t, 'before-catalog-publish');
  await rejected(await rt.request('/publish', { body: f.intent }), /before-catalog-publish/);
  const corrupt = Uint8Array.from(f.bytes); corrupt[0] ^= 1;
  await (await rt.bucket()).put(key, corrupt);
  await rt.restart();
  await rejected(await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') }), /content mismatch/);
  const after = await rt.inspect();
  assert.equal(after.records['pending:toy'].operation, operation);
  assert.equal(after.records['current:toy'], undefined);
  assert.equal(after.mock.nativeMutations, 1); assert.equal(after.mock.catalogMutations, 0);
  assert.equal(after.mock.artifactReads, 1, 'corrupt R2 bytes must not silently trigger a rewrite');
  const stored = await (await rt.bucket()).get(key);
  assert.deepEqual(new Uint8Array(await stored.arrayBuffer()), corrupt);
});

test('a completed real DO receipt cannot serve missing or corrupt R2 source after restart', { timeout: 30000 }, async t => {
  const { f, rt, key } = await prepare(t, null);
  assert.equal((await rt.request('/publish', { body: f.intent })).status, 200);
  await rt.restart();
  const before = await rt.inspect();
  const bucket = await rt.bucket();
  await bucket.delete(key);
  await rejected(await rt.request('/current', { credential: reader }), /source unavailable/);
  await rejected(await rt.request('/publish', { body: f.intent, credential: writer('FRESH') }), /source unavailable/);
  const corrupt = Uint8Array.from(f.bytes); corrupt[0] ^= 1;
  await bucket.put(key, corrupt);
  await rejected(await rt.request('/current', { credential: reader }), /content mismatch/);
  await rejected(await rt.request('/publish', { body: f.intent, credential: writer('FRESH') }), /content mismatch/);
  const after = await rt.inspect();
  assert.deepEqual(after.records, before.records);
  assert.deepEqual(after.mock, before.mock, 'a durable receipt is not permission to recreate or trust absent source');
});

test('an intervening mock catalog head cannot be overwritten by restarted recovery', { timeout: 30000 }, async t => {
  const { f, rt } = await prepare(t, 'before-catalog-publish');
  await rejected(await rt.request('/publish', { body: f.intent }), /before-catalog-publish/);
  const changedHead = 'a'.repeat(40);
  assert.equal((await rt.request('/control', { body: { catalogHead: changedHead } })).status, 200);
  await rt.restart();
  await rejected(await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') }), /expected-old conflict/);
  const after = await rt.inspect();
  assert.equal(after.mock.catalogHead, changedHead); assert.equal(after.mock.catalogMutations, 0);
  assert.equal(after.records['current:toy'], undefined); assert.equal(after.records['pending:toy'].stage, 'accepted');
});

test('bootstrap publishes an existing native state without accepting a Git push', { timeout: 30000 }, async t => {
  const { f, rt, operation } = await prepare(t, null, { bootstrap: true });
  await rejected(await rt.request('/publish', { body: f.intent }), /native acceptance command/);
  const published = await rt.request('/bootstrap', { body: f.intent });
  assert.equal(published.status, 200, JSON.stringify(published.body));
  assert.equal(published.body.operation, operation);
  assert.equal(published.body.receipt.kind, 'native-bootstrap');
  assert.equal(published.body.receipt.previous_native, undefined, 'bootstrap must not fabricate a native push receipt');
  const after = await rt.inspect();
  assert.equal(after.mock.bootstrapCalls, 1);
  assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
  assert.equal(after.mock.nativeReceipt, null); assert.equal(after.mock.nativeGeneration, 7);
  assert.equal(after.mock.catalog.expected, null, 'first catalog publication requires expected-absent CAS');
  assert.equal(after.mock.catalogMutations, 1);
  await rejected(await rt.request('/publish', { body: f.intent, credential: writer('FRESH') }), /native acceptance command/);
  assert.equal((await rt.request('/current', { credential: reader })).status, 200);
  assert.deepEqual((await rt.inspect()).mock, after.mock);
});

test('ambiguous bootstrap proof and catalog responses recover across actual workerd restarts', { timeout: 30000 }, async t => {
  for (const fault of ['after-bootstrap-proof', 'before-catalog-publish', 'after-catalog-commit']) {
    const { f, rt, operation } = await prepare(t, fault, { bootstrap: true });
    await rejected(await rt.request('/bootstrap', { body: f.intent }), new RegExp(fault));
    const before = await rt.inspect();
    assert.equal(before.records['current:toy'], undefined);
    assert.equal(before.records['pending:toy'].operation, operation);
    assert.equal(before.records['pending:toy'].stage, fault === 'after-bootstrap-proof' ? 'prepared' : 'accepted');
    await rt.restart();
    const restored = await rt.inspect();
    assert.notEqual(restored.instance, before.instance); assert.deepEqual(restored.records, before.records);
    const recovered = await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') });
    assert.equal(recovered.status, 200, JSON.stringify(recovered.body));
    assert.equal(recovered.body.operation, operation); assert.equal(recovered.body.receipt.kind, 'native-bootstrap');
    const after = await rt.inspect();
    assert.equal(after.records['pending:toy'], undefined);
    assert.equal(after.mock.bootstrapCalls, fault === 'after-bootstrap-proof' ? 2 : 1);
    assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
    assert.equal(after.mock.catalogMutations, 1); assert.equal(after.mock.artifactReads, 1);
    const replay = await rt.request('/bootstrap', { body: f.intent, credential: writer('THIRD') });
    assert.equal(replay.status, 200); assert.deepEqual(replay.body, recovered.body);
    assert.deepEqual((await rt.inspect()).mock, after.mock);
  }
});

test('bootstrap expected-absent catalog CAS loser preserves the winner after restart', { timeout: 30000 }, async t => {
  const { f, rt, operation } = await prepare(t, 'before-catalog-publish', { bootstrap: true });
  await rejected(await rt.request('/bootstrap', { body: f.intent }), /before-catalog-publish/);
  const winnerPin = 'c'.repeat(40);
  assert.equal((await rt.request('/control', { body: { catalogHead: winnerPin } })).status, 200);
  await rt.restart();
  await rejected(await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') }), /expected-old conflict/);
  const after = await rt.inspect();
  assert.equal(after.mock.catalogHead, winnerPin); assert.equal(after.mock.catalogMutations, 0);
  assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
  assert.equal(after.records['pending:toy'].operation, operation);
  assert.equal(after.records['current:toy'], undefined); assert.equal(after.records[`receipt:${operation}`], undefined);
});

test('a push-like proof cannot impersonate native-bootstrap proof or create a current receipt', { timeout: 30000 }, async t => {
  const { f, rt, operation } = await prepare(t, null, { bootstrap: true });
  assert.equal((await rt.request('/control', { body: { bootstrapProofKind: 'git-acceptance' } })).status, 200);
  await rejected(await rt.request('/bootstrap', { body: f.intent }), /bootstrap proof mismatch/);
  const after = await rt.inspect();
  assert.equal(after.records['pending:toy'].stage, 'prepared');
  assert.equal(after.records['current:toy'], undefined); assert.equal(after.records[`receipt:${operation}`], undefined);
  assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
  assert.equal(after.mock.artifactReads, 0); assert.equal(after.mock.catalogMutations, 0);
  assert.deepEqual((await (await rt.bucket()).list()).objects, []);
});

async function stageNative(rt, f, operation, credential = writer('ORIGINAL'), { checkTransfer = false } = {}) {
  // Explicitly synthetic validator input. This is not a claim to encode genuine native prost proof.
  const proof = encoder.encode(canonical({ kind: 'synthetic-native-proof-test-only', operation,
    source_sha256: f.artifact.sha256 }));
  const staged = await rt.request(checkTransfer ? '/stage-transfer' : '/stage', { credential, body: { intent: f.intent,
    proof: Array.from(proof), artifacts: [{ sha256: f.artifact.sha256, bytes: f.bytes }] } });
  assert.equal(staged.status, 200, JSON.stringify(staged.body));
  assert.equal(staged.body.operation, operation);
  if (checkTransfer) assert.deepEqual(staged.body.ownership,
    { synchronously_detached: true, rejected_mutations: 2, original_buffers: 2 });
  return proof;
}

test('actual workerd transfers staging buffer ownership before await and caller mutation cannot alter real R2 bytes', { timeout: 30000 }, async t => {
  const { f, rt, operation, key } = await prepare(t, null, { useStaging: true });
  const proof = await stageNative(rt, f, operation, writer('ORIGINAL'), { checkTransfer: true });
  await assertStagingContainsNoSession(rt, f, operation, proof);
  await rt.restart();
  const published = await rt.request('/publish', { body: f.intent, credential: writer('FRESH') });
  assert.equal(published.status, 200, JSON.stringify(published.body));
  await assertStagingContainsNoSession(rt, f, operation, proof);
  assert.deepEqual(Array.from(new Uint8Array(await (await (await rt.bucket()).get(key)).arrayBuffer())), f.bytes);
  const streaming = await rt.request('/verify-real-body');
  assert.equal(streaming.status, 200, JSON.stringify(streaming.body));
  assert.deepEqual(streaming.body, { verified: true, array_buffer_calls: 0 });
});

test('actual workerd stages the 64 MiB native plus 17 MiB proof ceiling with transfer and streamed real R2 read-back', { timeout: 90000 }, async t => {
  const { rt } = await prepare(t, null, { bootstrap: true, useStaging: true });
  const staged = await rt.request('/stage-ceiling', { body: {} });
  assert.equal(staged.status, 200, JSON.stringify(staged.body));
  assert.deepEqual(staged.body, { native_bytes: 64 * 1024 * 1024, proof_bytes: 17 * 1024 * 1024,
    synchronously_detached: true, streamed_readbacks: 3, array_buffer_calls: 0 });
  const objects = (await (await rt.bucket()).list()).objects;
  assert.equal(objects.length, 3);
  assert.deepEqual(objects.map(object => object.size).sort((a, b) => b - a).slice(0, 2),
    [64 * 1024 * 1024, 17 * 1024 * 1024]);
  const after = await rt.inspect();
  assert.deepEqual(after.records, {}); assert.equal(after.mock.nativeMutations, 0);
});
async function assertStagingContainsNoSession(rt, f, operation, proof) {
  const bucket = await rt.bucket();
  const prefix = `gateway-staging/${f.intent.scope.tenant_spool_id}/${f.intent.scope.spool_id}/${operation}/`;
  const list = await bucket.list({ prefix });
  assert.deepEqual(list.objects.map(object => object.key).sort(),
    [prefix + f.artifact.sha256, prefix + 'manifest', prefix + 'proof'].sort());
  for (const item of list.objects) {
    const object = await bucket.get(item.key);
    assert.equal(object.customMetadata.storage_class, 'private-native-staging');
    const bytes = new Uint8Array(await object.arrayBuffer());
    assert.doesNotMatch(new TextDecoder().decode(bytes), /PUBLIC_MOCK_|authorization|Biscuit|Bearer/);
    if (item.key.endsWith('/proof')) assert.deepEqual(bytes, proof);
    if (item.key.endsWith('/' + f.artifact.sha256)) assert.deepEqual(Array.from(bytes), f.bytes);
  }
}

test('real private R2 staging survives restart before coordinator submission with a fresh session', { timeout: 30000 }, async t => {
  const { f, rt, operation, key } = await prepare(t, null, { useStaging: true });
  const proof = await stageNative(rt, f, operation);
  const before = await rt.inspect();
  assert.deepEqual(before.records, {}); assert.equal(before.mock.nativeMutations, 0);
  assert.equal(before.mock.bytes, null, 'source bytes must have no mock-storage fallback');
  await assertStagingContainsNoSession(rt, f, operation, proof);
  await rt.restart();
  assert.notEqual((await rt.inspect()).instance, before.instance);
  const published = await rt.request('/publish', { body: f.intent, credential: writer('FRESH') });
  assert.equal(published.status, 200, JSON.stringify(published.body));
  assert.equal(published.body.operation, operation);
  const after = await rt.inspect();
  assert.equal(after.mock.submitCalls, 1); assert.equal(after.mock.freshSessionUsed, true);
  assert.equal(after.mock.nativeMutations, 1); assert.equal(after.mock.catalogMutations, 1);
  assert.equal(after.mock.artifactReads, 0, 'the coordinator must load bytes through actual R2NativeStaging');
  assert.equal(after.mock.bytes, null);
  assert.doesNotMatch(canonical(after.records), /PUBLIC_MOCK_|authorization|Biscuit|Bearer/);
  await assertStagingContainsNoSession(rt, f, operation, proof);
  assert.deepEqual(Array.from(new Uint8Array(await (await (await rt.bucket()).get(key)).arrayBuffer())), f.bytes);
});

test('real R2 staged plan repairs ambiguous native acceptance after cold restart without reusing a session', { timeout: 30000 }, async t => {
  const { f, rt, operation } = await prepare(t, 'after-native-commit', { useStaging: true });
  const proof = await stageNative(rt, f, operation);
  await rejected(await rt.request('/publish', { body: f.intent }), /after-native-commit/);
  const before = await rt.inspect();
  assert.equal(before.records['pending:toy'].stage, 'prepared');
  assert.equal(before.mock.nativeMutations, 1); assert.equal(before.mock.submitCalls, 1);
  assert.equal(before.mock.freshSessionUsed, false); assert.equal(before.mock.bytes, null);
  await rt.restart();
  const recovered = await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') });
  assert.equal(recovered.status, 200, JSON.stringify(recovered.body));
  assert.equal(recovered.body.operation, operation);
  const after = await rt.inspect();
  assert.equal(after.mock.nativeMutations, 1); assert.equal(after.mock.submitCalls, 2);
  assert.equal(after.mock.freshSessionUsed, true); assert.equal(after.mock.catalogMutations, 1);
  assert.equal(after.mock.artifactReads, 0); assert.equal(after.records['pending:toy'], undefined);
  assert.doesNotMatch(canonical(after.records), /PUBLIC_MOCK_|authorization|Biscuit|Bearer/);
  await assertStagingContainsNoSession(rt, f, operation, proof);
});

test('synthetic credential-bearing staging proof is rejected before any real R2 upload', { timeout: 30000 }, async t => {
  const { f, rt, operation } = await prepare(t, null, { useStaging: true });
  const proof = encoder.encode(canonical({ kind: 'synthetic-native-proof-test-only', operation,
    source_sha256: f.artifact.sha256, authorization: writer('ORIGINAL') }));
  await rejected(await rt.request('/stage', { body: { intent: f.intent, proof: Array.from(proof),
    artifacts: [{ sha256: f.artifact.sha256, bytes: f.bytes }] } }), /proof invalid or contains credentials/);
  const after = await rt.inspect();
  assert.equal(after.mock.submitCalls, 0); assert.equal(after.mock.nativeMutations, 0);
  assert.deepEqual(after.records, {});
  assert.deepEqual((await (await rt.bucket()).list()).objects, []);
});

test('incomplete or corrupt actual R2 staging cannot submit native content after restart', { timeout: 30000 }, async t => {
  for (const broken of ['manifest', 'proof']) {
    const { f, rt, operation, key } = await prepare(t, null, { useStaging: true });
    const proof = await stageNative(rt, f, operation);
    const prefix = `gateway-staging/${f.intent.scope.tenant_spool_id}/${f.intent.scope.spool_id}/${operation}/`;
    const bucket = await rt.bucket();
    if (broken === 'manifest') await bucket.delete(prefix + 'manifest');
    else { const corrupt = proof.slice(); corrupt[0] ^= 1; await bucket.put(prefix + 'proof', corrupt); }
    await rt.restart();
    await rejected(await rt.request('/publish', { body: f.intent, credential: writer('FRESH') }),
      broken === 'manifest' ? /object unavailable/ : /content changed/);
    const after = await rt.inspect();
    assert.equal(after.records['pending:toy'].stage, 'prepared');
    assert.equal(after.records['current:toy'], undefined);
    assert.equal(after.mock.submitCalls, 0); assert.equal(after.mock.acceptCalls, 0);
    assert.equal(after.mock.nativeMutations, 0); assert.equal(after.mock.catalogMutations, 0);
    assert.equal(await (await rt.bucket()).get(key), null, 'private staging must not become published source before acceptance');
  }
});

async function prepareRefresh(t, { newHead = false, useStaging = false } = {}) {
  const { f, rt } = await prepare(t, null, { bootstrap: true, useStaging });
  if (useStaging) await stageNative(rt, f, await operationId(f.intent));
  const initial = await rt.request('/bootstrap', { body: f.intent });
  assert.equal(initial.status, 200, JSON.stringify(initial.body));
  const intent = { ...structuredClone(f.intent), schema: 3,
    expected_catalog: initial.body.pin, expected_native: f.intent.native_state,
    expected_generation: 8, old_git: f.intent.new_git };
  if (newHead) {
    intent.native_state = state('c'); intent.new_git = 'c'.repeat(40);
    if (newHead === 'unrelated') intent.history = [{ state: intent.native_state, parents: [], artifacts: [f.artifact] }];
    else intent.history.push({ state: intent.native_state, parents: [f.intent.native_state], artifacts: [f.artifact] });
  }
  assert.equal((await rt.request('/control', { body: { nativeState: intent.native_state, nativeGeneration: 8 } })).status, 200);
  return { f, rt, intent, initial: initial.body };
}

for (const newHead of [false, true, 'unrelated']) {
  test(`real workerd refresh repairs ${newHead === 'unrelated' ? 'an independently moved native head' : newHead ? 'a changed native head' : 'same-head metadata generation advance'} without native acceptance`,
    { timeout: 30000 }, async t => {
      const { rt, intent, initial } = await prepareRefresh(t, { newHead });
      await rejected(await rt.request('/current', { credential: reader }), /changed/);
      await rt.restart();
      const refreshed = await rt.request('/refresh', { body: intent, credential: writer('FRESH') });
      assert.equal(refreshed.status, 200, JSON.stringify(refreshed.body));
      assert.equal(refreshed.body.receipt.kind, 'native-refresh');
      assert.equal(refreshed.body.receipt.generation, intent.expected_generation);
      assert.equal(refreshed.body.receipt.previous_native, undefined);
      assert.equal(refreshed.body.intent.native_state, intent.native_state);
      assert.equal(refreshed.body.intent.new_git, intent.new_git);
      assert.notEqual(refreshed.body.pin, initial.pin);
      const after = await rt.inspect();
      assert.equal(after.mock.refreshCalls, 1); assert.equal(after.mock.bootstrapCalls, 1);
      assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
      assert.equal(after.mock.nativeReceipt, null); assert.equal(after.mock.nativeGeneration, 8);
      assert.equal(after.mock.catalog.expected, initial.pin); assert.equal(after.mock.catalogMutations, 2);
      assert.equal(after.records['pending:toy'], undefined);
      assert.equal(after.records['current:toy'].pin, refreshed.body.pin);
      const current = await rt.request('/current', { credential: reader });
      assert.equal(current.status, 200); assert.deepEqual(current.body, refreshed.body);
      await rejected(await rt.request('/publish', { body: intent, credential: writer('FRESH') }), /native acceptance command/);
      assert.equal((await rt.inspect()).mock.acceptCalls, 0, 'refresh cannot fabricate or submit a Git acceptance');
    });
}

test('native refresh requires a writer even when an authenticated reader sees the stale catalog', { timeout: 30000 }, async t => {
  const { rt, intent, initial } = await prepareRefresh(t);
  const before = await rt.inspect();
  await rejected(await rt.request('/refresh', { body: intent, credential: reader }), /authority denied/);
  const after = await rt.inspect();
  assert.equal(after.records['current:toy'].pin, initial.pin);
  assert.deepEqual(after.records, before.records);
  assert.equal(after.mock.refreshCalls, 0); assert.equal(after.mock.catalogMutations, 1);
});

test('pending native refresh cannot be displaced and cold receive discovery repairs its exact operation', { timeout: 30000 }, async t => {
  const { rt, intent } = await prepareRefresh(t, { newHead: true });
  assert.equal((await rt.request('/control', { body: { fault: 'after-refresh-proof' } })).status, 200);
  await rejected(await rt.request('/refresh', { body: intent, credential: writer('FRESH') }), /after-refresh-proof/);
  const pending = await rt.inspect();
  assert.equal(pending.records['pending:toy'].operation, await operationId(intent));
  assert.equal(pending.records['pending:toy'].stage, 'prepared');
  const other = { ...intent, new_git: 'd'.repeat(40) };
  await rejected(await rt.request('/refresh', { body: other, credential: writer('FRESH') }), /pending|recover/i);
  assert.deepEqual((await rt.inspect()).records, pending.records);
  await rt.restart();
  const recovered = await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') });
  assert.equal(recovered.status, 200, JSON.stringify(recovered.body));
  assert.equal(recovered.body.operation, await operationId(intent));
  assert.equal(recovered.body.receipt.kind, 'native-refresh');
  const after = await rt.inspect();
  assert.equal(after.mock.refreshCalls, 2); assert.equal(after.mock.acceptCalls, 0);
  assert.equal(after.mock.nativeMutations, 0); assert.equal(after.mock.catalogMutations, 2);
  assert.equal(after.records['pending:toy'], undefined);
});

test('native refresh refuses corrupt prior pointers and immutable catalog bytes before replacing current', { timeout: 30000 }, async t => {
  for (const corruption of ['current-pin', 'current-operation', 'catalog']) {
    const { rt, intent } = await prepareRefresh(t);
    if (corruption === 'catalog')
      assert.equal((await rt.request('/control', { body: { corruptCatalog: true } })).status, 200);
    else assert.equal((await rt.request('/corrupt-current', { body: corruption === 'current-pin' ?
      { field: 'pin', value: 'f'.repeat(40) } : { field: 'operation', value: 'f'.repeat(64) } })).status, 200);
    const before = await rt.inspect();
    await rt.restart();
    const rejectedRefresh = await rt.request('/refresh', { body: intent, credential: writer('FRESH') });
    assert.equal(rejectedRefresh.status, 503, corruption);
    const after = await rt.inspect();
    assert.deepEqual(after.records, before.records, corruption);
    assert.equal(after.mock.refreshCalls, 0, corruption);
    assert.equal(after.mock.catalogMutations, 1, corruption);
    assert.equal(after.mock.acceptCalls, 0, corruption);
  }
});

test('native refresh CAS loser preserves an intervening catalog winner and existing local pointer', { timeout: 30000 }, async t => {
  const { rt, intent, initial } = await prepareRefresh(t, { newHead: true });
  const winner = 'e'.repeat(40);
  assert.equal((await rt.request('/control', { body: { catalogHead: winner } })).status, 200);
  await rejected(await rt.request('/refresh', { body: intent, credential: writer('FRESH') }), /expected-old conflict/);
  const after = await rt.inspect();
  assert.equal(after.mock.catalogHead, winner); assert.equal(after.mock.catalogMutations, 1);
  assert.equal(after.records['current:toy'].pin, initial.pin);
  assert.equal(after.records['pending:toy'].operation, await operationId(intent));
  assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
});

test('native refresh withholds publication when current closure authority is revoked after proof', { timeout: 30000 }, async t => {
  const { rt, intent, initial } = await prepareRefresh(t);
  assert.equal((await rt.request('/control', { body: { denyAfterRefreshProof: true } })).status, 200);
  await rejected(await rt.request('/refresh', { body: intent, credential: writer('FRESH') }), /authority denied/);
  await rt.restart();
  await rejected(await rt.request('/current?receive-discovery=1', { credential: writer('FRESH') }), /authority denied/);
  const after = await rt.inspect();
  assert.equal(after.records['current:toy'].pin, initial.pin);
  assert.equal(after.mock.refreshCalls, 1); assert.equal(after.mock.catalogMutations, 1);
  assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
});


test('native refresh rejects mismatched prior catalog identities and stale observed generations', { timeout: 30000 }, async t => {
  const { rt, intent } = await prepareRefresh(t);
  const before = await rt.inspect();
  for (const mutate of [value => value.expected_catalog = 'a'.repeat(40), value => value.expected_native = state('a'),
    value => value.old_git = 'a'.repeat(40), value => value.expected_generation = 7, value => value.expected_generation = 9]) {
    const altered = structuredClone(intent); mutate(altered);
    await rejected(await rt.request('/refresh', { body: altered, credential: writer('FRESH') }), /fence mismatch|changed/);
    const after = await rt.inspect();
    assert.deepEqual(after.records, before.records);
    assert.equal(after.mock.refreshCalls, 0); assert.equal(after.mock.catalogMutations, 1);
    assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
  }
});

test('real private R2 native-refresh staging survives cold restart without submitting a Git command', { timeout: 30000 }, async t => {
  const { f, rt, intent } = await prepareRefresh(t, { newHead: true, useStaging: true });
  const refreshFixture = { ...f, intent }, operation = await operationId(intent);
  const proof = await stageNative(rt, refreshFixture, operation, writer('FRESH'));
  await assertStagingContainsNoSession(rt, refreshFixture, operation, proof);
  await rt.restart();
  const refreshed = await rt.request('/refresh', { body: intent, credential: writer('FRESH') });
  assert.equal(refreshed.status, 200, JSON.stringify(refreshed.body));
  assert.equal(refreshed.body.receipt.kind, 'native-refresh');
  const after = await rt.inspect();
  assert.equal(after.mock.bytes, null); assert.equal(after.mock.submitCalls, 0);
  assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
  assert.equal(after.mock.refreshCalls, 1); assert.equal(after.mock.catalogMutations, 2);
  assert.equal(after.mock.artifactReads, 0, 'source bytes must come from the actual R2 staging adapter');
  assert.doesNotMatch(canonical(after.records), /PUBLIC_MOCK_|authorization|Biscuit|Bearer/);
});


async function replaceNativeSource(rt, intent, missingOrCorrupt) {
  const oldArtifact = intent.history[0].artifacts[0];
  const oldKey = `native/source/${oldArtifact.sha256}`;
  const bucket = await rt.bucket();
  if (missingOrCorrupt === 'missing') await bucket.delete(oldKey);
  else await bucket.put(oldKey, encoder.encode('corrupted retired native source'));
  const bytes = encoder.encode('distinct fresh complete native closure for refresh');
  const artifact = { sha256: await digest(bytes), size: bytes.length, kind: 'pack' };
  intent.history = [{ state: intent.native_state, parents: [], artifacts: [artifact] }];
  assert.equal((await rt.request('/control', { body: { artifact, bytes: Array.from(bytes) } })).status, 200);
  return { oldKey, newKey: `native/source/${artifact.sha256}`, bytes };
}

test('native refresh retires missing or corrupt obsolete R2 source only after fresh closure verification', { timeout: 30000 }, async t => {
  for (const broken of ['missing', 'corrupt']) {
    const { rt, intent } = await prepareRefresh(t, { newHead: 'unrelated' });
    const { oldKey, newKey, bytes } = await replaceNativeSource(rt, intent, broken);
    await rt.restart();
    const refreshed = await rt.request('/refresh', { body: intent, credential: writer('FRESH') });
    assert.equal(refreshed.status, 200, JSON.stringify(refreshed.body));
    const current = await rt.request('/current', { credential: reader });
    assert.equal(current.status, 200); assert.equal(current.body.receipt.kind, 'native-refresh');
    const bucket = await rt.bucket();
    assert.deepEqual(new Uint8Array(await (await bucket.get(newKey)).arrayBuffer()), bytes);
    const old = await bucket.get(oldKey);
    if (broken === 'missing') assert.equal(old, null);
    else assert.equal(await old.text(), 'corrupted retired native source');
    const after = await rt.inspect();
    assert.equal(after.mock.refreshCalls, 1); assert.equal(after.mock.catalogMutations, 2);
    assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
  }
});

test('missing or corrupt obsolete R2 source never bypasses denied current refresh authority', { timeout: 30000 }, async t => {
  for (const broken of ['missing', 'corrupt']) {
    const { rt, intent } = await prepareRefresh(t, { newHead: 'unrelated' });
    const { newKey } = await replaceNativeSource(rt, intent, broken);
    assert.equal((await rt.request('/control', { body: { denied: true } })).status, 200);
    const before = await rt.inspect();
    await rt.restart();
    await rejected(await rt.request('/refresh', { body: intent, credential: writer('FRESH') }), /authority denied/);
    const after = await rt.inspect();
    assert.deepEqual(after.records, before.records);
    assert.equal(after.mock.refreshCalls, 0); assert.equal(after.mock.catalogMutations, 1);
    assert.equal(after.mock.acceptCalls, 0); assert.equal(after.mock.nativeMutations, 0);
    assert.equal(await (await rt.bucket()).get(newKey), null);
  }
});
