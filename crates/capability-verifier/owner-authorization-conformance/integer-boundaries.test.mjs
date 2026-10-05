// Exercise the generated package: Rust-only WASM tests cannot see ABI truncation.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import test from "node:test";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const dist = process.env.OWNER_AUTH_BINDING_ROOT ?? path.join(root, "npm", "dist");
const wasm = await import(pathToFileURL(path.join(dist, "capability_verifier.js")));
wasm.initSync({ module: readFileSync(path.join(dist, "capability_verifier_bg.wasm")) });
const fixture = name => JSON.parse(readFileSync(path.join(root, "conformance", name), "utf8"));
const production = fixture("fixtures/production-v1.json").cases;
const nativeGenesis = fixture("fixtures/native-genesis-v1.json").cases.find(c => c.id === "start_thread");
const hybrid = fixture("hybrid/import-authority-host-witness-v1.json");
const bytes = hex => new Uint8Array(Buffer.from(hex, "hex"));
const transfer = production.find(c => c.api === "transfer" && c.expected_accept);
const ring = production.find(c => c.id === "policy-api-fixed-vector");
const genesis = production.find(c => c.id === "genesis-api-self-signed");
const purge = fixture("fixtures/v2.json").cases.find(c => c.expected === "purge");
const timeline = fixture("fixtures/timeline-v3.json").cases.find(c => c.expected_accept);
const vector = name => bytes((hybrid.signed_vectors[name] ?? hybrid.wire_vectors[name]).wire_hex);
const owner = wasm.verifyOwnerRoot(bytes(production.find(c => c.id === "owner-root-api-fixed-vector").root_hex));
const lineage = wasm.verifyResourceKeyring(bytes(ring.keyring_hex), bytes(ring.current_owner_hex), 1100n, 3600n);
const exports = [
  ["verifyOwnershipTransfer", [bytes(transfer.transfer_hex), bytes(transfer.source_history_hex), bytes(transfer.destination_history_hex), bytes(transfer.resource_uuid_hex), BigInt(transfer.sequence), BigInt(transfer.now), 3600n], [[4, "expected_sequence", "u64"], [5, "now_unix_seconds", "i64"], [6, "max_capability_ttl_seconds", "i64"]]],
  ...["verifyResourceKeyring", "verifyOwnershipTransferChain"].map(name => [name, [bytes(ring.keyring_hex), bytes(ring.current_owner_hex), 1100n, 3600n], [[2, "now_unix_seconds", "i64"], [3, "max_capability_ttl_seconds", "i64"]]]),
  ["verifySpoolOwnerGenesis", [bytes(genesis.genesis_hex), BigInt(genesis.now)], [[1, "now_unix_seconds", "i64"]]],
  ["verifySignedPolicyChain", [ring.records_hex.map(bytes), bytes(ring.keyring_hex), bytes(ring.current_owner_hex), 1100n, 3600n], [[3, "now_unix_seconds", "i64"], [4, "max_capability_ttl_seconds", "i64"]]],
  ["verifyNativeGenesisAuthority", [bytes(nativeGenesis.binding_hex), bytes(nativeGenesis.original_hex), bytes(nativeGenesis.envelope_hex), bytes(nativeGenesis.keyring_hex), bytes(nativeGenesis.current_owner_hex), bytes(nativeGenesis.initial_owner_hex), bytes(nativeGenesis.spool_genesis_hex), "[]", "[]", 1100n, 3600n], [[9, "now_unix_seconds", "i64"], [10, "max_capability_ttl_seconds", "i64"]]],
  ["verifyImportDelegation", [vector("delegation"), vector("permission"), bytes(ring.keyring_hex), vector("owner_history"), bytes(owner.owner_id_hex), bytes(lineage.spool_genesis_digest_hex), JSON.stringify(["root", "witness", "next_witness"].map(role => hybrid.keys[role].public_key_hex)), "[]", "[]", "[]", 1100n, 3600n], [[10, "now_unix_seconds", "i64"], [11, "max_capability_ttl_seconds", "i64"]]],
  ["verifyPurgeAuthorization", [bytes(purge.authorization_hex), bytes(purge.operation_body_hex), bytes(purge.payload_hex), bytes(purge.owner_genesis_hex), bytes(purge.current_owner_state_hash_hex), bytes(purge.spool_uuid_hex), purge.spool_path_segments, BigInt(purge.now_unix_seconds), 3600n], [[7, "now_unix_seconds", "i64"], [8, "max_capability_ttl_seconds", "i64"]]],
  ["verifyTimelineAcceptance", [bytes(timeline.origin_hex), bytes(timeline.acceptance_hex), bytes(timeline.current_owner_state_hash_hex), timeline.spool_path_segments, bytes(timeline.request_sha256_hex), BigInt(timeline.first_position), timeline.event_count, timeline.revoked_capability_ids_hex, timeline.revoked_subject_ids_hex, BigInt(timeline.now_unix_seconds), 3600n], [[5, "first_position", "u64"], [9, "now_unix_seconds", "i64"], [10, "max_capability_ttl_seconds", "i64"]]],
];
const typed = error => {
  assert.equal(error instanceof Error, false, "must throw a typed VerificationError, never TypeError");
  assert.equal(typeof error.code, "string");
  assert.equal(typeof error.message, "string");
  return true;
};
for (const [name, args, fields] of exports) {
  test(`${name}: valid control`, () => {
    const result = wasm[name](...args);
    if (name === "verifyTimelineAcceptance") assert.equal(result, true);
    else if (name === "verifyPurgeAuthorization") assert.equal(result, "purge");
    else assert.equal(typeof result, "object");
  });
  for (const [index, field, kind] of fields) {
    const invoke = value => wasm[name](...args.map((arg, i) => i === index ? value : arg));
    const low = kind === "u64" ? 0n : -(1n << 63n);
    const high = kind === "u64" ? (1n << 64n) - 1n : (1n << 63n) - 1n;
    for (const [label, value] of [["below-min", low - 1n], ["above-max", high + 1n], ["valid+2^64", args[index] + (1n << 64n)], ["valid-2^64", args[index] - (1n << 64n)]]) {
      test(`${name}.${field}: ${label}`, () => assert.throws(() => invoke(value), error => {
        typed(error);
        assert.equal(error.code, "invalid");
        assert.equal(error.message, `invalid owner-authorization object: ${field} is outside the ${kind} range`);
        return true;
      }));
    }
    for (const value of [low, low + 1n, high - 1n, high]) {
      test(`${name}.${field}: representable ${value}`, () => {
        try { invoke(value); } catch (error) {
          typed(error);
          assert.equal(error.message.includes("range"), false, "native semantic checks must receive representable endpoints");
        }
      });
    }
    for (const value of [Number(args[index]), 1.5, NaN, Infinity, "1", true, null, undefined, {}, Object(1n), Symbol("integer")]) {
      test(`${name}.${field}: wrong type ${String(value)}`, () => assert.throws(() => invoke(value), error => {
        typed(error);
        assert.equal(error.code, "invalid");
        assert.equal(error.message, `invalid owner-authorization object: ${field} must be a bigint`);
        return true;
      }));
    }
  }
}
const [, importArgs] = exports.find(([name]) => name === "verifyImportDelegation");
for (const value of [((1n << 63n) - 1n) / 1000n + 1n, -(1n << 63n) / 1000n - 1n]) {
  test(`verifyImportDelegation: checked milliseconds overflow ${value}`, () => assert.throws(
    () => wasm.verifyImportDelegation(...importArgs.map((arg, i) => i === 10 ? value : arg)),
    error => { typed(error); assert.equal(error.code, "hybrid_bounds"); return true; },
  ));
}
test("verifyImportDelegation: native expiry preserved", () => assert.throws(
  () => wasm.verifyImportDelegation(...importArgs.map((arg, i) => i === 10 ? 1400n : arg)),
  error => { typed(error); assert.equal(error.code, "hybrid_expired"); return true; },
));
test("verifyImportDelegation: native positive TTL preserved", () => assert.throws(
  () => wasm.verifyImportDelegation(...importArgs.map((arg, i) => i === 11 ? 0n : arg)),
  error => { typed(error); assert.equal(error.code, "invalid"); assert.match(error.message, /TTL ceiling must be positive/); return true; },
));

for (const [id, kind, requiresClaim] of [["start_thread", "account", false], ["local_adopt_push", "local_key", true]]) {
  const c = fixture("fixtures/native-genesis-v1.json").cases.find(c => c.id === id);
  const result = wasm.verifyNativeGenesisAuthority(bytes(c.binding_hex), bytes(c.original_hex), bytes(c.envelope_hex), bytes(c.keyring_hex), bytes(c.current_owner_hex), bytes(c.initial_owner_hex), bytes(c.spool_genesis_hex), "[]", "[]", 1100n, 3600n);
  assert.equal(result.owner_kind, kind, "native binding kind must be explicit");
  assert.equal(result.requires_hosting_claim, requiresClaim, "LocalKey binding needs separate hosted authority");
}
