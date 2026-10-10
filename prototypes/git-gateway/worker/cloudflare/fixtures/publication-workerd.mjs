// SPDX-License-Identifier: Apache-2.0
// LOCAL TEST FIXTURE ONLY. Runs the unmodified production coordinator with real workerd
// Durable Object storage and R2 bindings. Native acceptance, authorization, and Git catalog
// publication below are controlled mocks, not live Weft, Biscuit, or Artifacts integrations.
import { PublicationCoordinator, canonical, digest, operationId, verifyNativeObject } from '../../publication.mjs';
import { R2NativeStaging } from '../../native-staging.mjs';

const encoder = new TextEncoder();
const hash = value => digest(encoder.encode(canonical(value)));
const requireValue = (value, message) => { if (!value) throw new Error(message); };

export class PublicationRecoveryTestObject {
  constructor(ctx, env) {
    this.storage = ctx.storage;
    this.phases = Object.create(null);
    // Timing wrappers delegate directly to actual workerd R2. They do not replace its
    // storage semantics or persist any request, credential, object key, or content.
    this.bucket = {
      get: (...args) => this.measure('r2_get_actual', () => env.SOURCE_BYTES.get(...args)),
      put: (...args) => this.measure('r2_put_actual', () => env.SOURCE_BYTES.put(...args)),
    };
    this.instance = crypto.randomUUID();
    this.staging = new R2NativeStaging({
      bucket: this.bucket,
      authorize: async (intent, credential) => {
        await this.mockAuthorize(intent, credential, { mode: 'write' }); return true;
      },
      // This exact token-free JSON vector is deliberately synthetic, not a native prost
      // validator. These tests verify the real staging boundary and persistence only.
      validatePlan: async (intent, proof) => new TextDecoder().decode(proof) === canonical({
        kind: 'synthetic-native-proof-test-only', operation: await operationId(intent),
        source_sha256: (await this.storage.get('mock:state')).artifact.sha256,
      }),
      submit: async (intent, proof, artifacts, credential) => {
        const mock = await this.storage.get('mock:state');
        requireValue(mock.bytes === null, 'Staging test must not retain source bytes in the native mock');
        requireValue(artifacts.length === 1 && await digest(artifacts[0].bytes) === mock.artifact.sha256,
          'Mock native submit received incorrect staged source');
        await this.mockUpdate(value => { value.submitCalls++; value.freshSessionUsed = credential === 'PUBLIC_MOCK_WRITER_SESSION_FRESH'; });
        return this.mockAccept(intent, await operationId(intent));
      },
      resolveIntent: async receipt => {
        const pending = await this.storage.get('pending:toy');
        const record = pending?.operation === receipt.operation ? pending : await this.storage.get(`receipt:${receipt.operation}`);
        requireValue(record?.operation === receipt.operation, 'Durable receipt-to-intent mapping absent');
        return record.intent;
      },
      verifyBootstrap: (intent) => operationId(intent).then(operation => this.mockBootstrap(intent, operation)),
      verifyRefresh: (intent) => operationId(intent).then(operation => this.mockRefresh(intent, operation)),
    });
    this.coordinator = new PublicationCoordinator({
      storage: ctx.storage,
      bucket: this.bucket,
      native: {
        accept: async (intent, operation, credential) => (await this.storage.get('mock:state')).useStaging ?
          this.staging.accept(intent, operation, credential) : this.mockAccept(intent, operation),
        bootstrap: async (intent, operation, credential) => (await this.storage.get('mock:state')).useStaging ?
          this.staging.bootstrap(intent, operation, credential) : this.mockBootstrap(intent, operation),
        refresh: async (intent, operation, credential) => (await this.storage.get('mock:state')).useStaging ?
          this.staging.refresh(intent, operation, credential) : this.mockRefresh(intent, operation),
        readArtifact: async (receipt, artifact, credential) => (await this.storage.get('mock:state')).useStaging ?
          this.staging.readArtifact(receipt, artifact, credential) : this.mockReadArtifact(receipt, artifact),
      },
      catalog: {
        publish: publication => this.mockPublishCatalog(publication),
        resolve: (repository, pin) => this.mockResolveCatalog(repository, pin),
      },
      authorize: (intent, credential, options) => this.measure('authority_adapter',
        () => this.mockAuthorize(intent, credential, options)),
    });
    for (const [method, phase] of Object.entries({ accept: 'native_accept_adapter', bootstrap: 'native_bootstrap_adapter',
      refresh: 'native_refresh_adapter', readArtifact: 'native_source_adapter' })) {
      const action = this.coordinator.native[method];
      this.coordinator.native[method] = (...args) => this.measure(phase, () => action(...args));
    }
    for (const [method, phase] of Object.entries({ publish: 'catalog_cas_adapter', resolve: 'catalog_read_adapter' })) {
      const action = this.coordinator.catalog[method];
      this.coordinator.catalog[method] = (...args) => this.measure(phase, () => action(...args));
    }
  }
  async measure(phase, action) {
    const start = Date.now(); let failed = false;
    try { return await action(); } catch (error) { failed = true; throw error; }
    finally {
      const data = this.phases[phase] ??= { count: 0, failed: 0, elapsed_ms: 0 };
      data.count++; data.failed += Number(failed); data.elapsed_ms += Math.max(0, Date.now() - start);
    }
  }
  async mockUpdate(action) {
    return this.storage.transaction(async tx => {
      const mock = await tx.get('mock:state');
      requireValue(mock, 'Mock service fixture not initialized');
      const result = await action(mock, tx);
      await tx.put('mock:state', mock);
      return result;
    });
  }
  async fault(phase) {
    const hit = await this.mockUpdate(mock => {
      if (mock.fault !== phase) return false;
      mock.fault = null;
      return true;
    });
    if (hit) throw new Error(`injected ${phase}`);
  }
  async mockAuthorize(intent, credential, { mode }) {
    const mock = await this.storage.get('mock:state');
    const writer = /^PUBLIC_MOCK_WRITER_SESSION_[A-Z]+$/.test(credential ?? '');
    const reader = /^PUBLIC_MOCK_READER_SESSION_[A-Z]+$/.test(credential ?? '');
    requireValue(!mock.denied && (writer || mode === 'read' && reader), 'Mock current authority denied');
    return { scope: structuredClone(mock.intent.scope), actor: reader ? 'user:reader' : mock.intent.actor,
      gateway_signer: mock.intent.gateway_signer, billing_owner: mock.intent.billing_owner,
      authority_generation: mock.intent.authority_generation,
      native_state: mock.nativeState, native_generation: mock.nativeGeneration };
  }
  async mockBootstrap(intent, operation) {
    const proof = await this.mockUpdate(async (mock, tx) => {
      mock.bootstrapCalls++;
      requireValue(intent.schema === 2 && (await tx.get('pending:toy'))?.operation === operation,
        'Mock bootstrap requires an existing native head and durable journal');
      requireValue(mock.nativeState === intent.native_state, 'Mock bootstrap existing state changed');
      mock.nativeBootstrapProof ??= { schema: 1, kind: mock.bootstrapProofKind, operation,
        native_state: mock.nativeState, generation: mock.nativeGeneration,
        authority_generation: intent.authority_generation, history_sha256: await hash(intent.history),
        actor: intent.actor, gateway_signer: intent.gateway_signer, billing_owner: intent.billing_owner };
      return mock.nativeBootstrapProof;
    });
    await this.fault('after-bootstrap-proof');
    return proof;
  }
  async mockRefresh(intent, operation) {
    const proof = await this.mockUpdate(async mock => {
      mock.refreshCalls++;
      requireValue(intent.schema === 3 && mock.nativeState === intent.native_state &&
        mock.nativeGeneration === intent.expected_generation, 'Mock current refresh closure changed');
      const proof = { schema: 1, kind: 'native-refresh', operation, native_state: mock.nativeState,
        generation: mock.nativeGeneration, authority_generation: intent.authority_generation,
        history_sha256: await hash(intent.history), actor: intent.actor,
        gateway_signer: intent.gateway_signer, billing_owner: intent.billing_owner };
      mock.nativeRefreshProofs[operation] = proof;
      if (mock.denyAfterRefreshProof) mock.denied = true;
      return proof;
    });
    await this.fault('after-refresh-proof');
    return proof;
  }
  async mockAccept(intent, operation) {
    const receipt = await this.mockUpdate(async (mock, tx) => {
      mock.acceptCalls++;
      requireValue(intent.schema === 1, 'Mock native accept cannot bootstrap an existing state');
      requireValue((await tx.get(`pending:${intent.scope.repository}`))?.operation === operation,
        'Native mock requires durable prepared journal before acceptance');
      if (mock.nativeReceipt) {
        requireValue(mock.nativeReceipt.operation === operation, 'Mock native expected-old conflict');
        return mock.nativeReceipt;
      }
      requireValue(mock.nativeState === intent.expected_native && mock.nativeGeneration === intent.expected_generation,
        'Mock native expected-old conflict');
      mock.nativeState = intent.native_state;
      mock.nativeGeneration++;
      mock.nativeMutations++;
      mock.nativeReceipt = { schema: 1, operation, native_state: intent.native_state,
        previous_native: intent.expected_native, generation: mock.nativeGeneration,
        authority_generation: intent.authority_generation, history_sha256: await hash(intent.history),
        actor: intent.actor, gateway_signer: intent.gateway_signer, billing_owner: intent.billing_owner };
      return mock.nativeReceipt;
    });
    await this.fault('after-native-commit');
    return receipt;
  }
  async mockReadArtifact(receipt, artifact) {
    return this.mockUpdate(mock => {
      requireValue([mock.nativeReceipt, mock.nativeBootstrapProof, ...Object.values(mock.nativeRefreshProofs)].some(proof =>
        proof?.operation === receipt.operation) && mock.artifact.sha256 === artifact.sha256,
        'Mock accepted source absent');
      mock.artifactReads++;
      return new Uint8Array(mock.bytes);
    });
  }
  async mockPublishCatalog({ repository, expected, operation, manifest }) {
    await this.fault('before-catalog-publish');
    const pin = await this.mockUpdate(async mock => {
      mock.catalogCalls++;
      const document = JSON.parse(new TextDecoder().decode(manifest));
      requireValue(repository === mock.intent.scope.repository && expected === document.intent.expected_catalog,
        'Mock catalog scope mismatch');
      const exact = Array.from(manifest);
      const prior = Object.values(mock.catalogs).find(value => value.operation === operation);
      if (prior) {
        requireValue(mock.catalogHead === prior.pin && canonical(prior.bytes) === canonical(exact),
          'Mock catalog expected-old conflict');
        return prior.pin;
      }
      requireValue(mock.catalogHead === expected, 'Mock catalog expected-old conflict');
      const pin = (await digest(manifest)).slice(0, 40);
      mock.catalog = { operation, pin, expected, bytes: exact };
      mock.catalogs[pin] = mock.catalog;
      mock.catalogHead = pin;
      mock.catalogMutations++;
      return pin;
    });
    await this.fault('after-catalog-commit');
    return pin;
  }
  async mockResolveCatalog(repository, pin) {
    const mock = await this.storage.get('mock:state');
    requireValue(repository === mock.intent.scope.repository && mock.catalogs[pin], 'Mock catalog pin absent');
    const bytes = new Uint8Array(mock.catalogs[pin].bytes);
    if (mock.corruptCatalog) bytes[0] ^= 1;
    return bytes;
  }
  async fetch(request) {
    try {
      const url = new URL(request.url);
      const credential = request.headers.get('authorization');
      if (request.method === 'POST' && url.pathname === '/setup') {
        requireValue(!await this.storage.get('mock:state'), 'Fixture already initialized');
        const { intent, artifact, bytes, fault = null, useStaging = false } = await request.json();
        await this.storage.put('mock:state', { intent, artifact, bytes: useStaging ? null : bytes, fault, denied: false, useStaging,
          nativeState: intent.schema === 2 ? intent.native_state : intent.expected_native,
          nativeGeneration: intent.schema === 2 ? 7 : intent.expected_generation,
          nativeReceipt: null, nativeBootstrapProof: null, nativeRefreshProofs: {}, refreshCalls: 0,
          denyAfterRefreshProof: false, corruptCatalog: false, bootstrapProofKind: 'native-bootstrap', bootstrapCalls: 0,
          nativeMutations: 0, acceptCalls: 0, artifactReads: 0, submitCalls: 0, freshSessionUsed: false,
          catalog: null, catalogs: {}, catalogHead: intent.expected_catalog, catalogMutations: 0, catalogCalls: 0 });
        return Response.json({ ready: true });
      }
      if (request.method === 'POST' && url.pathname === '/control') {
        const patch = await request.json();
        requireValue(Object.keys(patch).every(key => ['denied', 'catalogHead', 'bootstrapProofKind', 'nativeState', 'nativeGeneration', 'fault',
          'denyAfterRefreshProof', 'corruptCatalog', 'artifact', 'bytes'].includes(key)), 'Unknown mock control');
        await this.mockUpdate(mock => Object.assign(mock, patch));
        return Response.json({ changed: true });
      }
      if (request.method === 'POST' && url.pathname === '/corrupt-current') {
        const { field, value } = await request.json();
        requireValue(['operation', 'pin'].includes(field), 'Unknown current corruption target');
        await this.storage.transaction(async tx => {
          const current = await tx.get('current:toy');
          requireValue(current, 'Current pointer absent');
          current[field] = value; await tx.put('current:toy', current);
        });
        return Response.json({ changed: true });
      }
      if (request.method === 'GET' && url.pathname === '/phase-trace') return Response.json(this.phases);
      if (request.method === 'GET' && url.pathname === '/verify-real-body') {
        const { artifact } = await this.storage.get('mock:state');
        const object = await this.bucket.get(`native/source/${artifact.sha256}`);
        requireValue(object?.body, 'Actual R2 source body absent');
        let arrayBufferCalls = 0;
        await verifyNativeObject({ size: object.size, body: object.body, arrayBuffer() {
          arrayBufferCalls++; throw new Error('Actual R2 integrity must not allocate a complete array buffer');
        } }, artifact);
        return Response.json({ verified: true, array_buffer_calls: arrayBufferCalls });
      }
      if (request.method === 'POST' && url.pathname === '/stage-ceiling') {
        // Synthetic ceiling fixture only: buffers are created inside this actual workerd
        // isolate, never expanded into JSON number arrays or claimed as genuine native proof.
        const nativeSize = 64 * 1024 * 1024, proofSize = 17 * 1024 * 1024;
        const source = new Uint8Array(nativeSize); source.fill(0xa5);
        const sha256 = await digest(source), proof = new Uint8Array(proofSize); proof.fill(0x5a);
        const { intent: base } = await this.storage.get('mock:state');
        requireValue(base.schema === 2, 'Synthetic ceiling requires bootstrap intent');
        const artifact = { sha256, size: nativeSize, kind: 'pack' };
        const intent = { ...base, history: [{ state: base.native_state, parents: [], artifacts: [artifact] }] };
        let arrayBufferCalls = 0, streamedReadbacks = 0;
        const bucket = { put: (...args) => this.bucket.put(...args), get: async (...args) => {
          const object = await this.bucket.get(...args); if (!object) return null;
          requireValue(object.body, 'Actual ceiling R2 body absent'); streamedReadbacks++;
          return { size: object.size, body: object.body, arrayBuffer() {
            arrayBufferCalls++; throw new Error('Ceiling read-back must stream');
          } };
        } };
        const staging = new R2NativeStaging({ bucket,
          authorize: async () => true, // Explicit controlled authority adapter.
          validatePlan: async (_, ownedProof, sources) => ownedProof.length === proofSize && ownedProof[0] === 0x5a &&
            ownedProof[proofSize - 1] === 0x5a && sources.length === 1 && sources[0].sha256 === sha256,
          submit: async () => { throw new Error('Ceiling fixture must never submit native state'); },
          resolveIntent: async () => { throw new Error('Ceiling fixture must never resolve a receipt'); },
        });
        const pending = staging.stage(intent, proof, [{ sha256, bytes: source }], credential);
        const detached = proof.buffer.byteLength === 0 && source.buffer.byteLength === 0;
        await pending;
        return Response.json({ native_bytes: nativeSize, proof_bytes: proofSize, synchronously_detached: detached,
          streamed_readbacks: streamedReadbacks, array_buffer_calls: arrayBufferCalls });
      }
      if (request.method === 'GET' && url.pathname === '/inspect') {
        const all = Object.fromEntries(await this.storage.list());
        const mock = all['mock:state']; delete all['mock:state'];
        return Response.json({ instance: this.instance, records: all, mock });
      }
      if (request.method === 'POST' && ['/stage', '/stage-transfer'].includes(url.pathname)) {
        const { intent, proof, artifacts } = await request.json();
        const ownedProof = new Uint8Array(proof);
        const ownedArtifacts = artifacts.map(value => ({ sha256: value.sha256, bytes: new Uint8Array(value.bytes) }));
        const pending = this.staging.stage(intent, ownedProof, ownedArtifacts, credential);
        let ownership;
        if (url.pathname === '/stage-transfer') {
          const originals = [ownedProof, ...ownedArtifacts.map(value => value.bytes)];
          let rejectedMutations = 0;
          for (const bytes of originals) {
            try { bytes.fill(0xff); } catch (error) { if (error instanceof TypeError) rejectedMutations++; else throw error; }
          }
          ownership = { synchronously_detached: originals.every(bytes => bytes.buffer.byteLength === 0),
            rejected_mutations: rejectedMutations, original_buffers: originals.length };
        }
        const operation = await pending;
        return Response.json({ operation, ...(ownership ? { ownership } : {}) });
      }
      if (request.method === 'POST' && url.pathname === '/refresh')
        return Response.json(await this.coordinator.refresh(await request.json(), credential));
      if (request.method === 'POST' && url.pathname === '/bootstrap')
        return Response.json(await this.coordinator.bootstrap(await request.json(), credential));
      if (request.method === 'POST' && url.pathname === '/publish')
        return Response.json(await this.coordinator.publish(await request.json(), credential));
      if (request.method === 'GET' && url.pathname === '/current') {
        const receiveDiscovery = url.searchParams.get('receive-discovery') === '1';
        return Response.json(await this.measure(receiveDiscovery ? 'receive_discovery_recovery' : 'current_verification',
          () => this.coordinator.current('toy', credential, { receiveDiscovery })));
      }
      return new Response('Unknown local test route', { status: 404 });
    } catch (error) {
      // A publication error cannot become a success response. This is a harness endpoint,
      // not proof of a live Git receive-pack ACK or a deployed Worker route.
      return Response.json({ error: error.message }, { status: 503 });
    }
  }
}

export default {
  fetch(request, env) {
    return env.PUBLICATION.get(env.PUBLICATION.idFromName('toy')).fetch(request);
  },
};
