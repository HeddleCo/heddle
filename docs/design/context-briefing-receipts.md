# Context briefing receipts

`context --for-thread` records the exact intent versions and annotation revision
digests it supplied to the recipient lane. The next capture can attach that
record to its State. The attachment contains digests and visibility labels,
not annotation prose.

This is a **self-attested local record: it proves what this repository's key
signed, not independent delivery**. A process running as the repository owner
can read `.heddle/identity.toml` and sign a fabricated local supply claim.
The signature and capture binding still reject changes made by another
principal without that key, and reject an unmodified receipt replayed onto
another lane or State. A consumed nonce cannot be attached to a second State
in the repository.

Independent proof of delivery requires a hosted supply path with a separate
authority. That path is future work; a local receipt must not be presented as
hosted verification.

## Deletion retention

Deleting an annotation leaves a tombstone and its revision history in the
context tree. Deletion wins over a concurrent amendment, whose revision is
retained but does not become current guidance. Tombstones and revision
histories currently grow without a bound. Compacting them requires proof that
every replica that can later merge has acknowledged the deletion; otherwise
an old replica could resurrect it. The current protocol has no such
acknowledgment, so compaction must wait for that mechanism. Follow-up:
[HeddleCo/heddle#1866](https://github.com/HeddleCo/heddle/issues/1866).
