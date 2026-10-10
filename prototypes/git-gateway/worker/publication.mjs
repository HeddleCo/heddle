// SPDX-License-Identifier: Apache-2.0
// Durable hosted publication orchestration. No root, credential, signer or billing owner is minted here.
// storage is a per-repository Durable Object storage binding; native and catalog are authenticated
// service adapters. catalog writes use Git expected-old ref updates, never a fictitious Artifacts put.
const encoder = new TextEncoder();
const HEX = /^[0-9a-f]{64}$/;
const OID = /^[0-9a-f]{40}$/;
const STATE = /^hs-[0-9a-z]{52}$/;
const NAME = /^[a-z][a-z0-9-]{0,63}$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
export const MAX_NATIVE_BYTES = 64 * 1024 * 1024;
export const MAX_PUBLICATION_METADATA = 64 * 1024;
export function canonical(value) {
  const sort = v => Array.isArray(v) ? v.map(sort) : v && typeof v === 'object' ?
    Object.fromEntries(Object.keys(v).sort().map(k => [k, sort(v[k])])) : v;
  return JSON.stringify(sort(value)) + '\n';
}
export async function digest(bytes) {
  return Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)), b => b.toString(16).padStart(2, '0')).join('');
}
const hash = value => digest(encoder.encode(canonical(value)));
const equal = (a, b) => canonical(a) === canonical(b);
function keys(value, expected) {
  return value && typeof value === 'object' && !Array.isArray(value) &&
    Object.keys(value).sort().join(',') === expected.split(',').sort().join(',');
}
function requireValue(ok, message) { if (!ok) throw new Error(message); }
function label(value) { return typeof value === 'string' && /^[A-Za-z0-9][A-Za-z0-9:._/@-]{0,255}$/.test(value); }
export function validateIntent(value) {
  requireValue(keys(value, 'schema,scope,actor,gateway_signer,billing_owner,expected_catalog,expected_native,expected_generation,old_git,new_git,native_state,authority_generation,history'), 'Invalid publication intent');
  const s = value.scope;
  requireValue(keys(s, 'tenant_spool_id,spool_id,repository,repo_path,thread_id,thread,disclosure_audience') &&
    UUID.test(s.tenant_spool_id) && UUID.test(s.spool_id) && NAME.test(s.repository) &&
    typeof s.repo_path === 'string' && /^[a-z0-9-]+(?:\/[a-z0-9-]+){1,7}$/.test(s.repo_path) &&
    /^[0-9a-f]{64}$/.test(s.thread_id) && typeof s.thread === 'string' && s.thread.length <= 255 &&
    /^[a-z][a-z0-9-]{0,63}(?:\/[a-z][a-z0-9-]{0,63}){0,7}$/.test(s.thread) &&
    s.disclosure_audience === 'public', 'Invalid publication scope');
  const bootstrap = value.schema === 2, refresh = value.schema === 3;
  requireValue([1, 2, 3].includes(value.schema) && label(value.actor) && HEX.test(value.gateway_signer) && label(value.billing_owner) &&
    (bootstrap ? value.expected_catalog === null && value.expected_native === null && value.expected_generation === null && value.old_git === null :
      OID.test(value.expected_catalog) && STATE.test(value.expected_native) && Number.isSafeInteger(value.expected_generation) &&
      value.expected_generation >= 0 && OID.test(value.old_git) && (refresh || value.old_git !== value.new_git)) && OID.test(value.new_git) &&
    STATE.test(value.native_state) && HEX.test(value.authority_generation), 'Invalid publication fence');
  requireValue(Array.isArray(value.history) && value.history.length >= 1 && value.history.length <= 128, 'History limit');
  const states = new Set(), artifacts = new Map(); let total = 0;
  for (const revision of value.history) {
    requireValue(keys(revision, 'state,parents,artifacts') && STATE.test(revision.state) && !states.has(revision.state) &&
      Array.isArray(revision.parents) && revision.parents.length <= 128 &&
      revision.parents.every((p, i) => STATE.test(p) && states.has(p) && revision.parents.indexOf(p) === i) &&
      Array.isArray(revision.artifacts) && revision.artifacts.length >= 1 && revision.artifacts.length <= 16, 'Invalid exact history');
    states.add(revision.state);
    const seen = new Set();
    for (const artifact of revision.artifacts) {
      requireValue(keys(artifact, 'sha256,size,kind') && HEX.test(artifact.sha256) &&
        Number.isSafeInteger(artifact.size) && artifact.size > 0 && artifact.size <= MAX_NATIVE_BYTES &&
        ['pack', 'index'].includes(artifact.kind) && !seen.has(artifact.sha256), 'Invalid native artifact');
      seen.add(artifact.sha256);
      const previous = artifacts.get(artifact.sha256);
      requireValue(!previous || equal(previous, artifact), 'Conflicting native artifact');
      artifacts.set(artifact.sha256, artifact);
      total += artifact.size;
      requireValue(total <= MAX_NATIVE_BYTES, 'Combined history byte limit');
    }
  }
  requireValue(value.history.at(-1).state === value.native_state && (bootstrap || refresh || states.has(value.expected_native)), 'Incomplete history fence');
  // No unrelated historical siblings: every retained state must be reachable from the selected tip.
  const byState = new Map(value.history.map(r => [r.state, r])); const reachable = new Set();
  const visit = state => { if (!reachable.has(state)) { reachable.add(state); byState.get(state).parents.forEach(visit); } };
  visit(value.native_state);
  requireValue(reachable.size === states.size, 'Unrelated history');
  requireValue(encoder.encode(canonical(value)).length <= MAX_PUBLICATION_METADATA - 4096, 'Publication metadata limit');
  return [...artifacts.values()];
}
export function normalizeIntent(intent) {
  validateIntent(intent);
  const normalized = structuredClone(intent);
  // History/artifact transport order is not semantic. Preserve each revision's ordered
  // parents (Git first-parent identity), but choose one deterministic topological order.
  const remaining = new Map(normalized.history.map(r => [r.state, r])), emitted = new Set();
  normalized.history = [];
  while (remaining.size) {
    const ready = [...remaining.values()].filter(r => r.parents.every(p => emitted.has(p))).sort((a, b) => a.state < b.state ? -1 : a.state > b.state ? 1 : 0);
    requireValue(ready.length, 'Cyclic publication history');
    const revision = ready[0];
    revision.artifacts.sort((a, b) => a.sha256 < b.sha256 ? -1 : a.sha256 > b.sha256 ? 1 : 0);
    normalized.history.push(revision); emitted.add(revision.state); remaining.delete(revision.state);
  }
  return normalized;
}
export async function operationId(intent) { return hash({ domain: intent.schema === 2 ? 'heddle-native-bootstrap-publication-v1' : intent.schema === 3 ? 'heddle-native-refresh-publication-v1' : 'heddle-hosted-git-publication-v1', intent: normalizeIntent(intent) }); }
function binding(intent) {
  return { scope: intent.scope, actor: intent.actor, gateway_signer: intent.gateway_signer,
    billing_owner: intent.billing_owner, authority_generation: intent.authority_generation };
}
async function receiptValid(receipt, intent, operation) {
  const bootstrap = intent.schema !== 1;
  const expectedKeys = bootstrap ? 'schema,kind,operation,native_state,generation,authority_generation,history_sha256,actor,gateway_signer,billing_owner' :
    'schema,operation,native_state,previous_native,generation,authority_generation,history_sha256,actor,gateway_signer,billing_owner';
  requireValue(keys(receipt, expectedKeys) && receipt.schema === 1 && receipt.operation === operation &&
    receipt.native_state === intent.native_state && Number.isSafeInteger(receipt.generation) &&
    (bootstrap ? receipt.kind === (intent.schema === 2 ? 'native-bootstrap' : 'native-refresh') && (intent.schema === 2 ? receipt.generation >= 0 : receipt.generation === intent.expected_generation) :
      receipt.previous_native === intent.expected_native && receipt.generation > intent.expected_generation) &&
    receipt.authority_generation === intent.authority_generation && receipt.history_sha256 === await hash(intent.history) &&
    receipt.actor === intent.actor && receipt.gateway_signer === intent.gateway_signer && receipt.billing_owner === intent.billing_owner,
    bootstrap ? 'Native bootstrap proof mismatch' : 'Native acceptance receipt mismatch');
}
async function verifyObject(bucket, key, artifact) {
  const object = await bucket.get(key);
  if (!object) return false;
  await verifyNativeObject(object, artifact);
  return true;
}
// Real R2 bodies are hashed incrementally: read-back must not double a maximum-sized
// native allocation while a staged history is still resident. ArrayBuffer-only adapters
// are supported for bounded local unit fixtures, never selected for actual R2 bodies.
export async function verifyNativeObject(object, artifact) {
  requireValue(Number.isSafeInteger(artifact.size) && artifact.size > 0 && artifact.size <= MAX_NATIVE_BYTES &&
    /^[0-9a-f]{64}$/.test(artifact.sha256), 'Native R2 descriptor mismatch');
  if (object.size !== artifact.size) { void object.body?.cancel().catch(() => {}); throw new Error('Native R2 size mismatch'); }
  if (object.body) {
    requireValue(typeof crypto.DigestStream === 'function', 'Streaming native integrity unavailable');
    const hash = new crypto.DigestStream('SHA-256'), writer = hash.getWriter(), reader = object.body.getReader(); let total = 0;
    void hash.digest.catch(() => {});
    try {
      while (true) { const { done, value } = await reader.read(); if (done) break;
        requireValue(value instanceof Uint8Array && (total += value.byteLength) <= artifact.size, 'Native R2 stream length mismatch');
        await writer.write(value);
      }
      await writer.close();
      const actual = Array.from(new Uint8Array(await hash.digest), b => b.toString(16).padStart(2, '0')).join('');
      requireValue(total === artifact.size && actual === artifact.sha256, 'Native R2 content mismatch');
    } catch (error) { await reader.cancel().catch(() => {}); await writer.abort(error).catch(() => {}); throw error; }
    finally { reader.releaseLock(); writer.releaseLock(); }
    return;
  }
  const data = await object.arrayBuffer();
  requireValue(data instanceof ArrayBuffer && data.byteLength === artifact.size && await digest(data) === artifact.sha256, 'Native R2 content mismatch');
}
export class PublicationCoordinator {
  constructor({ storage, native, bucket, catalog, authorize }) {
    requireValue(storage?.transaction && storage?.get && native?.accept && native?.readArtifact && bucket?.put && bucket?.get &&
      catalog?.publish && catalog?.resolve && typeof authorize === 'function', 'Hosted publication integrations required');
    Object.assign(this, { storage, native, bucket, catalog, authorize });
  }
  async check(intent, credential, receipt = null, mode = 'write') {
    // The authority returns server-derived dimensions after re-verifying the original Biscuit,
    // current registration/revocations, publisher delegation, and exact history disclosure.
    const current = await this.authorize(intent, credential, { mode, receipt });
    requireValue(keys(current, 'scope,actor,gateway_signer,billing_owner,authority_generation,native_state,native_generation'),
      'Current publication authority unavailable or changed');
    const { native_state, native_generation, ...identity } = current;
    const expectedIdentity = binding(intent);
    if (mode === 'read') { requireValue(label(identity.actor), 'Authenticated reader absent'); expectedIdentity.actor = identity.actor; }
    requireValue(equal(identity, expectedIdentity), 'Current publication authority unavailable or changed');
    requireValue(Number.isSafeInteger(native_generation) && (receipt ?
      native_state === receipt.native_state && native_generation === receipt.generation :
      intent.schema !== 1 ? native_state === intent.native_state && (intent.schema === 2 ? native_generation >= 0 : native_generation === intent.expected_generation) :
      native_state === intent.expected_native && native_generation === intent.expected_generation ||
      native_state === intent.native_state && native_generation > intent.expected_generation), 'Native head changed during publication');
  }
  async replace(repository, previous, next) {
    await this.storage.transaction(async tx => {
      requireValue(equal(await tx.get(`pending:${repository}`), previous), 'Publication journal conflict');
      await tx.put(`pending:${repository}`, next);
    });
  }
  async bootstrap(intent, credential) {
    requireValue(intent.schema === 2 && typeof this.native.bootstrap === 'function', 'Existing native bootstrap authority required');
    return this.#start(intent, credential);
  }
  async refreshBase(repository) {
    requireValue(NAME.test(repository), 'Invalid repository');
    requireValue(!await this.storage.get(`pending:${repository}`), 'Pending publication must recover before native refresh');
    const current = await this.storage.get(`current:${repository}`);
    requireValue(current, 'Native refresh requires initialized catalog');
    const record = await this.storage.get(`receipt:${current.operation}`);
    requireValue(record && record.pin === current.pin && record.intent.scope.repository === repository, 'Refresh prior pointer corrupt');
    // Old native generation can be obsolete. Authenticate only the prior pinned metadata CAS
    // fence; never read obsolete source bytes. The new full closure is independently authorized.
    await this.verifyContent(record, { sources: false });
    requireValue(equal(await this.storage.get(`current:${repository}`), current) &&
      !await this.storage.get(`pending:${repository}`), 'Refresh base changed');
    return record;
  }
  async refresh(intent, credential) {
    intent = normalizeIntent(intent);
    requireValue(intent.schema === 3 && typeof this.native.refresh === 'function', 'Native refresh verifier required');
    await this.check(intent, credential);
    const complete = await this.storage.get(`receipt:${await operationId(intent)}`);
    if (complete) { await this.verifyPublished(complete, credential); return complete; }
    const prior = await this.refreshBase(intent.scope.repository);
    requireValue(prior.pin === intent.expected_catalog && prior.intent.native_state === intent.expected_native &&
      prior.intent.new_git === intent.old_git && equal(prior.intent.scope, intent.scope), 'Native refresh catalog fence mismatch');
    return this.#start(intent, credential);
  }
  async publish(intent, credential) {
    requireValue(intent.schema === 1, 'Git push publication requires a native acceptance command');
    return this.#start(intent, credential);
  }
  async reconcile(repository, observed, credential) {
    observed = structuredClone(observed);
    const kind = observed?.outcome ?? 'accepted-superseded', unknown = kind === 'unresolved-superseded';
    requireValue(NAME.test(repository) && ['accepted-superseded', 'unresolved-superseded'].includes(kind) &&
      keys(observed, 'expected_operation,expected_native,expected_generation,expected_catalog' + (observed?.outcome === undefined ? '' : ',outcome')) &&
      HEX.test(observed.expected_operation) && STATE.test(observed.expected_native) && OID.test(observed.expected_catalog) &&
      Number.isSafeInteger(observed.expected_generation) && observed.expected_generation >= 0 &&
      typeof this.native.reconcile === 'function', 'Explicit reconciliation fences required');
    const terminal = await this.storage.get(`superseded:${observed.expected_operation}`);
    requireValue(!terminal || keys(terminal, 'schema,kind,acceptance,operation,pending,receipt,observed,inspected,previous_current') &&
      terminal.schema === 1 && terminal.operation === observed.expected_operation && terminal.kind === kind &&
      terminal.acceptance === (unknown ? 'unknown' : 'accepted') && keys(terminal.previous_current, 'operation,pin') &&
      HEX.test(terminal.previous_current.operation) && OID.test(terminal.previous_current.pin), 'Corrupt terminal reconciliation');
    const record = terminal?.pending ?? await this.storage.get(`pending:${repository}`);
    requireValue(keys(record, 'operation,intent,stage,receipt,pin') && record.pin === null &&
      (record.stage === 'prepared' ? record.receipt === null : record.receipt !== null) &&
      record.intent?.schema === 1 && record.intent.scope.repository === repository &&
      record.operation === observed.expected_operation && record.operation === await operationId(record.intent) &&
      ['prepared', 'accepted'].includes(record.stage), 'Exact pending Git acceptance required');
    requireValue(!unknown || record.stage === 'prepared' && record.receipt === null,
      'Known acceptance cannot be relabeled unknown');
    const inspected = await this.native.reconcile(record.intent, record.operation, observed, record.receipt, credential);
    requireValue(keys(inspected, 'kind,operation,receipt,catalog_pin,pending_catalog_pin,current_native,current_generation,actor,scope') &&
      inspected.kind === kind && inspected.operation === record.operation &&
      inspected.actor === record.intent.actor && equal(inspected.scope, record.intent.scope) &&
      inspected.current_native === observed.expected_native && inspected.current_generation === observed.expected_generation &&
      inspected.catalog_pin === observed.expected_catalog && (unknown ? inspected.receipt === null && inspected.pending_catalog_pin === null &&
        inspected.catalog_pin === record.intent.expected_catalog : OID.test(inspected.pending_catalog_pin) &&
        inspected.pending_catalog_pin !== record.intent.expected_catalog &&
        [record.intent.expected_catalog, inspected.pending_catalog_pin].includes(inspected.catalog_pin)), 'Reconciliation authority or catalog differs');
    if (!unknown) await receiptValid(inspected.receipt, record.intent, record.operation);
    requireValue(inspected.current_generation > (unknown ? record.intent.expected_generation : inspected.receipt.generation) &&
      (!record.receipt || equal(record.receipt, inspected.receipt)), 'Reconciliation cannot alter acceptance or relax its generation fence');
    if (terminal) {
      requireValue(terminal.kind === kind && equal(terminal.observed, observed) &&
        equal(terminal.inspected, inspected) && equal(terminal.receipt, inspected.receipt) &&
        terminal.previous_current.pin === record.intent.expected_catalog, 'Terminal reconciliation differs');
      const previous = await this.storage.get(`receipt:${terminal.previous_current.operation}`);
      requireValue(previous && previous.pin === terminal.previous_current.pin && equal(previous.intent.scope, record.intent.scope),
        'Terminal prior metadata differs');
      await this.verifyContent(previous, { sources: false });
      requireValue(equal(inspected, await this.native.reconcile(record.intent, record.operation, observed, record.receipt, credential)),
        'Terminal reconciliation authority changed');
      return terminal;
    }
    const current = await this.storage.get(`current:${repository}`);
    requireValue(current?.pin === record.intent.expected_catalog, 'Reconciliation prior catalog pointer differs');
    const prior = await this.storage.get(`receipt:${current.operation}`);
    requireValue(prior && prior.pin === current.pin && equal(prior.intent.scope, record.intent.scope), 'Reconciliation prior metadata absent');
    await this.verifyContent(prior, { sources: false });
    let published = null;
    if (inspected.catalog_pin === inspected.pending_catalog_pin) {
      // The immutable manifest may have reached Artifacts before process loss. Record that
      // fact as the next CAS base, without claiming its now-stale native view is readable.
      const metadataBytes = encoder.encode(canonical({ schema: 2, operation: record.operation, intent: record.intent, native_receipt: inspected.receipt })).length;
      published = { ...record, receipt: inspected.receipt, stage: 'published', pin: inspected.catalog_pin, usage: {
        authenticated_actor: record.intent.actor, gateway_signer: record.intent.gateway_signer, spool_billing_owner: record.intent.billing_owner,
        native_source_bytes: validateIntent(record.intent).reduce((n, a) => n + a.size, 0), artifacts_metadata_bytes: metadataBytes,
        projection_cache_bytes: 0, accounting_basis: 'logical-content-reference-v1',
      } };
      await this.verifyContent(published, { sources: false });
    }
    requireValue(equal(inspected, await this.native.reconcile(record.intent, record.operation, observed, record.receipt, credential)),
      'Reconciliation authority changed during metadata inspection');
    const outcome = { schema: 1, kind, acceptance: unknown ? 'unknown' : 'accepted', operation: record.operation,
      pending: record, receipt: inspected.receipt, observed, inspected, previous_current: current };
    await this.storage.transaction(async tx => {
      requireValue(equal(await tx.get(`pending:${repository}`), record) && equal(await tx.get(`current:${repository}`), current) &&
        !await tx.get(`superseded:${record.operation}`), 'Reconciliation journal changed');
      await tx.put(`superseded:${record.operation}`, outcome);
      if (published) {
        const existing = await tx.get(`receipt:${record.operation}`);
        requireValue(!existing || equal(existing, published), 'Immutable publication record changed');
        await tx.put(`receipt:${record.operation}`, published);
        await tx.put(`current:${repository}`, { operation: record.operation, pin: published.pin });
      }
      await tx.delete(`pending:${repository}`);
    });
    return outcome;
  }
  async #start(intent, credential) {
    // Copy before the first await so caller mutation cannot change the durable semantic identity.
    intent = normalizeIntent(intent);
    const operation = await operationId(intent), repository = intent.scope.repository;
    requireValue(!await this.storage.get(`superseded:${operation}`), 'Git acceptance was explicitly superseded; no Git ACK is available');
    await this.check(intent, credential);
    const complete = await this.storage.get(`receipt:${operation}`);
    if (complete) { await this.verifyPublished(complete, credential); return complete; }
    await this.storage.transaction(async tx => {
      const previous = await tx.get(`pending:${repository}`);
      requireValue(!previous || previous.operation === operation && equal(previous.intent, intent), 'Another publication is pending');
      if (intent.schema === 3) requireValue((await tx.get(`current:${repository}`))?.pin === intent.expected_catalog, 'Native refresh current catalog changed');
      if (!previous) await tx.put(`pending:${repository}`, { operation, intent, stage: 'prepared', receipt: null, pin: null });
    });
    return this.recover(repository, credential);
  }
  async recover(repository, credential) {
    requireValue(NAME.test(repository), 'Invalid repository');
    let record = await this.storage.get(`pending:${repository}`);
    if (!record) return null;
    const { intent, operation } = record;
    requireValue(!await this.storage.get(`superseded:${operation}`), 'Superseded acceptance cannot resume publication');
    const artifacts = validateIntent(intent);
    requireValue(repository === intent.scope.repository && operation === await operationId(intent), 'Corrupt publication journal');
    await this.check(intent, credential);
    if (record.stage === 'prepared') {
      // accept is a genuine durable expected-old native transaction. An ambiguous response is
      // retried with the SAME semantic identity; no rollback of an accepted native head is claimed.
      const receipt = intent.schema === 2 ?
        await this.native.bootstrap(intent, operation, credential) : intent.schema === 3 ?
        await this.native.refresh(intent, operation, credential) : await this.native.accept(intent, operation, credential);
      await receiptValid(receipt, intent, operation);
      const next = { ...record, stage: 'accepted', receipt };
      await this.replace(repository, record, next); record = next;
    }
    await receiptValid(record.receipt, intent, operation);
    for (const artifact of artifacts) {
      await this.check(intent, credential, record.receipt);
      const key = `native/source/${artifact.sha256}`;
      if (!await verifyObject(this.bucket, key, artifact)) {
        // Only the accepted receiver can supply bytes here; a transient Git quarantine is never
        // a recovery dependency and Git pack bytes are not permanent source storage.
        const bytes = await this.native.readArtifact(record.receipt, artifact, credential);
        requireValue(bytes instanceof Uint8Array && bytes.byteLength === artifact.size &&
          await digest(bytes) === artifact.sha256, 'Accepted native artifact mismatch');
        await this.check(intent, credential, record.receipt);
        await this.bucket.put(key, bytes, { onlyIf: { etagDoesNotMatch: '*' }, sha256: artifact.sha256,
          customMetadata: { storage_class: 'native-source', content_sha256: artifact.sha256 } });
        requireValue(await verifyObject(this.bucket, key, artifact), 'Native R2 write not durable');
      }
    }
    await this.check(intent, credential, record.receipt);
    const manifest = { schema: 2, operation, intent, native_receipt: record.receipt };
    const manifestBytes = encoder.encode(canonical(manifest));
    requireValue(manifestBytes.length <= MAX_PUBLICATION_METADATA, 'Manifest metadata limit');
    // publish is deterministic and compares expected-old. A retry after an uncertain push must
    // recognize the exact existing commit; another successful operation must never be overwritten.
    const pin = await this.catalog.publish({ repository, expected: intent.expected_catalog,
      operation, manifest: manifestBytes }, credential);
    requireValue(typeof pin === 'string' && OID.test(pin), 'Invalid catalog publication pin');
    const published = { ...record, stage: 'published', pin, usage: {
      authenticated_actor: intent.actor, gateway_signer: intent.gateway_signer, spool_billing_owner: intent.billing_owner,
      native_source_bytes: artifacts.reduce((n, a) => n + a.size, 0), artifacts_metadata_bytes: manifestBytes.length,
      projection_cache_bytes: 0, accounting_basis: 'logical-content-reference-v1',
    } };
    await this.verifyPublished(published, credential);
    await this.storage.transaction(async tx => {
      requireValue(equal(await tx.get(`pending:${repository}`), record), 'Publication journal conflict');
      await tx.put(`receipt:${operation}`, published);
      await tx.put(`current:${repository}`, { operation, pin });
      await tx.delete(`pending:${repository}`);
    });
    // Expiry/revocation during the final storage transaction still prevents a Git ACK.
    await this.check(intent, credential, record.receipt);
    return published;
  }
  async verifyPublished(record, credential, mode = 'write') {
    const { intent } = record;
    await this.check(intent, credential, record.receipt, mode);
    await this.verifyContent(record);
    await this.check(intent, credential, record.receipt, mode);
  }
  async verifyContent(record, { sources = true } = {}) {
    const { intent, operation } = record;
    requireValue(record.stage === 'published' && operation === await operationId(intent) && OID.test(record.pin), 'Invalid publication receipt');
    await receiptValid(record.receipt, intent, operation);
    const actual = await this.catalog.resolve(intent.scope.repository, record.pin);
    const expected = encoder.encode(canonical({ schema: 2, operation, intent, native_receipt: record.receipt }));
    requireValue(actual instanceof Uint8Array && actual.length === expected.length &&
      await digest(actual) === await digest(expected), 'Published catalog differs from accepted source');
    // A completed journal is not authority, and missing/corrupt R2 content never becomes a cache hit.
    for (const artifact of sources ? validateIntent(intent) : [])
      requireValue(await verifyObject(this.bucket, `native/source/${artifact.sha256}`, artifact), 'Published native source unavailable');
  }
  async current(repository, credential, { receiveDiscovery = false, allowAbsent = false } = {}) {
    requireValue(NAME.test(repository), 'Invalid repository');
    if (receiveDiscovery) await this.recover(repository, credential);
    requireValue(!await this.storage.get(`pending:${repository}`), 'Native acceptance may require publication recovery');
    const current = await this.storage.get(`current:${repository}`);
    if (!current && allowAbsent) return null;
    requireValue(current, 'No published repository');
    const record = await this.storage.get(`receipt:${current.operation}`);
    requireValue(record && record.pin === current.pin, 'Publication receipt missing');
    await this.verifyPublished(record, credential, receiveDiscovery ? 'write' : 'read');
    return record;
  }
}
