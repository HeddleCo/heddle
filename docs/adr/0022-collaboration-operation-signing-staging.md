# Collaboration operation signing staging

Collaboration operations should eventually be signed with the same identity direction as source states, but signatures are not required for the first local discussion-log slice. Slice 1 records attribution and stable operation IDs; later work can reuse automatic identity signing for tamper-evident collaboration operations and allow Weft policy to require signatures for hosted writes.

The v1 operation envelope should reserve explicit signature fields even when they are empty, so later signature requirements do not require a surprising schema reshaping. Weft may require signatures for hosted writes before the local-only slice requires them for local validity.

Adding signature requirements later does not invalidate old unsigned operations locally. They remain historical operations with unsigned provenance. Hosted policy may reject unsigned operations for new sync or sensitive records, but local materialization preserves old history.

Signatures are not attached later by mutating old operation bytes. If Heddle needs to upgrade trust in old unsigned history, it creates a collaboration attestation operation that cites the old operation and signs a claim about it.

Attestations affect trust and display overlays, and may affect hosted validity, but they do not alter the attested operation's discussion text, lifecycle effect, anchors, or visibility.

Competing attestations can coexist and materialize as trust-overlay disagreement. One attestation does not erase another unless a later revocation or attestation operation explicitly supersedes it under policy.

Operation signatures cover the canonical operation envelope, not sync metadata. Sync metadata varies by remote and over time; signing it would either break signatures or freeze remote policy state into local content.

When signatures are present, operation capability context is signed because it is part of the canonical operation envelope and provenance claim. Hosted acceptance context remains unsigned sync metadata.

**Status:** proposed

**Considered Options:** Requiring signatures immediately would strengthen integrity, but it would block the local storage and merge model on signing plumbing. Never signing collaboration operations would leave agent coordination weaker than source history, which conflicts with Heddle's provenance direction.
