# HYBRID Part 5: API alpha.32

This adoption starts from API PR #352 at
`2e238de4bf334fa6950af756d574c29a3948bca0`, version `=0.31.0-alpha.32`.
The final release pin and final-SHA gates remain pending the owner’s tag note.
Crate and npm versions stay at 0.28.8. `SYNC_MANDATORY_GATE` remains off.

The client consumes discovery’s typed Git storage estimate, uses widened
arithmetic for conversion headroom, and partitions selected refs into disjoint
sibling jobs. Each job has one positive result-byte total and an operation
count, bounded by the current advertised host limits. Independent prepared
jobs retain their proposals and destination token when a sibling activates.

Repository installation and retained-job recovery use the API’s
`verify_import_bundle_witnesses` composition helper. Independent owner lineage,
effective owner expiry, native records and policy signatures remain verified
by Heddle. The API derives historical owner times, renewal order, narrowing and
cumulative consumption. Durable install rejects recovery-only evidence;
recovery reads expose its typed outcome and optional authenticated snapshot.
Unwitnessed scheduled Commit and renewal tails do not become admitted history.
Every retained Thread projection of a job constrains its next admission; those
projections advance together so an older row cannot reset accepted history.
Retained root history crosses exactly one selected epoch, requiring an admitted
checkpoint before another root replacement.
Transferred observations select a policy accepted in their new owner’s phase;
the signer-based controls retain the earlier owner’s policy for earlier receipts.

Renew checks current control availability and returns authority-only Applied.
Retry selects the writer-only retry target and validates a fresh host attempt
UUID, independent of its request identity and known prior attempts. Source
custody and `ORIGINAL_WINDOW_ENDED` remain typed control refusals.
Retry freshness includes physical attempts in retained signed publications,
even when ordinary operation visibility omits those rows.

The WASM `remainingImportScope` binding exchanges exact protobuf bytes, including
large and `u64::MAX` totals. Verification’s time and TTL arguments keep their
checked bigint boundary. Native and generated npm bindings run the same expanded
differential corpus.

Run `python3 scripts/regenerate-hybrid-alpha32.py` to reproduce signed corpora
from the exact manifest pin in an isolated API archive. It runs the upstream
generators and continuity gate before copying fixtures, and uses Heddle’s signer
to regenerate claimed-owner expiry inputs. Frozen descriptor inventories and
JSON ordering are retained after comparing every generated record. Signatures
are never edited by hand.

Iteration proof at the PR head includes both workspace clippy configurations,
66 crypto tests, 105 capability-verifier tests, 29 repository trust tests,
38 Thread API HYBRID tests, hosted import lifecycle tests, sibling Commit
validation in both orders, and native/WASM differential seed 38322398.
Final receipts will record the final Heddle SHA, the complete requested gate
list, and GitHub checks including Windows.

`scripts/prove-native-witness-guards.py` also includes Part 2’s installation
ordering and durable witness checkpoint proofs. The latter removes both the
transaction checkpoint check and the API composition checkpoint check as a
family; either check alone continues to protect the receiver. Each mutation
must fail the named runtime assertion, restore exact sources, then pass.

Applied surfaces: existing import/native client paths and additive WASM bindings;
no new CLI verb or flag. API-owned wire fields and sley Git conversion remain the
shared boundaries. Recovery, cancellation, renewal, retry, custody loss, replay,
revocation and atomic rollback cover the reverse states.
