// SPDX-License-Identifier: Apache-2.0
// Real workerd R2 and SQLite Durable Objects. Native authority/receipt inspection
// and remote Git catalog are explicit controlled adapters, not live integrations.
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { canonical, digest, operationId } from '../publication.mjs';
import { hostedGateway } from '../hosted-gateway.mjs';
const root = dirname(fileURLToPath(import.meta.url)), encoder = new TextEncoder();
const state = c => 'hs-' + c.repeat(52), writer = 'PUBLIC_MOCK_WRITER_SESSION_FRESH';
const fixtureSource = String.raw`
import { PublicationRecoveryTestObject } from './fixtures/publication-workerd.mjs';
import { canonical, digest } from '../publication.mjs';
const check = (value, reason) => { if (!value) throw new Error(reason); };
export class ReconciliationSecurityObject extends PublicationRecoveryTestObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.coordinator.native.reconcile = (...args) => this.inspectReconciliation(...args);
    this.coordinator.storage = { get: (...args) => ctx.storage.get(...args), transaction: async action => {
      let afterCommit = false;
      const result = await ctx.storage.transaction(async tx => {
        const pending = await tx.get('pending:toy'), key = pending && 'superseded:' + pending.operation;
        const before = key && await tx.get(key), result = await action(tx);
        if (key && !before && await tx.get(key)) {
          const control = await tx.get('fixture:reconcile') || {};
          if (control.transactionFault === 'before-commit') throw new Error('Injected reconciliation before commit');
          if (control.transactionFault === 'after-commit') {
            control.transactionFault = null; await tx.put('fixture:reconcile', control); afterCommit = true;
          }
        }
        return result;
      });
      if (afterCommit) throw new Error('Injected reconciliation after commit');
      return result;
    } };
  }
  async inspectReconciliation(intent, operation, observed, journalReceipt, credential) {
    return this.mockUpdate(async (mock, tx) => {
      const control = await tx.get('fixture:reconcile') || {}; control.calls = (control.calls || 0) + 1;
      check(/^PUBLIC_MOCK_WRITER_SESSION_[A-Z]+$/.test(credential || '') && !mock.denied,
        'Controlled original writer/current closure authority denied');
      check(control.calls !== control.denyOnCall, 'Controlled final closure authority denied');
      check(control.calls !== control.throwOnCall, 'Injected immutable receipt inspection failure');
      const unresolved = observed.outcome === 'unresolved-superseded';
      // Explicit unresolved mode does not infer non-acceptance or fall back from receipt failure.
      const receipt = unresolved ? null : structuredClone(mock.nativeReceipt);
      if (!unresolved) {
        check(receipt && receipt.operation === operation, 'Controlled immutable receipt absent');
        check(!journalReceipt || canonical(journalReceipt) === canonical(receipt), 'Controlled journal receipt differs');
      }
      const pendingPin = unresolved ? null : (await digest(new TextEncoder().encode(canonical({
        schema: 2, operation, intent, native_receipt: receipt })))).slice(0, 40);
      const proof = { kind: unresolved ? 'unresolved-superseded' : 'accepted-superseded', operation, receipt,
        catalog_pin: mock.catalogHead, pending_catalog_pin: pendingPin, current_native: mock.nativeState,
        current_generation: mock.nativeGeneration, actor: intent.actor, scope: structuredClone(intent.scope), ...control.proofPatch };
      if (control.calls === 1 && control.advanceAfterFirst) {
        mock.nativeState = control.advanceAfterFirst.state || mock.nativeState;
        mock.nativeGeneration += control.advanceAfterFirst.generation || 0;
      }
      if (control.calls === 2 && control.replacePending) {
        const pending = await tx.get('pending:toy'); pending.operation = 'd'.repeat(64); await tx.put('pending:toy', pending);
      }
      if (control.calls === 2 && control.replaceCurrent) {
        const current = await tx.get('current:toy'); current.pin = 'e'.repeat(40); await tx.put('current:toy', current);
      }
      await tx.put('fixture:reconcile', control); return proof;
    });
  }
  async fetch(request) {
    const url = new URL(request.url), credential = request.headers.get('authorization');
    try {
      if (request.method === 'POST' && url.pathname === '/configure-pending') {
        const { intent, fault } = await request.json();
        await this.mockUpdate(mock => { mock.intent = intent; mock.fault = fault; }); return Response.json({ configured: true });
      }
      if (request.method === 'POST' && url.pathname === '/reconcile-control') {
        const patch = await request.json();
        await this.storage.transaction(async tx => {
          await tx.put('fixture:reconcile', { ...await tx.get('fixture:reconcile'), ...patch });
        }); return Response.json({ changed: true });
      }
      if (request.method === 'POST' && url.pathname === '/reconcile')
        return Response.json(await this.coordinator.reconcile('toy', await request.json(), credential));
      if (request.method === 'POST' && url.pathname === '/audit-corrupt') {
        const { operation, field, value } = await request.json();
        check(['schema', 'operation', 'receipt', 'acceptance', 'previous_current', 'extra'].includes(field), 'Unknown audit corruption');
        await this.storage.transaction(async tx => {
          const audit = await tx.get('superseded:' + operation); check(audit, 'Audit absent');
          audit[field] = value; await tx.put('superseded:' + operation, audit);
        }); return Response.json({ changed: true });
      }
      return super.fetch(request);
    } catch (error) { return Response.json({ error: error.message }, { status: 503 }); }
  }
}
export default { fetch(request, env) { return env.PUBLICATION.get(env.PUBLICATION.idFromName('toy')).fetch(request); } };
`;
let bundle;
async function runtime(t) {
  bundle ??= build({ stdin: { contents: fixtureSource, resolveDir: root, sourcefile: 'reconciliation-security-fixture.mjs', loader: 'js' },
    bundle: true, format: 'esm', platform: 'browser', target: 'es2022', write: false, logLevel: 'silent' });
  const dir = await mkdtemp(join(tmpdir(), 'heddle-reconciliation-security-'));
  const options = convertV4MiniflareOptions({ name: 'heddle-reconciliation-security', script: (await bundle).outputFiles[0].text,
    modules: true, compatibilityDate: '2026-10-01', host: '127.0.0.1', port: 0,
    durableObjects: { PUBLICATION: { className: 'ReconciliationSecurityObject', useSQLite: true } },
    r2Buckets: { SOURCE_BYTES: 'heddle-reconciliation-security' }, resourcePersistencePath: join(dir, 'persistent'),
    resourceTmpPath: join(dir, 'temporary'), cf: false, telemetry: { enabled: false },
    outboundService: () => new Response(null, { status: 503 }) });
  let mf = new Miniflare(options); await mf.ready;
  t.after(async () => { await mf.dispose(); await rm(dir, { recursive: true, force: true }); });
  return {
    async request(path, body, credential = writer) {
      const response = await mf.dispatchFetch('http://localhost' + path, { method: body === undefined ? 'GET' : 'POST',
        headers: { authorization: credential, 'content-type': 'application/json' }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
      return { status: response.status, body: await response.json(), headers: Object.fromEntries(response.headers) };
    },
    async inspect() { return (await this.request('/inspect')).body; },
    async restart() { await mf.dispose(); mf = new Miniflare(options); await mf.ready; },
  };
}
async function prepare(t, fault = 'after-native-commit') {
  const bytes = encoder.encode('synthetic source for actual workerd reconciliation journal tests');
  const artifact = { sha256: await digest(bytes), size: bytes.length, kind: 'pack' };
  const initial = { schema: 2,
    scope: { tenant_spool_id: '11111111-1111-4111-8111-111111111111', spool_id: '22222222-2222-4222-8222-222222222222',
      repository: 'toy', repo_path: 'org/toy', thread_id: '3'.repeat(64), thread: 'main', disclosure_audience: 'public' },
    actor: 'user:verified', gateway_signer: '4'.repeat(64), billing_owner: 'account:owner', authority_generation: '8'.repeat(64),
    expected_catalog: null, expected_native: null, expected_generation: null, old_git: null,
    native_state: state('a'), new_git: '6'.repeat(40), history: [{ state: state('a'), parents: [], artifacts: [artifact] }] };
  const rt = await runtime(t);
  assert.equal((await rt.request('/setup', { intent: initial, artifact, bytes: Array.from(bytes) })).status, 200);
  const bootstrap = await rt.request('/bootstrap', initial); assert.equal(bootstrap.status, 200, JSON.stringify(bootstrap.body));
  const intent = { ...initial, schema: 1, expected_catalog: bootstrap.body.pin, expected_native: state('a'),
    expected_generation: 7, old_git: initial.new_git, native_state: state('b'), new_git: '7'.repeat(40),
    history: [...initial.history, { state: state('b'), parents: [state('a')], artifacts: [artifact] }] };
  await rt.request('/configure-pending', { intent, fault });
  const failed = await rt.request('/publish', intent); assert.equal(failed.status, 503);
  const operation = await operationId(intent);
  await rt.request('/control', { nativeState: state('c'), nativeGeneration: 9 });
  const before = await rt.inspect(), observed = { expected_operation: operation, expected_native: state('c'),
    expected_generation: 9, expected_catalog: before.mock.catalogHead };
  return { rt, intent, artifact, operation, observed, before, bootstrap: bootstrap.body };
}
async function rejected(result, pattern) {
  assert.equal(result.status, 503, JSON.stringify(result.body)); if (pattern) assert.match(result.body.error, pattern);
}
function unchangedNative(before, after) {
  for (const field of ['nativeMutations', 'acceptCalls', 'submitCalls', 'bootstrapCalls', 'refreshCalls', 'catalogMutations', 'catalogCalls', 'artifactReads'])
    assert.equal(after.mock[field], before.mock[field], 'Reconciliation must not change ' + field);
}
for (const fault of ['after-native-commit', 'before-catalog-publish', 'after-catalog-commit']) {
  test('actual reconciliation archives ' + fault + ' without accepting or publishing again', { timeout: 30000 }, async t => {
    const f = await prepare(t, fault); await f.rt.restart();
    const result = await f.rt.request('/reconcile', f.observed); assert.equal(result.status, 200, JSON.stringify(result.body));
    assert.equal(result.body.kind, 'accepted-superseded'); assert.equal(result.body.acceptance, 'accepted');
    const after = await f.rt.inspect(); unchangedNative(f.before, after);
    assert.equal(after.records['pending:toy'], undefined); assert.deepEqual(after.records['superseded:' + f.operation], result.body);
    assert.equal(after.records['current:toy'].pin, f.observed.expected_catalog); assert.equal(after.records['fixture:reconcile'].calls, 2);
    await rejected(await f.rt.request('/publish', f.intent), /superseded|ACK/i);
    await rejected(await f.rt.request('/current'), /head changed/i);
    const replay = await f.rt.request('/reconcile', f.observed); assert.equal(replay.status, 200, JSON.stringify(replay.body));
    assert.deepEqual(replay.body, result.body); assert.doesNotMatch(canonical(result.body), /PUBLIC_MOCK_|Bearer|unpack ok|ok refs\/heads/);
  });
}
test('only explicit fresh native refresh restores a readable view after reconciliation', { timeout: 30000 }, async t => {
  const f = await prepare(t); await rejected(await f.rt.request('/current?receive-discovery=1'), /head changed/i);
  const reconciled = await f.rt.request('/reconcile', f.observed); assert.equal(reconciled.status, 200);
  const refresh = { ...f.intent, schema: 3, expected_catalog: f.bootstrap.pin, expected_native: f.bootstrap.intent.native_state,
    expected_generation: 9, old_git: f.bootstrap.intent.new_git, native_state: state('c'), new_git: '9'.repeat(40),
    history: [{ state: state('c'), parents: [], artifacts: [f.artifact] }] };
  const result = await f.rt.request('/refresh', refresh); assert.equal(result.status, 200, JSON.stringify(result.body));
  const after = await f.rt.inspect(); assert.equal(after.mock.refreshCalls, 1); assert.equal(after.mock.acceptCalls, f.before.mock.acceptCalls);
  assert.deepEqual(after.records['superseded:' + f.operation], reconciled.body); assert.equal((await f.rt.request('/current')).status, 200);
});
test('conflicting remote winners and corrupt prior or pending catalog metadata fail closed', { timeout: 30000 }, async t => {
  for (const fault of ['after-native-commit', 'after-catalog-commit']) {
    const f = await prepare(t, fault); await f.rt.request('/control', { corruptCatalog: true });
    await rejected(await f.rt.request('/reconcile', f.observed), /catalog differs/i);
    assert.deepEqual((await f.rt.inspect()).records['pending:toy'], f.before.records['pending:toy']);
  }
  const f = await prepare(t); await f.rt.request('/control', { catalogHead: 'f'.repeat(40) });
  await rejected(await f.rt.request('/reconcile', { ...f.observed, expected_catalog: 'f'.repeat(40) }), /catalog differs/i);
  assert.equal((await f.rt.inspect()).records['superseded:' + f.operation], undefined);
});
test('writer identity, actor/scope, accepted receipt and current observed fences cannot be forged', { timeout: 30000 }, async t => {
  const f = await prepare(t);
  await rejected(await f.rt.request('/reconcile', f.observed, 'PUBLIC_MOCK_READER_SESSION_FRESH'), /writer|authority/);
  for (const proofPatch of [{ actor: 'user:other' }, { scope: { ...f.intent.scope, repo_path: 'org/other' } },
    { current_native: state('d') }, { current_generation: 10 }, { pending_catalog_pin: f.intent.expected_catalog },
    { receipt: { ...f.before.mock.nativeReceipt, generation: 9 } },
    { receipt: { ...f.before.mock.nativeReceipt, history_sha256: 'a'.repeat(64) } },
    { receipt: { ...f.before.mock.nativeReceipt, actor: 'user:other' } }]) {
    await f.rt.request('/reconcile-control', { calls: 0, proofPatch }); await rejected(await f.rt.request('/reconcile', f.observed));
    const after = await f.rt.inspect(); assert.deepEqual(after.records['pending:toy'], f.before.records['pending:toy']);
    assert.equal(after.records['superseded:' + f.operation], undefined); unchangedNative(f.before, after);
  }
});
test('current closure denial and native head or generation races preserve pending', { timeout: 30000 }, async t => {
  for (const control of [{ denyOnCall: 2 }, { advanceAfterFirst: { state: state('d') } }, { advanceAfterFirst: { generation: 1 } }]) {
    const f = await prepare(t); await f.rt.request('/reconcile-control', control); await rejected(await f.rt.request('/reconcile', f.observed));
    const after = await f.rt.inspect(); assert.deepEqual(after.records['pending:toy'], f.before.records['pending:toy']);
    assert.equal(after.records['superseded:' + f.operation], undefined);
  }
});
test('same-head metadata advancement still requires a strictly newer generation and explicit reconciliation', { timeout: 30000 }, async t => {
  const f = await prepare(t);
  await f.rt.request('/control', { nativeState: f.intent.native_state, nativeGeneration: 8 });
  const observed = { ...f.observed, expected_native: f.intent.native_state, expected_generation: 8 };
  await rejected(await f.rt.request('/reconcile', observed), /generation fence/i);
  assert.equal((await f.rt.inspect()).records['pending:toy'].operation, f.operation);
  await f.rt.request('/control', { nativeGeneration: 9 });
  const result = await f.rt.request('/reconcile', { ...observed, expected_generation: 9 });
  assert.equal(result.status, 200, JSON.stringify(result.body)); assert.equal(result.body.acceptance, 'accepted');
  assert.equal(result.body.inspected.current_native, result.body.receipt.native_state);
  unchangedNative(f.before, await f.rt.inspect());
});
test('malformed or mismatched operator observations cannot displace a pending operation', { timeout: 30000 }, async t => {
  const f = await prepare(t);
  for (const observed of [{ ...f.observed, extra: true }, { ...f.observed, expected_generation: -1 },
    { ...f.observed, expected_generation: Number.MAX_SAFE_INTEGER + 1 }, { ...f.observed, expected_operation: 'd'.repeat(64) },
    { ...f.observed, expected_native: state('d') }, { ...f.observed, expected_catalog: 'f'.repeat(40) }]) {
    await rejected(await f.rt.request('/reconcile', observed));
    const after = await f.rt.inspect(); assert.deepEqual(after.records['pending:toy'], f.before.records['pending:toy']);
    assert.equal(after.records['superseded:' + f.operation], undefined);
  }
});
test('atomic reconciliation cannot overwrite concurrent pending or current pointers', { timeout: 30000 }, async t => {
  for (const field of ['replacePending', 'replaceCurrent']) {
    const f = await prepare(t); await f.rt.request('/reconcile-control', { [field]: true });
    await rejected(await f.rt.request('/reconcile', f.observed), /journal changed/i);
    const after = await f.rt.inspect(); assert.equal(after.records['superseded:' + f.operation], undefined);
    if (field === 'replacePending') assert.equal(after.records['pending:toy'].operation, 'd'.repeat(64));
    else assert.equal(after.records['current:toy'].pin, 'e'.repeat(40));
  }
});
for (const transactionFault of ['before-commit', 'after-commit']) {
  test('cold restart recovers reconciliation ' + transactionFault + ' with an immutable audit', { timeout: 30000 }, async t => {
    const f = await prepare(t); await f.rt.request('/reconcile-control', { transactionFault });
    await rejected(await f.rt.request('/reconcile', f.observed), /Injected reconciliation/);
    const failed = await f.rt.inspect(); assert.equal(Boolean(failed.records['pending:toy']), transactionFault === 'before-commit');
    assert.equal(Boolean(failed.records['superseded:' + f.operation]), transactionFault === 'after-commit');
    await f.rt.restart(); await f.rt.request('/reconcile-control', { transactionFault: null });
    const repaired = await f.rt.request('/reconcile', f.observed); assert.equal(repaired.status, 200, JSON.stringify(repaired.body));
    assert.deepEqual((await f.rt.request('/reconcile', f.observed)).body, repaired.body); unchangedNative(f.before, await f.rt.inspect());
  });
}
test('corrupt terminal fields cannot become a successful immutable retry', { timeout: 30000 }, async t => {
  const f = await prepare(t), result = await f.rt.request('/reconcile', f.observed); assert.equal(result.status, 200);
  for (const [field, value] of [['schema', 99], ['operation', 'f'.repeat(64)], ['receipt', { ...result.body.receipt, native_state: state('d') }],
    ['acceptance', 'unknown'], ['previous_current', { ...result.body.previous_current, pin: 'e'.repeat(40) }], ['extra', true]]) {
    await f.rt.request('/audit-corrupt', { operation: f.operation, field, value }); await rejected(await f.rt.request('/reconcile', f.observed));
    if (field !== 'extra') await f.rt.request('/audit-corrupt', { operation: f.operation, field, value: result.body[field] });
  }
});
test('terminal retries still perform the final fresh authority check without changing the archived audit', { timeout: 30000 }, async t => {
  const f = await prepare(t), result = await f.rt.request('/reconcile', f.observed); assert.equal(result.status, 200);
  await f.rt.request('/reconcile-control', { calls: 0, denyOnCall: 2 });
  await rejected(await f.rt.request('/reconcile', f.observed), /final closure authority denied/);
  const after = await f.rt.inspect(); assert.deepEqual(after.records['superseded:' + f.operation], result.body);
  assert.equal(after.records['pending:toy'], undefined); unchangedNative(f.before, after);
});
test('explicit unresolved outcome preserves uncertainty without receipt inference or fallback', { timeout: 30000 }, async t => {
  const f = await prepare(t); await f.rt.request('/reconcile-control', { throwOnCall: 1 });
  await rejected(await f.rt.request('/reconcile', f.observed), /receipt inspection failure/);
  assert.equal((await f.rt.inspect()).records['pending:toy'].operation, f.operation);
  await f.rt.request('/reconcile-control', { throwOnCall: null });
  const observed = { ...f.observed, outcome: 'unresolved-superseded' };
  const result = await f.rt.request('/reconcile', observed); assert.equal(result.status, 200, JSON.stringify(result.body));
  assert.equal(result.body.kind, 'unresolved-superseded'); assert.equal(result.body.acceptance, 'unknown');
  assert.equal(result.body.receipt, null); assert.equal(result.body.inspected.pending_catalog_pin, null);
  unchangedNative(f.before, await f.rt.inspect()); await rejected(await f.rt.request('/publish', f.intent), /superseded|ACK/i);
  await f.rt.restart(); assert.deepEqual((await f.rt.request('/reconcile', observed)).body, result.body);
});
test('unresolved mode cannot relabel a journaled acceptance or clear an unexplained remote catalog', { timeout: 30000 }, async t => {
  const accepted = await prepare(t, 'before-catalog-publish');
  await rejected(await accepted.rt.request('/reconcile', { ...accepted.observed, outcome: 'unresolved-superseded' }));
  assert.equal((await accepted.rt.inspect()).records['pending:toy'].stage, 'accepted');
  const unknown = await prepare(t); await unknown.rt.request('/control', { catalogHead: 'f'.repeat(40) });
  await rejected(await unknown.rt.request('/reconcile', { ...unknown.observed, expected_catalog: 'f'.repeat(40), outcome: 'unresolved-superseded' }));
  assert.equal((await unknown.rt.inspect()).records['pending:toy'].stage, 'prepared');
});
test('native-reconcile HTTP requires write authority and returns audit JSON rather than a Git ACK', async () => {
  let calls = 0, action;
  const forbidden = () => { throw new Error('Unrelated Git mutation must not run'); };
  const gateway = hostedGateway({ coordinator: { publish: forbidden, current: forbidden, reconcile: async () => {
    calls++; return { kind: 'accepted-superseded', acceptance: 'accepted', operation: 'a'.repeat(64),
      inspected: { catalog_pin: 'b'.repeat(40), current_native: state('c') } };
  } }, staging: { stage: forbidden }, native: { prepare: forbidden, project: forbidden },
  authorizeRequest: async (_, value, credential) => { action = value; return credential === writer; } });
  const request = credential => new Request('http://fixture/repositories/toy.git/native-reconcile', { method: 'POST',
    headers: { authorization: credential, 'content-type': 'application/json' }, body: '{}' });
  assert.equal((await gateway.fetch(request('reader'))).status, 403); assert.equal(calls, 0);
  const result = await gateway.fetch(request(writer)); assert.equal(result.status, 200); assert.equal(action, 'write');
  assert.match(result.headers.get('content-type'), /^application\/json/); assert.equal(result.headers.get('cache-control'), 'no-store');
  const body = await result.text(); assert.doesNotMatch(body, /unpack ok|ok refs\/heads/);
  assert.equal(JSON.parse(body).next, '/repositories/toy.git/native-refresh'); assert.equal(calls, 1);
});
