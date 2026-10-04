# HYBRID Part 4: native witnessing

[PR #1968](https://github.com/HeddleCo/heddle/pull/1968) implements the native
arm of #1961 on API `0.31.0-alpha.30`, commit
`09827252a76d2b3b8509836a32e6fc8723d5ff49`. Both API pins and the standalone
verifier consumer use that contract. The three native fixture copies match the
tag byte for byte (SHA-256
`7609f0012cdb0b6c43251913834f7e82a32a5cff5fe8ef4d91138938182ff015`).
The corrected owner-chain vectors are enabled; there is no alpha.28 skip.

## Production behavior

- Native genesis keeps its original signature and a second creator signature
  binding the exact authority envelope, owner identity and history, Spool
  lineage, and publisher. Account creation verifies StartThread authority with
  independently selected roots and historical time. LocalKey remains the
  immutable creator and requires an explicit co-signed hosted ownership claim.
- Explicit native/import carriers dispatch independently. Dual, malformed,
  unsupported and unwitnessed hosted evidence rejects. Import delegation is
  required only by the import arm; neither arm falls back to the other.
- Native installation, publication, Fetch and relay use Part 1b's artifact
  transaction and HostedTrust serialization. Current authorization is checked
  again before and after artifact publication. Durable proofs, creator bindings,
  admissions, native records and filesystem artifacts share rollback.
- Account source/control/claim/resolution uses purpose 2. Account-authored
  LocalIntegration also uses its original source authority and purpose 2.
  Hosted integration dependencies use their byte-identical purpose-4 execution.
  LocalKey capture/integration uses native proof and its Thread's witnessed
  claim. LocalIntegration selects its cross-Thread source operation recursively.
- The verified LocalKey publisher must equal the immutable genesis local owner.
  Local work must be an ancestor of the sole claim's signed source frontier or
  the authorized resolution's frontier. Conflicts need the exact complete claim
  set and winning claim. Each source Thread has its own ownership cutoff.
- Complete witness history survives Fetch, Push and device relay. Refresh changes
  only the public witness set and exact retirement paths, preserves original
  bytes, and rechecks durable root epoch/high-water/time at commit. Revoked
  witness keys cannot be refreshed into authority.
- CLI/client StartThread freezes authority and genesis, signs the creator
  binding, then signs request PoP. Local adopt retains the original local key
  and publishes its explicit claim. Present bindings require capable peers and
  advertise protocol 2; `SYNC_MANDATORY_GATE` stays **off**.
- The production WASM binding exposes `verifyNativeGenesisAuthority` with
  checked bigint time/TTL inputs. Native/WASM conformance includes native
  creator-binding, envelope, lineage, expiry and revocation cases.

## Tests and discriminating controls

`thread-api::hybrid::native_tests` executes the published positives and every
portable negative, plus real atomic installs and durable-state assertions.
The owner/cutoff negatives independently pass portable signature, claim,
capability and witness verification before the production native gate refuses
them. Controls include a cutoff head's ancestors, beyond-cutoff descendants,
wrong-key integration with a valid claim, and both violations combined.
Account integration requires purpose 2; competing valid first admissions for
one subject reject. Late authorization loss restores all artifacts and history.

`hosted-client::native_sync::tests` exercises account StartThread, hosted
publication and fresh clone. `cli/tests/adopt_hosted_publication.rs` runs local
Git adoption, hosted push and fresh Fetch, preserving original signatures,
attribution and claims. `hosted_clone_writes.rs` runs the CLI clone/write flows
against native witness-producing transports. Device F4 exercises actual relay
export, expired-set refresh, both exact retirement paths, missing/neighboring
proofs, revoked keys and unchanged original content.

## Final gates and guard-removal receipts

Final receipts in the PR record the tested SHA, command, exit status, test
summary and elapsed time. Local command logs use `/tmp/1968-final-*`; their
machine-readable index is `/tmp/1968-final-results.json`. Gates use a dedicated
Cargo target and fresh HEDDLE_HOME, with CLI units serialized as in CI:

1. Nightly rustfmt on touched Rust files, including Part 1b's tests.rs drift.
2. Workspace/all-target clippy and CI-feature clippy with warnings denied.
3. Workspace tests, serialized CLI units, CI-feature CLI integration and docs.
4. The Thread API feature, dependency-isolation, test and clippy matrix.
5. The complete hosted-client suite and CI/preview hosted clone/write tests.
6. Portable WASM builds, verifier WASM tests, npm build/pack, bigint boundaries,
   differential parity for every committed seed and forced-divergence rejection.
7. Native gate-family fail-then-pass controls and Part 2's verify-before-install
   and durable previous-witness-set guard proofs.
8. GitHub PR checks watched through completion, including Windows `installation`.

Guard proofs mutate an isolated final-source copy, require the named assertion
to fail, restore the exact original bytes, then run the same passing test.
Compilation failure is never a successful guard proof. The proof receipts are
indexed separately in `/tmp/1968-final-guard-results.json`.

## Applied surfaces

The verb and human/agent surfaces are exercised by CLI clone/write/publication
tests; there is no new flag or everyday verb. Git adoption keeps sley's native
conversion and original attribution. Wire fields come from the pinned API;
no view RPC or server-minted client root is added. Reverse states are exercised
by atomic rollback, ownership resolution, stale/revoked trust, proof refresh,
replay and exact-original export.
