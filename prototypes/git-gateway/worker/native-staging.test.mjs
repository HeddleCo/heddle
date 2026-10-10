// Unit checks for input ownership; actual R2/process-loss coverage is in the workerd suite.
import test from 'node:test';
import assert from 'node:assert/strict';
import { R2NativeStaging } from './native-staging.mjs';
import { digest } from './publication.mjs';
const encode = new TextEncoder();
test('staging consumes native proof and source before any asynchronous authority check', async () => {
  const source = encode.encode('native fixture bytes'), proof = encode.encode('proof without any grant');
  const pristineSource = source.slice(), pristineProof = proof.slice(), sha256 = await digest(source);
  const state = `hs-${'b'.repeat(52)}`;
  const intent = { schema: 2, scope: { tenant_spool_id: '11111111-1111-4111-8111-111111111111',
    spool_id: '22222222-2222-4222-8222-222222222222', repository: 'toy', repo_path: 'org/toy',
    thread_id: '3'.repeat(64), thread: 'main', disclosure_audience: 'public' }, actor: 'fixture:actor',
    gateway_signer: '4'.repeat(64), billing_owner: 'fixture:billing', expected_catalog: null, expected_native: null,
    expected_generation: null, old_git: null, new_git: '5'.repeat(40), native_state: state, authority_generation: '6'.repeat(64),
    history: [{ state, parents: [], artifacts: [{ sha256, size: source.length, kind: 'pack' }] }] };
  const saved = new Map();
  const staging = new R2NativeStaging({ bucket: {
    async get(key) { const v = saved.get(key); return v ? { size: v.length, arrayBuffer: async () => v.slice().buffer } : null; },
    async put(key, value) { if (!saved.has(key)) saved.set(key, value.slice()); },
  }, submit: async () => { throw new Error('not a push'); }, resolveIntent: async () => intent,
  authorize: async () => { assert.equal(source.byteLength, 0); assert.equal(proof.byteLength, 0);
    assert.throws(() => source.fill(0), TypeError); assert.throws(() => proof.fill(1), TypeError); return true; },
  validatePlan: async (_, p, sources) => { assert.deepEqual(p, pristineProof); assert.deepEqual(sources[0].bytes, pristineSource); return true; } });
  await staging.stage(intent, proof, [{ sha256, bytes: source }], 'not-persisted-test-session');
  assert.deepEqual([...saved.entries()].find(([key]) => key.endsWith('/proof'))[1], pristineProof);
  assert.deepEqual([...saved.entries()].find(([key]) => key.endsWith(`/${sha256}`))[1], pristineSource);
  assert.equal([...saved.values()].some(v => new TextDecoder().decode(v).includes('not-persisted-test-session')), false);
  const writes = saved.size;
  const subProof = new Uint8Array(new ArrayBuffer(pristineProof.length + 1), 1);
  const sharedProof = new Uint8Array(new SharedArrayBuffer(pristineProof.length));
  for (const invalid of [subProof, sharedProof]) {
    await assert.rejects(staging.stage(intent, invalid, [{ sha256, bytes: pristineSource.slice() }], 'fixture'), /owned proof/);
  }
  const subSource = new Uint8Array(new ArrayBuffer(pristineSource.length + 1), 1);
  await assert.rejects(staging.stage(intent, pristineProof.slice(), [{ sha256, bytes: subSource }], 'fixture'), /owned exact artifact/);
  assert.equal(saved.size, writes);
  const deniedSource = pristineSource.slice(), deniedProof = pristineProof.slice();
  staging.authorize = async () => false;
  await assert.rejects(staging.stage(intent, deniedProof, [{ sha256, bytes: deniedSource }], 'fixture'), /denied/);
  assert.equal(deniedSource.byteLength, 0); assert.equal(deniedProof.byteLength, 0);
  assert.equal(saved.size, writes);
});
