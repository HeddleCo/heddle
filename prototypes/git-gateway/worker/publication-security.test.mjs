// SPDX-License-Identifier: Apache-2.0
// Independent fault-injection/security tests. This tests the actual orchestration;
// in-memory service adapters do not claim live Weft, R2, or Artifacts verification.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PublicationCoordinator, operationId, digest, canonical, validateIntent, MAX_NATIVE_BYTES } from './publication.mjs';

const encoder = new TextEncoder();
const copy = value => value === undefined ? undefined : structuredClone(value);
const state = character => 'hs-' + character.repeat(52);
const hash = value => digest(encoder.encode(canonical(value)));
async function fixture() {
  const blob = encoder.encode('receiver-owned exact full native history bytes');
  const artifact = { sha256: await digest(blob), size: blob.length, kind: 'pack' };
  const intent = { schema: 1,
    scope: { tenant_spool_id: '11111111-1111-4111-8111-111111111111', spool_id: '22222222-2222-4222-8222-222222222222',
      repository: 'toy', repo_path: 'org/toy', thread_id: '3'.repeat(64), thread: 'main', disclosure_audience: 'public' },
    actor: 'user:verified', gateway_signer: '4'.repeat(64), billing_owner: 'account:owner',
    expected_catalog: '5'.repeat(40), expected_native: state('a'), expected_generation: 7,
    old_git: '6'.repeat(40), new_git: '7'.repeat(40), native_state: state('b'), authority_generation: '8'.repeat(64),
    history: [{ state: state('a'), parents: [], artifacts: [artifact] }, { state: state('b'), parents: [state('a')], artifacts: [artifact] }],
  };
  const records = new Map(), objects = new Map(), manifests = new Map(), receipts = new Map();
  const events = [];
  const stateful = { denied: false, fault: null, mutateReceipt: null, mutations: 0, acceptCalls: 0, publishCalls: 0, catalogMutations: 0,
    afterStoreCommit: null, corruptObject: false, corruptCatalog: false,
    nativeState: intent.expected_native, nativeGeneration: intent.expected_generation,
    advanceAt: null };
  const hit = phase => {
    events.push(phase);
    if (stateful.advanceAt === phase) { stateful.advanceAt = null; stateful.nativeState = state('c'); stateful.nativeGeneration = 9; }
    if (stateful.fault === phase) { stateful.fault = null; throw new Error(`injected ${phase}`); }
  };
  const storage = {
    async get(key) { return copy(records.get(key)); },
    async transaction(action) {
      const snapshot = new Map([...records].map(([k, v]) => [k, copy(v)]));
      const tx = { async get(key) { return copy(snapshot.get(key)); }, async put(key, value) { snapshot.set(key, copy(value)); }, async delete(key) { snapshot.delete(key); } };
      const result = await action(tx);
      records.clear(); for (const [key, value] of snapshot) records.set(key, value);
      if (stateful.afterStoreCommit) await stateful.afterStoreCommit(records);
      return result;
    },
  };
  const native = {
    async accept(value, operation) {
      stateful.acceptCalls++; hit('before-native-accept');
      assert.ok(records.has('pending:toy'), 'journal must survive native commit uncertainty');
      let receipt = receipts.get(operation);
      if (!receipt) {
        stateful.mutations++;
        receipt = { schema: 1, operation, native_state: value.native_state, previous_native: value.expected_native,
          generation: 8, authority_generation: value.authority_generation, history_sha256: await hash(value.history),
          actor: value.actor, gateway_signer: value.gateway_signer, billing_owner: value.billing_owner };
        receipts.set(operation, copy(receipt));
        stateful.nativeState = receipt.native_state; stateful.nativeGeneration = receipt.generation;
      }
      hit('after-native-commit');
      if (stateful.mutateReceipt) stateful.mutateReceipt(receipt);
      return copy(receipt);
    },
    async readArtifact() { hit('native-artifact-read'); return blob.slice(); },
  };
  const bucket = {
    async get(key) {
      hit('r2-get'); const bytes = objects.get(key); if (!bytes) return null;
      return { size: bytes.length, async arrayBuffer() { const body = bytes.slice(); if (stateful.corruptObject) body[0] ^= 1; return body.buffer; } };
    },
    async put(key, bytes, options) {
      hit('before-r2-put');
      assert.equal(receipts.size, 1, 'source upload requires receiver acceptance');
      assert.deepEqual(options.onlyIf, { etagDoesNotMatch: '*' });
      assert.equal(await digest(bytes), options.sha256);
      if (!objects.has(key)) objects.set(key, bytes.slice());
      hit('after-r2-put');
    },
  };
  const catalog = {
    async publish({ expected, operation, manifest }) {
      stateful.publishCalls++; hit('before-catalog-publish');
      assert.equal(expected, intent.expected_catalog);
      assert.equal(objects.size, 1, 'catalog must not refer to absent native bytes');
      if (!manifests.has(operation)) { manifests.set(operation, manifest.slice()); stateful.catalogMutations++; }
      else assert.deepEqual(manifests.get(operation), manifest, 'ambiguous retry must request exact immutable manifest');
      hit('after-catalog-commit'); return '9'.repeat(40);
    },
    async resolve(_repository, pin) {
      hit('catalog-resolve'); assert.equal(pin, '9'.repeat(40));
      const bytes = [...manifests.values()][0].slice(); if (stateful.corruptCatalog) bytes[0] ^= 1; return bytes;
    },
  };
  const authorize = async value => {
    hit('authorize'); if (stateful.denied) throw new Error('current authority revoked');
    return { scope: copy(value.scope), actor: value.actor, gateway_signer: value.gateway_signer,
      billing_owner: value.billing_owner, authority_generation: value.authority_generation,
      native_state: stateful.nativeState, native_generation: stateful.nativeGeneration };
  };
  const coordinator = () => new PublicationCoordinator({ storage, native, bucket, catalog, authorize });
  return { intent, artifact, blob, records, objects, manifests, receipts, events, stateful, storage, native, bucket, catalog, authorize, coordinator };
}

test('native rejection keeps a durable pending command but cannot upload content, publish, or return success', async () => {
  const f = await fixture(); f.stateful.fault = 'before-native-accept';
  await assert.rejects(f.coordinator().publish(f.intent, 'session-1'), /before-native-accept/);
  assert.equal(f.stateful.mutations, 0); assert.equal(f.objects.size, 0); assert.equal(f.manifests.size, 0);
  assert.ok(f.records.has('pending:toy')); assert.equal(f.records.has('current:toy'), false);
  await assert.rejects(f.coordinator().current('toy', 'session-1'), /recovery/);
});

test('every committed-but-unpublished fault repairs with the same operation and exactly one native/catalog mutation', async () => {
  for (const phase of ['after-native-commit', 'native-artifact-read', 'after-r2-put', 'before-catalog-publish', 'after-catalog-commit', 'catalog-resolve']) {
    const f = await fixture(); f.stateful.fault = phase;
    await assert.rejects(f.coordinator().publish(f.intent, 'session-old'), new RegExp(phase));
    assert.equal(f.records.has('current:toy'), false, phase);
    assert.ok(f.records.has('pending:toy'), phase);
    await assert.rejects(f.coordinator().current('toy', 'session-new'), /recovery/);
    // Brand-new coordinator simulates restart; neither quarantine nor the old session is needed.
    const recovered = await f.coordinator().current('toy', 'session-new', { receiveDiscovery: true });
    assert.equal(recovered.stage, 'published', phase);
    assert.equal(recovered.operation, await operationId(f.intent));
    assert.equal(f.stateful.mutations, 1, phase); assert.equal(f.stateful.catalogMutations, 1, phase);
    assert.equal(f.records.has('pending:toy'), false, phase);
    const replay = await f.coordinator().publish(f.intent, 'session-third');
    assert.deepEqual(replay, recovered, phase);
    assert.equal(f.stateful.mutations, 1); assert.equal(f.stateful.catalogMutations, 1);
  }
});

test('mismatched native receipts cannot authorize R2 bytes or a Git catalog', async () => {
  for (const field of ['operation', 'native_state', 'previous_native', 'authority_generation', 'history_sha256', 'actor', 'gateway_signer', 'billing_owner', 'generation']) {
    const f = await fixture(); f.stateful.mutateReceipt = receipt => { receipt[field] = field === 'generation' ? 7 : 'substituted'; };
    await assert.rejects(f.coordinator().publish(f.intent, 'session'), /receipt mismatch/, field);
    assert.equal(f.objects.size, 0, field); assert.equal(f.manifests.size, 0, field);
    assert.equal(f.records.has('current:toy'), false, field);
  }
});

test('corrupt R2 content is not overwritten, published, or accepted as a recovery cache hit', async () => {
  const f = await fixture();
  f.objects.set(`native/source/${f.artifact.sha256}`, f.blob.slice()); f.stateful.corruptObject = true;
  await assert.rejects(f.coordinator().publish(f.intent, 'session'), /content mismatch/);
  assert.equal(f.manifests.size, 0); assert.equal(f.records.has('current:toy'), false);
  f.stateful.corruptObject = false; await f.coordinator().recover('toy', 'session');
  f.stateful.corruptObject = true;
  await assert.rejects(f.coordinator().current('toy', 'session'), /content mismatch/);
  await assert.rejects(f.coordinator().publish(f.intent, 'new-session'), /content mismatch/);
  assert.equal(f.stateful.mutations, 1);
});

test('catalog content must exactly match accepted native receipt before current pointer or success', async () => {
  const f = await fixture(); f.stateful.corruptCatalog = true;
  await assert.rejects(f.coordinator().publish(f.intent, 'session'), /differs from accepted source/);
  assert.equal(f.records.has('current:toy'), false); assert.ok(f.records.has('pending:toy'));
  f.stateful.corruptCatalog = false; await f.coordinator().recover('toy', 'session');
  f.stateful.corruptCatalog = true;
  await assert.rejects(f.coordinator().current('toy', 'session'), /differs from accepted source/);
});

test('revocation at final durable receipt transaction prevents ACK and later cached authorization', async () => {
  const f = await fixture();
  f.stateful.afterStoreCommit = async records => { if (records.has('current:toy')) f.stateful.denied = true; };
  await assert.rejects(f.coordinator().publish(f.intent, 'session'), /revoked/);
  assert.ok(f.records.has('current:toy'), 'publication may commit even when ACK is correctly withheld');
  assert.equal(f.stateful.mutations, 1); assert.equal(f.stateful.catalogMutations, 1);
  await assert.rejects(f.coordinator().current('toy', 'session'), /revoked/);
  await assert.rejects(f.coordinator().publish(f.intent, 'new-session'), /revoked/);
});

test('a different actor, gateway signer, billing owner, scope or command cannot displace a pending publication', async () => {
  for (const field of ['actor', 'gateway_signer', 'billing_owner', 'new_git', 'authority_generation', 'scope']) {
    const f = await fixture(); f.stateful.fault = 'after-native-commit';
    await assert.rejects(f.coordinator().publish(f.intent, 'session'));
    const other = copy(f.intent);
    if (field === 'scope') other.scope.repo_path = 'org/other';
    else other[field] = ['gateway_signer', 'authority_generation'].includes(field) ? 'a'.repeat(64) : field === 'new_git' ? 'b'.repeat(40) : 'other';
    assert.notEqual(await operationId(other), await operationId(f.intent), field);
    await assert.rejects(f.coordinator().publish(other, 'other-session'), /Another publication is pending/, field);
    assert.equal(f.stateful.mutations, 1, field); assert.equal(f.manifests.size, 0, field);
  }
});

test('complete old plus new history budgets count repeated artifact references and reject disconnected roots', async () => {
  const f = await fixture();
  const oversized = copy(f.intent);
  for (const revision of oversized.history) revision.artifacts[0].size = MAX_NATIVE_BYTES / 2 + 1;
  assert.throws(() => validateIntent(oversized), /Combined history byte limit/);
  const disconnected = copy(f.intent); disconnected.history[1].parents = [];
  assert.throws(() => validateIntent(disconnected), /Unrelated history/);
  const missingOld = copy(f.intent); missingOld.expected_native = state('c');
  assert.throws(() => validateIntent(missingOld), /Incomplete history fence/);
  const cyclic = copy(f.intent); cyclic.history[0].parents = [state('b')];
  assert.throws(() => validateIntent(cyclic), /Invalid exact history/);
});

test('caller mutation after publish begins cannot change the accepted semantic operation', async () => {
  const f = await fixture(); const original = copy(f.intent), operation = await operationId(original);
  const pending = f.coordinator().publish(f.intent, 'session');
  f.intent.actor = 'attacker'; f.intent.history[0].artifacts[0].sha256 = 'a'.repeat(64);
  const result = await pending;
  assert.equal(result.operation, operation); assert.deepEqual(result.intent, original);
});

test('concurrent native advance cannot become a stale successful Git publication or cached retry', async () => {
  for (const phase of ['native-artifact-read', 'after-r2-put', 'after-catalog-commit', 'catalog-resolve']) {
    const f = await fixture(); f.stateful.advanceAt = phase;
    await assert.rejects(f.coordinator().publish(f.intent, 'session'), /Native head changed/, phase);
    assert.equal(f.records.has('current:toy'), false, phase);
    assert.ok(f.records.has('pending:toy'), phase);
    await assert.rejects(f.coordinator().recover('toy', 'new-session'), /Native head changed/, phase);
    assert.equal(f.stateful.mutations, 1, phase);
  }
  const f = await fixture(); await f.coordinator().publish(f.intent, 'session');
  f.stateful.nativeState = state('c'); f.stateful.nativeGeneration = 9;
  await assert.rejects(f.coordinator().current('toy', 'reader'), /Native head changed/);
  await assert.rejects(f.coordinator().publish(f.intent, 'session'), /Native head changed/);
  assert.equal(f.stateful.mutations, 1);
});

test('equal native state with a different generation is not an accepted receipt replay', async () => {
  const f = await fixture(); await f.coordinator().publish(f.intent, 'session');
  f.stateful.nativeGeneration++;
  await assert.rejects(f.coordinator().current('toy', 'reader'), /Native head changed/);
  await assert.rejects(f.coordinator().publish(f.intent, 'session'), /Native head changed/);
});
