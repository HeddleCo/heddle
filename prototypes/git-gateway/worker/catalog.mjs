// SPDX-License-Identifier: Apache-2.0
// ReadCatalog: async resolve(immutableCommitPin) -> validated manifest.
// The publication allowlist comes from the trusted publisher, never the request.
import { abortable } from './abortable.mjs';
const OID = /^[0-9a-f]{40}$/;
const NAME = /^[a-z][a-z0-9-]{0,63}$/;
const THREAD = /^[a-z][a-z0-9-]{0,63}(?:\/[a-z][a-z0-9-]{0,63}){0,7}$/;
const KEYS = ['git_oid', 'mode', 'policy_epoch', 'repository', 'schema', 'source', 'state', 'thread'];
export function validateManifest(text) {
  if (new TextEncoder().encode(text).length > 16384) throw new Error('manifest limit');
  const m = JSON.parse(text);
  if (!m || Array.isArray(m) || Object.keys(m).sort().join(',') !== KEYS.join(',')) throw new Error('invalid manifest');
  if (m.schema !== 1 || !['snapshot', 'history'].includes(m.mode) ||
      !Number.isSafeInteger(m.policy_epoch) || m.policy_epoch < 1 ||
      typeof m.git_oid !== 'string' || !OID.test(m.git_oid) ||
      typeof m.state !== 'string' || !/^hs-[0-9a-z]{52}$/.test(m.state)) throw new Error('invalid manifest');
  for (const name of ['repository', 'source']) {
    if (typeof m[name] !== 'string' || !NAME.test(m[name])) throw new Error('invalid manifest');
  }
  if (typeof m.thread !== 'string' || m.thread.length > 255 || !THREAD.test(m.thread)) throw new Error('invalid manifest');
  const canonical = JSON.stringify(Object.fromEntries(KEYS.map(key => [key, m[key]]))) + '\n';
  if (text !== canonical) throw new Error('noncanonical manifest');
  return Object.freeze(m);
}

export async function readManifestBlob(blob, signal) {
  if (!(blob instanceof Blob) || blob.size > 16384) throw new Error('catalog view unavailable');
  signal?.throwIfAborted();
  // Blob.text() silently strips a UTF-8 BOM and replaces invalid sequences.
  // Preserve the exact bytes required by the shared canonical JSON contract.
  const bytes = await abortable(blob.arrayBuffer(), signal);
  const text = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes);
  return validateManifest(text);
}

export class ArtifactsCatalog {
  constructor(artifacts, repository, publishedPins) {
    if (!Array.isArray(publishedPins) || publishedPins.length > 1024 ||
        publishedPins.some(pin => typeof pin !== 'string' || !OID.test(pin))) throw new Error('invalid publication pins');
    this.artifacts = artifacts; this.repository = repository;
    this.publishedPins = new Set(publishedPins);
  }
  async resolve(pin, signal) {
    if (typeof pin !== 'string' || !OID.test(pin) || !this.publishedPins.has(pin)) throw new Error('unpublished catalog pin');
    signal?.throwIfAborted();
    const repo = await abortable(this.artifacts.get(this.repository), signal, handle => handle[Symbol.dispose]());
    try {
      signal?.throwIfAborted();
      // Documented Blob|null return; there is no binding put/write method.
      const blob = await abortable(repo.readFile({ ref: pin, path: 'manifest.json' }), signal);
      return await readManifestBlob(blob, signal);
    } finally {
      repo[Symbol.dispose]();
    }
  }
}
