---
status: accepted
---

# State-bound evidence and landing delegation

Verification used for landing is an immutable signed attestation, not summary
metadata on a Thread. Each Verification Attestation identifies the exact source
State, versioned check definition, result, verifier, completion time, and
signature. Changing source, changing the check definition, or revoking the
verifier makes the attestation inapplicable without rewriting it.

Automatic landing authority comes only from a signed, versioned Landing
Delegation Policy issued by a principal. A policy bounds destination, path or
component scope, impact ceilings, required attestations and trusted verifiers,
validity, and whether fresh hosted authorization is required. Evaluation fails
closed and records the exact policy version and evidence set. A policy change
cannot authorize itself. Agent confidence and model identity may make a policy
more restrictive, but can never grant authority or satisfy missing evidence.

Thread Intent and acceptance criteria are versioned and attributed. A material
agent-authored amendment is a proposal until approved by the delegating
principal; delegated landing cannot use an unapproved amendment to weaken the
original goal or proof requirements.

When a client lacks the required human key, it may prepare a Human-Signable
Action containing the exact canonical request bytes, method, digest, scope,
expiry, and operation id. The proposal has no effect. A passkey-capable client,
currently often Tapestry, presents and signs those exact semantics; Weft verifies
and applies them without rewriting or signing on the human's behalf. Heddle and
Tapestry remain equal clients of the same Iroh RPC contract.

## Consequences

- `heddle-api` owns shared attestation, policy, intent-revision, evaluation, and
  pending-action wire types before Heddle, Weft, or Tapestry adapters consume
  them. There is no second view or web-only business RPC.
- Heddle mints client authority and can evaluate an unexpired policy offline
  when all required evidence is locally verifiable and the policy permits it.
  Weft independently accepts or rejects later publication.
- Weft verifies signatures, revocation, hosted freshness, and policy scope; it
  does not mint human authority or infer permission from agent metadata.
- Review defaults to the decision and unmet requirements, while raw evidence,
  traces, and optional session transcripts remain available for forensics.
- Harness/model assurance is displayed as unknown, claimed, observed, or
  attested. Heddle should observe and attest as broadly as integrations permit,
  but must not upgrade ambient detection into cryptographic proof.

## Considered options

A `tests_passed` boolean is convenient but does not say what ran, against which
State, or who made the claim. A confidence threshold appears automatable but
lets the actor seeking authority influence the authority decision. Hosted-only
approval would weaken local-first and offline work. State-bound attestations plus
bounded principal delegation keep proof, authority, and transport distinct.
