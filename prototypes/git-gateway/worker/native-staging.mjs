// SPDX-License-Identifier: Apache-2.0
// Real private R2 staging for process-loss recovery. Only native SourcePack/proof bytes are
// retained; request Git packs and transport/signer secrets are never accepted by this API.
import { canonical, digest, operationId, normalizeIntent, validateIntent, verifyNativeObject, MAX_NATIVE_BYTES } from './publication.mjs';
const encode = new TextEncoder();
const MAX_PROOF_BYTES = 17 * 1024 * 1024;
const requireValue = (ok, message) => { if (!ok) throw new Error(message); };
const bytes = async (object, maximum = MAX_NATIVE_BYTES) => {
  if (!object || !Number.isSafeInteger(object.size) || object.size < 1 || object.size > maximum) {
    void object?.body?.cancel().catch(() => {}); throw new Error('Native staging object unavailable');
  }
  const data = new Uint8Array(await object.arrayBuffer());
  requireValue(data.length === object.size, 'Native staging length changed'); return data;
};
async function immutable(bucket, key, data) {
  const hash = await digest(data);
  await bucket.put(key, data, { onlyIf: { etagDoesNotMatch: '*' }, sha256: hash,
    customMetadata: { storage_class: 'private-native-staging', content_sha256: hash } });
  const actual = await bucket.get(key);
  requireValue(actual, 'Native staging content unavailable');
  await verifyNativeObject(actual, { size: data.length, sha256: hash });
  return { key, sha256: hash, size: data.length };
}
async function read(bucket, descriptor) {
  requireValue(Number.isSafeInteger(descriptor.size) && descriptor.size > 0 && descriptor.size <= MAX_NATIVE_BYTES, 'Native staging descriptor limit');
  const data = await bytes(await bucket.get(descriptor.key), descriptor.size);
  requireValue(data.length === descriptor.size && await digest(data) === descriptor.sha256, 'Native staging content changed');
  return data;
}
export class R2NativeStaging {
  constructor({ bucket, submit, validatePlan, authorize, resolveIntent, verifyBootstrap, verifyRefresh, inspectReconciliation }) {
    requireValue(bucket?.put && bucket?.get && typeof submit === 'function' && typeof validatePlan === 'function' &&
      typeof authorize === 'function' && typeof resolveIntent === 'function', 'Native staging integrations required');
    Object.assign(this, { bucket, submit, validatePlan, authorize, resolveIntent, verifyBootstrap, verifyRefresh, inspectReconciliation });
  }
  async stage(intent, proof, artifacts, credential) {
    intent = normalizeIntent(intent);
    requireValue(proof instanceof Uint8Array && proof.length > 0 && proof.length <= MAX_PROOF_BYTES,
      'Native proof metadata limit');
    const declared = validateIntent(intent);
    requireValue(Array.isArray(artifacts) && artifacts.length === declared.length, 'Complete native staging inventory required');
    // Consume every caller-owned byte before the first await. Detachment prevents mutation
    // without doubling a 64 MiB source closure plus 17 MiB proof inside a 128 MiB Worker.
    // Consumption remains final even if later authorization or persistence fails.
    const owned = value => value instanceof Uint8Array && value.buffer instanceof ArrayBuffer &&
      value.byteOffset === 0 && value.byteLength === value.buffer.byteLength && value.buffer.resizable !== true;
    requireValue(owned(proof), 'Native staging requires an owned proof buffer');
    artifacts = declared.map(expected => {
      const value = artifacts.find(a => a.sha256 === expected.sha256);
      requireValue(owned(value?.bytes) && value.bytes.length === expected.size, 'Native staging requires owned exact artifact buffers');
      return { sha256: expected.sha256, bytes: value.bytes };
    });
    const transfer = [...new Set([proof.buffer, ...artifacts.map(a => a.bytes.buffer)])];
    ({ proof, artifacts } = structuredClone({ proof, artifacts }, { transfer }));
    const operation = await operationId(intent);
    requireValue(await this.authorize(intent, credential) === true, 'Native staging denied');
    // Rust validates token-free prost openings/originals and matches their native inventories
    // and accepted Git/native targets to the canonical intent. A boolean from the caller or
    // client headers cannot substitute for the configured native validator.
    const wanted = validateIntent(intent);
    requireValue(Array.isArray(artifacts) && artifacts.length === wanted.length, 'Complete native staging inventory required');
    const prefix = `gateway-staging/${intent.scope.tenant_spool_id}/${intent.scope.spool_id}/${operation}/`;
    const sources = [], checked = []; let total = 0;
    for (const expected of wanted) {
      const input = artifacts.find(a => a.sha256 === expected.sha256);
      requireValue(input?.bytes instanceof Uint8Array && input.bytes.length === expected.size &&
        await digest(input.bytes) === expected.sha256, 'Native staging artifact differs');
      total += input.bytes.length; requireValue(total <= MAX_NATIVE_BYTES, 'Combined native staging limit');
      checked.push({ sha256: expected.sha256, bytes: input.bytes });
    }
    requireValue(await this.validatePlan(intent, proof, checked, credential) === true, 'Native staging proof invalid or contains credentials');
    for (const input of checked) sources.push(await immutable(this.bucket, `${prefix}${input.sha256}`, input.bytes));
    const plan = await immutable(this.bucket, `${prefix}proof`, proof);
    requireValue(await this.authorize(intent, credential) === true, 'Native staging authority changed');
    // Manifest is written LAST. Incomplete uploads cannot be submitted after a process loss.
    const manifest = encode.encode(canonical({ schema: 1, operation, intent, proof: plan, artifacts: sources }));
    requireValue(manifest.length <= 96 * 1024, 'Native staging manifest limit');
    await immutable(this.bucket, `${prefix}manifest`, manifest);
    return operation;
  }
  async load(intent, operation, credential, { mode = 'write' } = {}) {
    intent = normalizeIntent(intent);
    requireValue(operation === await operationId(intent) && await this.authorize(intent, credential, { mode }) === true,
      'Native staging scope denied');
    const prefix = `gateway-staging/${intent.scope.tenant_spool_id}/${intent.scope.spool_id}/${operation}/`;
    const data = await bytes(await this.bucket.get(`${prefix}manifest`), 96 * 1024);
    requireValue(data.length <= 96 * 1024, 'Native staging manifest limit');
    const text = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(data), value = JSON.parse(text);
    requireValue(text === canonical(value) && value.schema === 1 && value.operation === operation &&
      canonical(value.intent) === canonical(intent) && Object.keys(value).sort().join(',') === 'artifacts,intent,operation,proof,schema',
      'Native staging manifest mismatch');
    const wanted = validateIntent(intent);
    requireValue(Array.isArray(value.artifacts) && value.artifacts.length === wanted.length &&
      value.proof?.key === `${prefix}proof` && value.proof.size <= MAX_PROOF_BYTES, 'Invalid staging descriptor');
    const artifacts = [];
    for (const expected of wanted) {
      const source = value.artifacts.find(a => a.sha256 === expected.sha256);
      requireValue(source?.key === `${prefix}${expected.sha256}` && source.size === expected.size, 'Native staging scope mismatch');
      artifacts.push({ sha256: expected.sha256, bytes: await read(this.bucket, source) });
    }
    const proof = await read(this.bucket, value.proof);
    requireValue(await this.validatePlan(intent, proof, artifacts, credential, { mode }) === true, 'Native staging proof changed');
    requireValue(await this.authorize(intent, credential, { mode }) === true, 'Native staging authority changed');
    return { proof, artifacts };
  }
  // Authority lookup must not recursively authorize itself. This exposes only a hash-checked,
  // bounded proof to the configured trusted native authority, never to a public caller. Missing
  // pre-stage proof is allowed; corrupt or cross-scope existing metadata always fails closed.
  async readProof(intent) {
    intent = normalizeIntent(intent); const operation = await operationId(intent);
    const prefix = `gateway-staging/${intent.scope.tenant_spool_id}/${intent.scope.spool_id}/${operation}/`;
    const object = await this.bucket.get(`${prefix}manifest`);
    if (!object) return null;
    const data = await bytes(object, 96 * 1024);
    requireValue(data.length <= 96 * 1024, 'Native staging manifest limit');
    const text = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(data), value = JSON.parse(text);
    requireValue(text === canonical(value) && value.schema === 1 && value.operation === operation &&
      canonical(value.intent) === canonical(intent) && Object.keys(value).sort().join(',') === 'artifacts,intent,operation,proof,schema' &&
      value.proof?.key === `${prefix}proof` && Number.isSafeInteger(value.proof.size) && value.proof.size > 0 &&
      value.proof.size <= MAX_PROOF_BYTES, 'Native staging proof scope mismatch');
    return read(this.bucket, value.proof);
  }
  async bootstrap(intent, operation, credential) {
    requireValue(intent.schema === 2 && typeof this.verifyBootstrap === 'function', 'Native bootstrap verifier required');
    const { proof, artifacts } = await this.load(intent, operation, credential);
    return this.verifyBootstrap(intent, proof, artifacts, credential);
  }
  async refresh(intent, operation, credential) {
    requireValue(intent.schema === 3 && typeof this.verifyRefresh === 'function', 'Native refresh verifier required');
    const { proof, artifacts } = await this.load(intent, operation, credential);
    return this.verifyRefresh(intent, proof, artifacts, credential);
  }
  async reconcile(intent, operation, observed, receipt, credential) {
    requireValue(intent.schema === 1 && operation === await operationId(intent) && typeof this.inspectReconciliation === 'function',
      'Genuine pending acceptance inspection required');
    const proof = await this.readProof(intent);
    requireValue(proof instanceof Uint8Array, 'Pending acceptance proof unavailable');
    // Ordinary authorize/load must retain their exact old generation fence. This separate
    // callback verifies old receipt audit AND fresh current closure without any native mutation.
    return this.inspectReconciliation(intent, operation, observed, receipt, proof, credential);
  }
  async accept(intent, operation, credential) {
    requireValue(intent.schema === 1, 'Bootstrap is not a Git acceptance command');
    const { proof, artifacts } = await this.load(intent, operation, credential);
    // The configured Rust sender injects only this fresh verified session, streams the exact
    // durable plan to genuine PublishContent, and validates the native atomic receipt.
    return this.submit(intent, proof, artifacts, credential);
  }
  async readArtifact(receipt, artifact, credential) {
    // Receipt-to-intent lookup belongs to the durable coordinator, never to caller metadata.
    requireValue(typeof this.resolveIntent === 'function', 'Native receipt scope resolver required');
    const intent = await this.resolveIntent(receipt);
    requireValue(receipt.operation === await operationId(intent) && await this.authorize(intent, credential) === true,
      'Accepted native source scope denied');
    const expected = validateIntent(intent).find(a => a.sha256 === artifact.sha256);
    requireValue(expected && expected.size === artifact.size && expected.kind === artifact.kind, 'Unknown accepted native artifact');
    const key = `gateway-staging/${intent.scope.tenant_spool_id}/${intent.scope.spool_id}/${receipt.operation}/${artifact.sha256}`;
    const result = await read(this.bucket, { key, sha256: artifact.sha256, size: artifact.size });
    requireValue(await this.authorize(intent, credential) === true, 'Native staging authority changed');
    return result;
  }
}
