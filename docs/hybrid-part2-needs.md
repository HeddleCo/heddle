# HYBRID Part 2 core interfaces

Part 2 is [PR #1963](https://github.com/HeddleCo/heddle/pull/1963).
The three core interfaces requested here are implemented by
[Part 1b, PR #1966](https://github.com/HeddleCo/heddle/pull/1966).
[Part 3, PR #1965](https://github.com/HeddleCo/heddle/pull/1965) supplies the
production WASM verifier, moved policy verification and checked bigint boundary.
Dependencies enter this branch only through plain merges of `origin/integration`.

## Atomic source installation

`ThreadReplica::install_hybrid_import` takes a final
`before_commit: impl FnOnce(&mut InstallArtifacts) -> Result<()>` callback.
It keeps the receiver's trust serialization and SQLite transaction held while
Part 2 publishes the staged pack, owner pin and Spool identity through the file
journal. A late clock or disclosure rejection rolls back the files as well as
replicas, accepted evidence, job associations and trust high-water state.

Part 2 supplies an isolated object store. It never writes the destination pack
before authority verification. `StagedSource::install_hosted` consumes this seam;
ordinary locally owned source continues through its existing native installer.
Hosted publication retains the same bundle through actual-artifact validation
and supplies independently pinned ownership to the hosted installation path.

## Selected branch installation

The complete public bundle authenticates the logical job's history, including
sibling branches. Requested native originals select the exact causal closure to
install. A selected capture or a genesis-only clone does not create an unrelated
sibling replica. Every selected native original requires its own authenticated
admission; a valid signature alone is insufficient.

`HostedReplica` uses the same seam on receive and export. It retains unchanged
native originals and the full public bundle and rechecks durable receiver trust.
An unconfigured relay cannot export a stripped original. A missing bundle cannot
use a cached hosted admission, and durable N+1 invalidates retained N.

## Read-only trust preparation

`HostedTrust::snapshot` returns the receiver's selected root, root epoch, prior
verified witness set, clock floor and known job associations. Asynchronous proof
refresh uses this snapshot without holding a mutation transaction. Installation
then re-reads current trust under serialization; the snapshot grants no authority.

`Repository::pinned_owner_observation` reads the existing independently pinned
owner observation for device relay. Incoming evidence cannot create this pin.
`SelectedAuthority` resolves exact verified owner histories and typed revocations;
unknown identifiers or the wrong cancellation/key namespace fail closed.

## Wire and rollout

Part 2 pins API alpha.25. Configuration and source resolution are authenticated,
bounded discovery. Connected `github` and unconnected `public-git` are explicit
custody choices; a URL domain does not choose credentials. Unknown repository
format blocks Prepare/signing. Commit refreshes current source and support.
Renewal reads retained CAS before Prepare, verifies the predecessor as a recovery
handle, preserves authorized pins and freezes the complete Renew body for replay.

All new API rejection variants have explicit Rust error-code and TypeScript
union mappings. Preparation refusal retains its typed reason.
`hybrid::SYNC_MANDATORY_GATE` remains OFF. Capable optional streams negotiate
HYBRID explicitly; ordinary Sync constructors keep their existing rollout state.
