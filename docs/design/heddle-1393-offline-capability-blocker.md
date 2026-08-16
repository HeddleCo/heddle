# Heddle #1393: offline capability cutover blocker

**Status:** Blocked as of 2026-08-16. No authorization fallback is safe.

The canonical authorization model remains Weft's
[`IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md`](https://github.com/HeddleCo/weft/blob/78415dc82070a10ba942f847eeeb20053e3859d5/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md).
This note records only the dependency and transport gap preventing Heddle from
applying that model; it does not redefine the model or reproduce its Biscuit
rules.

## Safety conclusion

Heddle cannot yet replace its TOML key lists with an owner-anchored offline
verifier. The shared Weft verifier is consumable, but the inputs required for
an owner-anchored ACCEPT decision do not reach Heddle:

- a clone does not receive or persist the owner's authorization keyring;
- redaction, purge, state-visibility, and state-attachment sidecars do not
  carry an owner-anchored capability; and
- Weft's current capability/envelope path is still server-rooted, so treating
  that bearer as owner authority would violate the target trust model.

Deleting the TOML gates while those inputs are absent would make every wire
operation deny. Keeping either TOML path beside a Biscuit verifier would create
the forbidden dual verifier. Learning an owner key from the first sidecar
would also change clone-time TOFU into attacker-controlled first-operation
trust. All three partial states are rejected.

## Verified dependency state

The shared verifier crate is not the blocker:

- `weft-capability-verifier` is `publish = false`, so it is unavailable from
  crates.io.
- A git dependency pinned to Weft commit
  `78415dc82070a10ba942f847eeeb20053e3859d5` resolves and compiles.
- The crate exposes the shared `rules.biscuit` through its normal verification
  entry points. Heddle must consume those entry points rather than copy the
  rules.

The isolated dependency check completed with:

```text
Checking weft-capability-verifier v0.1.1 (...#78415dc8)
Checking heddle-1393-verifier-check v0.0.0
Finished `dev` profile ...
```

## Missing live contract

Heddle currently depends on `heddle-api = 0.6.4`. Weft at the pinned commit
depends on `heddle-api = 0.8.0`. API 0.8.0 defines the intended
`CloneAuthorizationKeyring`, but
[`owner_authorization.proto`](https://github.com/HeddleCo/api/blob/ea82d8fcad9c7c0fccd4f5cf32904aa0c67fd9ef/proto/heddle/api/v1alpha1/owner_authorization.proto)
explicitly declares that file inert: no live request, response, bearer, or
trust surface imports it before the atomic Weft cutover.

The live sync contract still transfers only raw sidecar bytes:

- `RedactionTransfer` has `blob_hash` and `redactions_blob`;
- `StateVisibilityTransfer` has `state_id` and `state_visibility_blob`; and
- `StateAttachmentTransfer` has attachment identity, kind, and raw object
  bytes.

Neither API 0.6.4 nor 0.8.0 attaches a capability to those messages. Heddle's
folded clone bootstrap likewise contains only discussions and context; it has
no owner keyring field.

Weft's landed
[`#836` design](https://github.com/HeddleCo/weft/blob/78415dc82070a10ba942f847eeeb20053e3859d5/docs/design/836-owner-anchored-destructive-authz.md)
also records owner-signed grant-envelope minting as pending. Its current path
is signed by the deployment-wide server key. Therefore the shared verifier can
verify a supplied key and token, but Heddle has no valid owner key/token pair
to supply.

## Scope drift found in Heddle

The checkout has a third authorization list, `[purge].trusted_keys`, in
addition to the two stores named by #1393. It authorizes local purge signing
and wire purge evidence. A completed capability cutover must explicitly decide
its removal too; otherwise purge would retain a TOML authorization path after
redact and metadata moved to Biscuits.

The named metadata-supersession consumer is not a distinct live acceptance
path on this checkout. `verify_trusted_client_metadata_signature` has only the
state-visibility caller, while `put_state_attachment` rejects signature
attachments with `supersedes` as append-only before that verifier can run. The
live metadata-supersession carrier and acceptance seam therefore also need to
land before Heddle can produce the requested ACCEPT/DENY evidence for that
decision.

## Conditions to unblock implementation

The code cutover can proceed atomically after all of these are available:

1. Weft exclusively emits owner-anchored capabilities accepted by the shared
   verifier, with no server-rooted authorization fallback.
2. A live clone response carries the verified `CloneAuthorizationKeyring`, and
   Heddle persists its owner pin before accepting sidecars.
3. Every affected sidecar carries the complete offline capability material
   needed by a peer with no Weft connection.
4. A live metadata-supersession acceptance seam invokes the literal shared
   capability decision before persisting a superseding record.
5. The owner confirms whether `[purge].trusted_keys` is part of the retirement
   scope, as required to avoid a remaining purge verifier.

Once those contracts land, #1393 should remove all selected TOML surfaces and
mutation commands in the same change that wires the four offline decisions and
their ACCEPT/DENY matrix. Until then, deny-by-default means refusing to claim a
functional migration rather than inventing an authority source.
