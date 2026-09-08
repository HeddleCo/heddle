//! Extracted facts shared by native Weft and Worker WASM verification.
//!
//! After [`crate::authorize_at`] runs the authorizer, callers
//! need to read the principal subject, the session id (= revocation
//! id), the granted rights, and a handful of metadata facts (device,
//! agent provider, service-account id) to render `whoami` and
//! enforce per-RPC capability checks. This module holds the typed
//! shape the rest of the server consumes — every call site that
//! today reaches into `BiscuitFacts` reaches into [`BiscuitFacts`]
//! after the cutover.

use biscuit_auth::{
    Authorizer, Biscuit, PublicKey,
    builder::{Binary, BlockBuilder, Check, Op, Rule, Term, Unary},
};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

use super::BiscuitError;

#[cfg(test)]
mod authz_walk_regression_tests;

/// A [`BiscuitFacts`] carrying nothing but the rights and the staff marker —
/// the two inputs every capability gate reads. Shared by this module's tests
/// and [`authz_walk_regression_tests`] so both exercise the same construction.
#[cfg(test)]
fn facts_with(rights: Vec<Right>, is_staff: bool) -> BiscuitFacts {
    BiscuitFacts {
        sub: "test-user".into(),
        sid: "test-sid".into(),
        exp: 0,
        amr: Vec::new(),
        scope: String::new(),
        cnf: None,
        authority_device_pop_key_present: false,
        act: None,
        device_id: None,
        credential_id: None,
        agent_provider: None,
        agent_model: None,
        delegation_agent_id: None,
        service_account_id: None,
        rights,
        revocation_ids: Vec::new(),
        is_staff,
        staff_marker_present: is_staff,
        envelope_device_pubkey_hex: None,
        subject_kind_str: None,
        subject_str: None,
        subject_user_uuid_str: None,
        signup_bootstrap_email: None,
        bootstrap_session: false,
        limits_identity_disclosure: false,
        bounded_identity_scope: String::new(),
        request_signed_session: false,
        root_established: false,
    }
}

/// A single (kind, path, action) capability.
///
/// `kind` is free-form at the Biscuit boundary. Resource rights use the
/// canonical `"spool"` vocabulary; `"thread"` and `"context"` remain distinct
/// while their long-term mapping is unresolved. Actions currently include
/// `read`, `write`, `admin`, `merge`, and `approve`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Right {
    pub kind: String,
    pub path: String,
    pub action: String,
}

impl Right {
    pub fn new(
        kind: impl Into<String>,
        path: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            path: path.into(),
            action: action.into(),
        }
    }

    pub fn spool_admin(path: impl Into<String>) -> Self {
        Self::new("spool", path, "admin")
    }

    pub fn spool_read(path: impl Into<String>) -> Self {
        Self::new("spool", path, "read")
    }

    pub fn spool_write(path: impl Into<String>) -> Self {
        Self::new("spool", path, "write")
    }

    /// Thread-scoped grants. The `path` is the canonical
    /// `<repo_path>/threads/<thread_name>` produced by
    /// [`crate::resource::thread_path`].
    pub fn thread_read(path: impl Into<String>) -> Self {
        Self::new("thread", path, "read")
    }

    pub fn thread_write(path: impl Into<String>) -> Self {
        Self::new("thread", path, "write")
    }

    /// Right to merge a thread into another (gated by
    /// `thread_policies` when one exists for the target thread).
    /// Separate from `write` so branch protection can disallow
    /// merges without locking out commits.
    pub fn thread_merge(path: impl Into<String>) -> Self {
        Self::new("thread", path, "merge")
    }

    /// Right to formally sign off on a merge into this thread.
    /// Separate from `write` so reviewer-only roles (CI bots,
    /// security reviewers) can sign without push access.
    pub fn thread_approve(path: impl Into<String>) -> Self {
        Self::new("thread", path, "approve")
    }

    /// Context-stream grants. The `path` is the parent repo's path
    /// (1:1 with the repo); use
    /// [`crate::resource::context_path`] for clarity at
    /// call sites.
    pub fn context_read(path: impl Into<String>) -> Self {
        Self::new("context", path, "read")
    }

    pub fn context_write(path: impl Into<String>) -> Self {
        Self::new("context", path, "write")
    }
}

/// Actor metadata for delegation chains. Mirrors RFC 8693's `act`
/// claim shape: when set, the caller is *acting on behalf of* the
/// embedded subject (typically a parent agent for a sub-agent).
#[derive(Debug, Clone)]
pub struct ActorClaims {
    pub sub: String,
}

/// Typed view over the facts the Datalog engine ended up with after
/// applying the rule pack. Constructed via [`BiscuitFacts::extract`]
/// once `authorizer.authorize()` has succeeded.
///
/// Field names mirror the prior `BiscuitFacts` shape (`sub`, `sid`,
/// `exp`, `scope`, `amr`, `cnf`, `act`, ...) so that call sites
/// throughout the server compile unchanged after a one-line type
/// alias swap. The Biscuit-native concepts (rights, revocation_ids,
/// is_admin marker) live alongside.
#[derive(Debug, Clone)]
pub struct BiscuitFacts {
    /// Principal subject (username for human accounts, opaque id for
    /// service accounts). From the `user($sub)` fact.
    pub sub: String,
    /// Session id == Biscuit revocation id of the authority block.
    /// From the `session($sid)` fact.
    pub sid: String,
    /// Unix-seconds expiry. From `expires_at($t)`.
    pub exp: u64,
    /// Authentication methods (`"passkey"`, `"oauth:github"`, etc.).
    pub amr: Vec<String>,
    /// Pre-rendered scope string for the few legacy consumers that have not
    /// been rewritten yet. Format: space-separated `spool:{path}` tokens
    /// followed by one conservative `read`/`write`/`admin` capability ceiling,
    /// or `spool:* staff` when the token carries the operator marker. New code
    /// should query [`Self::is_staff`] / [`Self::has_right`] directly.
    pub scope: String,
    /// Effective PoP key hex string for device-bound tokens. Root-signed
    /// tokens start at the authority `device_pop_key($key)`; client-minted
    /// tokens start at the grant-envelope device key. Valid delegated
    /// blocks rotate it, while an exact terminal trusted presence block
    /// retains the prior leaf key.
    pub cnf: Option<String>,
    /// Whether `device_pop_key` was present in the authority block itself.
    /// The client-mint identity fence uses this instead of `cnf`, because
    /// `cnf` may legitimately come from the grant-envelope anchor.
    #[doc(hidden)]
    pub authority_device_pop_key_present: bool,
    /// `act` chain. Set when the token was minted on behalf of
    /// another subject (delegated agent flow).
    pub act: Option<ActorClaims>,
    /// CLI/agent device root id, when this session is bound to one.
    pub device_id: Option<String>,
    /// `issued_credentials.id` when the session was minted from a
    /// long-lived credential row (device, service account, agent).
    /// `None` for browser sessions, which don't have a credential
    /// row — they live in `sessions` only.
    pub credential_id: Option<String>,
    /// Agent provider / model (e.g. "anthropic" / "claude-opus") when
    /// the bearer is an agent credential. Surfaced in `whoami` and
    /// in audit logs.
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    /// Deepest `agent($id)` label carried by a verified post-authority
    /// delegation block. `None` for an undelegated authority-only credential.
    pub delegation_agent_id: Option<String>,
    /// `service_accounts.id` when the bearer is a service-account
    /// credential.
    pub service_account_id: Option<String>,
    /// All rights, *including* those derived by the rule pack
    /// (admin → write → read on a single resource). Cross-resource
    /// inheritance (parent spool → descendant spool, staff) is
    /// handled by [`Self::can_read`] / [`Self::can_write`] /
    /// [`Self::is_admin_on`].
    pub rights: Vec<Right>,
    /// Hex-encoded revocation identifiers, one per block. The hosted
    /// layer pushes these through the in-memory revocation cache;
    /// caching them here means the cache check doesn't need to
    /// re-parse the token.
    pub revocation_ids: Vec<String>,
    /// `staff(true)` marker presence — the token carries the operator
    /// (Heddle staff) capability. Cached so [`Self::is_staff`] is O(1).
    /// Distinct from `right(*, *, "admin")` which is a customer-grant
    /// admin role on a resource.
    is_staff: bool,
    /// Whether the token's authority block literally contained a
    /// `staff(true)` fact (HeddleCo/weft#102 Codex r1: P1 #1). Distinct
    /// from [`Self::is_staff`], which trusts only the authority-block marker.
    /// Used by
    /// [`crate::verify_client_minted_at_with_resource`] to reject client-minted tokens
    /// that try to smuggle an operator marker past the envelope's
    /// `right(...)`-only subset check — the literal staff fact is not
    /// constrained by the envelope's `rights` field today, so the
    /// only safe contract is to forbid it on the client-mint path.
    pub staff_marker_present: bool,
    /// Hex-encoded Ed25519 device public key used as the stable
    /// rate-limit / revocation identity. Set to the envelope-bound
    /// device key on the envelope verify path, or to the registered
    /// biscuit-root public key that matched on [`crate::verify_at_with_resource`] /
    /// registered-root verify. The token's own `session()` fact is
    /// client-chosen and therefore not safe to use as a unique
    /// abuse-rate-limit bucket or revocation handle.
    pub envelope_device_pubkey_hex: Option<String>,
    /// The `subject_kind($s)` fact value as it appeared in the
    /// authority block (`"user"` / `"anon"` per
    /// `docs/auth/anon-biscuit-spike.md` §3.2). `None` on legacy
    /// biscuits minted before the substrate; the verifier layer
    /// short-circuits `VerifiedBiscuit` injection in that case and
    /// the existing per-pubkey `RateLimitIdentity` injection still
    /// runs unchanged. Unknown values (something other than the two
    /// listed) populate the field with the raw string so a future
    /// kind is observable in logs without a `BiscuitFacts` schema
    /// rev; the verifier rejects them at `Subject` construction.
    pub subject_kind_str: Option<String>,
    /// The raw `subject($s)` fact value. Parsed into a `Uuid` by the
    /// verifier layer when [`Self::subject_kind_str`] is `"anon"`;
    /// surfaced as a string here so the parser lives at the verifier
    /// boundary rather than in extraction. The `"user"` leg reads
    /// [`Self::subject_user_uuid_str`] from a separate fact instead
    /// (Codex r6 P1 cid 3295178444).
    pub subject_str: Option<String>,
    /// The raw `subject_user_uuid($s)` fact value. Parsed into a
    /// `Uuid` by the verifier layer when [`Self::subject_kind_str`]
    /// is `"user"`. Separate from [`Self::subject_str`] (Anon path)
    /// so the envelope-binding `user($username)` fact and the
    /// verifier's `Subject::User` UUID don't have to be the same
    /// string. The client-minted verify path forbids the
    /// fact via [`crate::client_minted_identity_violation`]
    /// because the envelope can't constrain it; server-minted tokens
    /// (legacy verify entry) are trusted by construction.
    pub subject_user_uuid_str: Option<String>,
    /// Legacy verified-address fact consumed only by BindSignupInviteEmail.
    /// Self-sovereign signup authority is request-carried and never sourced
    /// from this field.
    pub signup_bootstrap_email: Option<String>,
    /// `bootstrap_session(true)` marker before recovery setup (weft#182).
    pub bootstrap_session: bool,
    /// Whether ObserveIdentity must return the least-privileged self-introspection
    /// shape because a route ceiling grants self-introspection as an exception.
    /// Delegating a proof key alone preserves the parent's account authority.
    ///
    /// `pub` (was `pub(crate)`) so the ObserveIdentity handler in `weft-server` can read
    /// it across the weft#719 phase 3 crate boundary. Set only by
    /// [`BiscuitFacts`]'s own extraction — there is no public constructor.
    pub limits_identity_disclosure: bool,
    /// Authority scope intersected with every resource ceiling that exempts
    /// the ObserveIdentity route. Used only when `limits_identity_disclosure` is true.
    ///
    /// `pub` for the same cross-crate reason as `limits_identity_disclosure`.
    pub bounded_identity_scope: String,
    /// Authority-scoped `request_signed_session(true)` marker (web
    /// PoP-binding Option C). When set, this `cnf`-bound browser session
    /// enforces per-request PoP through the request-signature middleware
    /// (`x-heddle-sig-*`), so `token_id` exempts it from the legacy
    /// inline-proof gate (`x-heddle-proof-*`). Read under `trusting
    /// authority`, so an attenuated block cannot forge it.
    pub request_signed_session: bool,
    /// Authority-scoped `root_established(true)` ceremony marker. Marks a
    /// credential that completed an independent-root ceremony (self-rooted
    /// passkey/device or server-rooted bootstrap).
    /// Independent-root RPCs require this fact in addition to a single
    /// Biscuit block. Agent-rooted mints never emit it.
    pub root_established: bool,
}

// ---- Fact extraction -----------------------------------------------
//
// Extraction is a HYBRID of two mechanisms, split by whether a fact is
// authorization-load-bearing (weft#513):
//
//   * Every identity / capability / binding fact an authz decision
//     consumes is read via an authority-scoped `Authorizer::query(...
//     trusting authority)`. The `trusting authority` origin filter is
//     what makes the value un-forgeable: an appended attenuation /
//     third-party block cannot get the fact into the result set, so a
//     token holder cannot self-grant capability or impersonate by
//     appending a block offline. This is the whole point of the fix.
//
//   * The handful of NON-authz facts (audit / `whoami` display
//     metadata, plus the two any-block markers the client-mint fence
//     reads) are still walked out of `Authorizer::dump()`, which
//     returns every fact post-authorize in one pass. Each is justified
//     inline in `extract`.
//
// An earlier version harvested EVERYTHING from the dump walk to avoid
// re-running the Datalog engine per predicate — but `dump()` iterates a
// `HashMap<Origin, HashSet<Fact>>` in randomized (`RandomState`) order,
// so a `.is_none()` "first-wins" guard was a per-request coin flip
// between the authority block and an attacker block: probabilistically
// forgeable. The per-query Datalog cost the walk was avoiding is a
// non-issue now that `authorizer_limits()` sets `max_iterations: 100` /
// `max_time: 1h` (see [`crate::authorize_atr_limits`]), so the
// ~dozen scoped queries below evaluate well within budget.

impl BiscuitFacts {
    /// Pull out every fact the rest of the server needs. Run AFTER
    /// `authorizer.authorize()` has succeeded so derived rights are
    /// visible too.
    pub fn extract(
        authorizer: &mut Authorizer,
        biscuit: &Biscuit,
        initial_pop_key_hex: Option<&str>,
        trusted_presence_signers: &[PublicKey],
    ) -> Result<Self, BiscuitError> {
        let (facts, _rules, checks, _policies) = authorizer.dump();

        // --- Audit-only / any-block facts (weft#513) -----------------
        //
        // `Authorizer::dump()` returns facts from EVERY block (authority
        // + attenuation + third-party) in randomized (`RandomState`)
        // order, so a `.is_none()` "first-wins" guard on this walk is a
        // per-request coin flip between the authority block and an
        // attacker-appended block — UNSOUND for anything an authz
        // decision consumes. Therefore EVERY authorization-load-bearing
        // identity / capability / binding fact is sourced from an
        // authority-scoped `query(... trusting authority)` BELOW, never
        // from this walk. Only facts that are (a) NOT authz-load-bearing
        // AND (b) safe to read from any block remain here; each is
        // justified inline.
        let mut amr: Vec<String> = Vec::new();
        // Any-block presence of a `staff(...)` fact. This drives ONLY
        // the client-mint identity fence (`staff_marker_present`),
        // which must reject a staff marker appearing in ANY block of a
        // client-minted token. The authorization-level `is_staff` is
        // computed separately from an authority-scoped query below so a
        // forged appended `staff(...)` cannot grant operator power
        // (weft#513).
        let mut staff_marker_any_block: bool = false;
        // Any-block presence of a `bootstrap_session(...)` marker. Like
        // `staff_marker_any_block`, this feeds ONLY the client-mint
        // identity fence (`client_minted_identity_violation`), which must
        // reject the marker in ANY block of a client-minted token. NO
        // server-path authorization reads `facts.bootstrap_session` —
        // recovery-scope gating runs off the authority-scoped
        // `rights`/scope (see `access_scope_bootstrap_session`), not this
        // marker — so any-block detection is both safe and exactly what
        // the fence wants (weft#513).
        let mut bootstrap_session: bool = false;

        for fact in &facts {
            let pred = &fact.predicate;
            let name = pred.name.as_str();
            let terms = &pred.terms;
            match name {
                // `amr`: authentication-method labels ("passkey",
                // "oauth:github", ...). Audit / `whoami` DISPLAY metadata
                // only — no server branch gates on it (verified weft#513).
                // Accumulated across blocks by design so the whoami view
                // is complete; an appended `amr` can only add a spurious
                // display label, never move an authorization decision.
                "amr" => {
                    if let Some(s) = first_string(terms) {
                        amr.push(s);
                    }
                }
                "staff" => staff_marker_any_block = true,
                "bootstrap_session" => bootstrap_session = true,
                _ => {}
            }
        }

        // SECURITY (weft#513): authorization-granting facts MUST be
        // trusted ONLY from the root-signed authority block (block 0).
        // Biscuit attenuation lets any token holder append blocks
        // offline, and `authorizer.dump()` (walked above) returns facts
        // from EVERY block — so harvesting `right`/`staff` from the dump
        // let an appended block forge any capability
        // (spool:*:admin, purge). `Authorizer::query` with a
        // `trusting authority` scope restricts the rule body to facts
        // whose origin is the authority block or the authorizer itself;
        // appended/attenuation/third-party blocks are excluded by
        // construction. Rule-pack derivations (admin ⇒ write ⇒
        // read/redact) that chain off an authority-block grant keep an
        // authority-inclusive origin and are still returned, so
        // legitimate auth is unaffected; a derivation off an appended
        // fact carries that block's origin and is dropped. This is the
        // defense documented — but until now not implemented — in
        // `rules.biscuit`.
        let mut rights = authority_rights(authorizer)?;

        // SECURITY (weft#513): `subject_user_uuid` and `delegated_from`
        // are authorization-load-bearing identity facts, so — like
        // `right`/`staff` above — they MUST be trusted ONLY from the
        // root-signed authority block. `subject_user_uuid` feeds
        // `resolve_subject_user_id` → role resolution (an appended
        // `subject_user_uuid($victim)` would let an anon/legacy token
        // resolve roles AS the victim); `delegated_from` feeds
        // `ActorClaims` (an appended `delegated_from($victim)` would
        // forge the delegation actor for presence/credential rotation).
        // Bind both via the same authority-scoped mechanism as
        // `authority_rights` so appended/attenuation/third-party blocks
        // are excluded by construction.
        let subject_user_uuid_str = authority_string_fact(authorizer, "subject_user_uuid")?;
        let act_sub = authority_string_fact(authorizer, "delegated_from")?;

        // SECURITY (weft#513): `service_account` gates
        // `credential_belongs_to_caller` (the RevokeCredential authz
        // path), so it is authority-scoped like the identity facts above
        // — an appended `service_account($victim)` cannot make an
        // attacker's token "belong to" a victim service account.
        // `signup_bootstrap_email` remains authorization-load-bearing for the
        // legacy invite-email binding route, so it stays authority-scoped.
        let service_account_id = authority_string_fact(authorizer, "service_account")?;
        let signup_bootstrap_email = authority_string_fact(authorizer, "signup_bootstrap_email")?;

        // SECURITY (weft#513 round 3): close the CLASS. Every remaining
        // identity / capability / binding fact an authorization path
        // consumes is sourced from the authority block via the same
        // `trusting authority` origin filter — no authz-load-bearing fact
        // is left on the randomized-order dump walk.
        //   - `user` → `sub`: THE principal. Feeds
        //     `credential_belongs_to_caller` (`claims.sub == record.subject`)
        //     and `resolve_user_id_for_subject(&claims.sub)` role
        //     resolution — the highest-value impersonation target.
        //   - `session` → `sid`: revocation key + `RateLimitIdentity`
        //     bucket. A forged `sid` would evade revocation / rate limits.
        //   - `device` / `device_pop_key` / `credential_id`: PoP
        //     device-binding key + revocation-cache keys
        //     (`contains_pubkey`, the PoP proof in `token_id`).
        //   - `subject_kind` / `subject`: drive `Subject::{User,Anon}`
        //     construction in `derive_verified_biscuit` — the verified
        //     identity stamped on every request.
        let sub = authority_string_fact(authorizer, "user")?;
        let sid = authority_string_fact(authorizer, "session")?;
        let device_id = authority_string_fact(authorizer, "device")?;
        let authority_cnf = authority_string_fact(authorizer, "device_pop_key")?;
        if authority_pop_delegation_present(authorizer)? {
            return Err(BiscuitError::Invalid(
                "pop_delegation is forbidden in the authority block".to_string(),
            ));
        }
        let credential_id = authority_string_fact(authorizer, "credential_id")?;
        // Agent attribution is durable provenance, so it is identity metadata
        // rather than display-only metadata. Trust only the server-minted
        // authority block: an offline-appended block must not forge or
        // override the provider/model stamped onto content.
        let agent_provider = authority_string_fact(authorizer, "agent_provider")?;
        let agent_model = authority_string_fact(authorizer, "agent_model")?;
        let subject_kind_str = authority_string_fact(authorizer, "subject_kind")?;
        let subject_str = authority_string_fact(authorizer, "subject")?;
        // `expires_at` is a Date fact (not a string). Its PRIMARY expiry
        // gate is the baked-in `check if time($now) < <exp>` clause in the
        // authority block — unforgeable, since an appended block can only
        // ADD checks, never remove them. But `facts.exp` also flows into
        // `VerifiedBiscuit.expires_at` → the anon-promotion durable-profile
        // lifetime (`anon_promotion_context_from_request`), so a forged
        // appended `expires_at($far_future)` winning the randomized
        // first-wins dump race would extend a promoted profile's life.
        // Authority-scope it too, via the Date→`SystemTime` term
        // conversion (weft#513).
        let expires_at_secs = authority_date_secs(authorizer, "expires_at")?;

        // Deduplicate rights — the rule pack derives `read` from
        // `write` and `admin`, so the same (kind, path) can appear
        // multiple times with different actions. We want each unique
        // tuple once for clean traversal.
        rights.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then(a.path.cmp(&b.path))
                .then(a.action.cmp(&b.action))
        });
        rights.dedup();

        let sub =
            sub.ok_or_else(|| BiscuitError::Invalid("token missing user() fact".to_string()))?;
        let sid =
            sid.ok_or_else(|| BiscuitError::Invalid("token missing session() fact".to_string()))?;

        let revocation_ids = super::revocation_ids(biscuit);
        let authority_device_pop_key_present = authority_cnf.is_some();
        // Web PoP Option C: authority-scoped so an attenuated block can't
        // forge the marker to escape the legacy inline-PoP gate.
        let request_signed_session = authority_request_signed_session(authorizer)?;
        // Independent-root ceremony marker: authority-scoped so an
        // attenuated block cannot forge it to pass require_independent_root.
        let root_established = authority_root_established(authorizer)?;
        let delegation = verified_delegation_chain(
            authority_cnf,
            initial_pop_key_hex,
            trusted_presence_signers,
            biscuit,
        )?;
        // Operator (`is_staff`) is an authorization decision: it must
        // derive ONLY from authority-block facts. The literal
        // `staff(...)` marker is queried under `trusting authority`.
        // Customer-resource admin rights never imply operator capability.
        // `staff_marker_any_block` is NOT consulted here — it exists only for
        // the client-mint fence.
        let is_staff = authority_staff_marker(authorizer)?;
        let identity_observation_bounds = identity_observation_bounds(&checks, &rights, is_staff);

        // Render a scope `scope` string for the legacy consumers
        // that haven't been swung over to the BiscuitFacts helpers
        // yet. Tokens are space-separated; staff always carry the
        // bare "staff" sentinel that `is_staff_scope` detects.
        let scope = render_scope_string(&rights, is_staff);

        let exp = if expires_at_secs > 0 {
            expires_at_secs as u64
        } else {
            0
        };

        Ok(Self {
            sub,
            sid,
            exp,
            amr,
            scope,
            cnf: delegation.effective_pop_key,
            authority_device_pop_key_present,
            act: act_sub.map(|sub| ActorClaims { sub }),
            device_id,
            credential_id,
            agent_provider,
            agent_model,
            delegation_agent_id: delegation.agent_id,
            service_account_id,
            rights,
            revocation_ids,
            is_staff,
            staff_marker_present: staff_marker_any_block,
            envelope_device_pubkey_hex: None,
            subject_kind_str,
            subject_str,
            subject_user_uuid_str,
            signup_bootstrap_email,
            bootstrap_session,
            limits_identity_disclosure: identity_observation_bounds.limited,
            bounded_identity_scope: identity_observation_bounds.scope,
            request_signed_session,
            root_established,
        })
    }

    /// Is the caller a Heddle operator? True only after a registered
    /// staff grant is overlaid from Postgres. Authority-block
    /// `staff(true)` is recorded on [`Self::staff_marker_present`] and
    /// is never sufficient on its own — clients mint every biscuit.
    /// Distinct from [`Self::is_admin_on`], which checks for the
    /// customer-facing `Role::Admin` grant on a specific resource.
    pub fn is_staff(&self) -> bool {
        self.is_staff
    }

    /// Clear any authority-block staff marker so a client-minted token
    /// cannot self-assert operator standing. Hosted verify then calls
    /// [`Self::apply_staff_grant`] when the matching root key's owner
    /// holds a staff-scoped issued credential.
    pub fn discard_authority_staff_marker(&mut self) {
        self.is_staff = false;
    }

    /// Overlay operator standing from a registered staff grant.
    pub fn apply_staff_grant(&mut self) {
        self.is_staff = true;
    }

    /// Bind the complete user substrate from a verifier-chosen key owner.
    ///
    /// Hosted verify stamps this after signature check so subject construction
    /// and grant resolution cannot follow client-minted `subject_kind` /
    /// `subject_user_uuid` / `user(...)` facts.
    pub fn bind_user_subject(&mut self, user_id: uuid::Uuid) {
        self.subject_kind_str = Some("user".to_string());
        self.subject_user_uuid_str = Some(user_id.to_string());
    }

    /// The verified immutable user UUID carried by the `subject_user_uuid`
    /// fact, parsed from [`Self::subject_user_uuid_str`]. This is the SECURITY
    /// KEY for grant resolution (HeddleCo/weft#300). After hosted verify,
    /// the value is the verifying root's owner (not a client-asserted UUID).
    /// Returns `None` for tokens that carry no substrate UUID fact (e.g.
    /// legacy / anon), whose callers must fall back to a DB handle→id lookup
    /// or fail closed.
    pub fn subject_user_id(&self) -> Option<uuid::Uuid> {
        self.subject_user_uuid_str
            .as_deref()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
    }

    /// Does the caller hold the LITERAL right (post-rule-pack)? Does
    /// not consider spool inheritance — for that, use
    /// [`Self::can_read`] / [`Self::can_write`] / [`Self::is_admin_on`],
    /// which walk parent spools.
    pub fn has_right(&self, kind: &str, path: &str, action: &str) -> bool {
        self.rights.iter().any(|r| {
            r.kind.as_str() == kind && r.path.as_str() == path && r.action.as_str() == action
        })
    }

    /// True if the caller has at least `read` on the given resource,
    /// considering spool inheritance and staff. A caller
    /// who is admin on `org/acme` can read `org/acme/heddle`.
    pub fn can_read(&self, kind: &str, path: &str) -> bool {
        if self.is_staff {
            return true;
        }
        self.has_action_through_inheritance(kind, path, "read")
            || self.has_action_through_inheritance(kind, path, "write")
            || self.has_action_through_inheritance(kind, path, "admin")
    }

    /// Whether live-grant authority may be exercised on `spool_path`.
    ///
    /// Full sessions (`spool:*`, no concrete spool rights) resolve access
    /// from hosted grants. A device or service credential that declared an
    /// explicit `spool:{path}` binding is restricted to that path and its
    /// descendants even when the subject holds broader live grants.
    pub fn covers_declared_spool(&self, spool_path: &str) -> bool {
        if self.is_staff {
            return true;
        }
        if !self.rights.iter().any(|right| right.kind == "spool") {
            return true;
        }
        self.can_read("spool", spool_path)
    }

    /// True if the caller has at least `write` on the given resource,
    /// considering spool inheritance and staff.
    pub fn can_write(&self, kind: &str, path: &str) -> bool {
        if self.is_staff {
            return true;
        }
        self.has_action_through_inheritance(kind, path, "write")
            || self.has_action_through_inheritance(kind, path, "admin")
    }

    /// True if the caller is admin on the given resource, considering spool
    /// inheritance and staff.
    pub fn is_admin_on(&self, kind: &str, path: &str) -> bool {
        if self.is_staff {
            return true;
        }
        self.has_action_through_inheritance(kind, path, "admin")
    }

    /// Check whether any of the caller's rights at `action` covers
    /// `(kind, path)`, including via the resource hierarchy walked
    /// by [`crate::resource::walk_to_root`].
    ///
    /// The walk is uniform across resource kinds:
    ///
    ///   1. Direct match: a literal `right(kind, path, action)` fact.
    ///   2. The same check at each ancestor, nearest first, up to root.
    ///
    /// Adding a new resource kind (thread, context, ...) is then a
    /// single-line change in `resolve_parent` rather than a cascade
    /// of conditionals here. Biscuit's Datalog couldn't express the
    /// walk directly because the head variables wouldn't be bound by
    /// a body predicate; we resolve it in Rust at the call site.
    /// Cost: O(rights × tree-depth), and typical tokens carry fewer
    /// than a dozen rights with paths fewer than 5 segments deep.
    fn has_action_through_inheritance(&self, kind: &str, path: &str, action: &str) -> bool {
        covered_through_inheritance(kind, path, |candidate_kind, candidate_path| {
            self.has_right(candidate_kind, candidate_path, action)
        })
    }
}

/// Does `predicate` hold at `(kind, path)` or at any of its ancestors?
///
/// The ONE inheritance walk (weft#1130). Both callers that need it —
/// [`BiscuitFacts::has_action_through_inheritance`], which gates real reads and
/// writes, and [`authority_can_read`], which bounds a limited ObserveIdentity scope —
/// route through here, so the two can no longer drift apart. They previously
/// hand-rolled the same loop with different quantifier nesting; the answers
/// agreed, but nothing held them to it.
///
/// An unknown `kind` (a resource type not in [`ResourceKind`]) has no defined
/// chain, so it gets the literal check only — never inheritance from a path
/// that merely looks like an ancestor.
fn covered_through_inheritance(
    kind: &str,
    path: &str,
    predicate: impl Fn(&str, &str) -> bool,
) -> bool {
    if predicate(kind, path) {
        return true;
    }
    let Some(start) = crate::resource::ResourceKind::parse(kind) else {
        return false;
    };
    crate::resource::walk_to_root(start, path)
        .any(|(ancestor_kind, ancestor_path)| predicate(ancestor_kind.as_str(), ancestor_path))
}

/// Require and apply exactly one signed key transition in each post-authority
/// block. Reading each block directly keeps the transition attached to the
/// chain edge it authenticates instead of trusting the unordered authorizer
/// fact dump.
struct VerifiedDelegationChain {
    effective_pop_key: Option<String>,
    agent_id: Option<String>,
}

fn verified_delegation_chain(
    authority_key_hex: Option<String>,
    initial_pop_key_hex: Option<&str>,
    trusted_presence_signers: &[PublicKey],
    biscuit: &Biscuit,
) -> Result<VerifiedDelegationChain, BiscuitError> {
    let authority_key_present = authority_key_hex.is_some();
    let mut effective_key = authority_key_hex.or_else(|| initial_pop_key_hex.map(str::to_owned));
    let mut agent_id = None;

    // Session classes are deliberately disjoint:
    //   * root: authority-only, optionally anchored by device_pop_key;
    //   * grant envelope: authority-only, anchored by initial_pop_key_hex;
    //   * delegated: one or more post-authority blocks.
    // Only the delegated class enters the per-block pop_delegation validator.
    // In particular, a client-minted grant-envelope session has no child block
    // in which a pop_delegation fact could exist; its effective key is the
    // envelope anchor supplied above.
    let grant_envelope_session =
        biscuit.block_count() == 1 && !authority_key_present && initial_pop_key_hex.is_some();
    if grant_envelope_session {
        return Ok(VerifiedDelegationChain {
            effective_pop_key: effective_key,
            agent_id,
        });
    }
    let delegated_session = biscuit.block_count() > 1;
    if !delegated_session {
        return Ok(VerifiedDelegationChain {
            effective_pop_key: effective_key,
            agent_id,
        });
    }

    let revocation_ids = biscuit.revocation_identifiers();
    let external_keys = biscuit.external_public_keys();
    for index in 1..biscuit.block_count() {
        let block = parse_block(biscuit, index)?;
        let external_key = external_keys.get(index).copied().flatten();
        let presence_marker_present = block
            .facts
            .iter()
            .any(|fact| fact.predicate.name == super::PRESENCE_ATTENUATION_FACT);
        if let Some(external_key) = external_key.filter(|_| presence_marker_present) {
            if index + 1 != biscuit.block_count() {
                return Err(BiscuitError::Invalid(
                    "the trusted presence attenuation block must be terminal".to_string(),
                ));
            }
            if !trusted_presence_signers.contains(&external_key) {
                return Err(BiscuitError::Invalid(
                    "third-party attenuation block was not signed by a trusted Weft key"
                        .to_string(),
                ));
            }
            validate_presence_attenuation_block(&block, index)?;
            continue;
        }
        if presence_marker_present {
            return Err(BiscuitError::Invalid(
                "presence attenuation marker requires a trusted third-party signature".to_string(),
            ));
        }
        if block
            .facts
            .iter()
            .any(|fact| fact.predicate.name == crate::edge::EDGE_ATTENUATION_FACT)
        {
            // A first-party narrowing block. It carries no proof-of-possession
            // delegation because it delegates nothing — it only adds checks,
            // which the verifier runs from every block, so it can only reduce
            // the authority it was appended to.
            validate_edge_attenuation_block(&block, index)?;
            continue;
        }
        let delegations = block
            .facts
            .iter()
            .filter(|fact| fact.predicate.name == "pop_delegation")
            .collect::<Vec<_>>();
        let [delegation] = delegations.as_slice() else {
            return Err(BiscuitError::Invalid(format!(
                "attenuation block {index} must contain exactly one pop_delegation fact"
            )));
        };
        let [
            Term::Str(parent_hex),
            Term::Str(child_hex),
            Term::Str(signature_hex),
        ] = delegation.predicate.terms.as_slice()
        else {
            return Err(BiscuitError::Invalid(format!(
                "attenuation block {index} pop_delegation must contain three string terms"
            )));
        };
        let expected_parent = revocation_ids.get(index - 1).ok_or_else(|| {
            BiscuitError::Invalid(format!(
                "attenuation block {index} has no preceding revocation identifier"
            ))
        })?;
        let parent = decode_fixed_hex(
            parent_hex,
            expected_parent.len(),
            "pop_delegation parent revocation identifier",
        )?;
        if parent.as_slice() != *expected_parent {
            return Err(BiscuitError::Invalid(format!(
                "attenuation block {index} pop_delegation must reference its immediately preceding block"
            )));
        }
        let child = decode_fixed_hex(child_hex, 32, "pop_delegation child public key")?;
        let signature = decode_fixed_hex(signature_hex, 64, "pop_delegation signature")?;
        let parent_key_hex = effective_key.as_deref().ok_or_else(|| {
            BiscuitError::Invalid(
                "post-authority delegation requires an authority or grant-envelope proof-of-possession anchor"
                    .to_string(),
            )
        })?;
        let parent_key = decode_fixed_hex(
            parent_key_hex,
            32,
            "effective parent proof-of-possession key",
        )?;
        let parent_key = VerifyingKey::from_bytes(
            parent_key
                .as_slice()
                .try_into()
                .expect("fixed-length PoP parent key"),
        )
        .map_err(|_| {
            BiscuitError::Invalid("effective parent proof-of-possession key is invalid".to_string())
        })?;
        let signature = Signature::from_bytes(
            signature
                .as_slice()
                .try_into()
                .expect("fixed-length PoP signature"),
        );
        parent_key
            .verify(&super::pop_delegation_payload(&parent, &child), &signature)
            .map_err(|_| {
                BiscuitError::Invalid(
                    "pop_delegation signature was not made by the effective parent key".to_string(),
                )
            })?;
        effective_key = Some(hex::encode(child));

        let agent_labels = block
            .facts
            .iter()
            .filter(|fact| fact.predicate.name == "agent")
            .collect::<Vec<_>>();
        match agent_labels.as_slice() {
            [] => {}
            [label] => match label.predicate.terms.as_slice() {
                [Term::Str(value)] if !value.is_empty() => agent_id = Some(value.clone()),
                _ => {
                    return Err(BiscuitError::Invalid(format!(
                        "attenuation block {index} has a malformed agent fact"
                    )));
                }
            },
            _ => {
                return Err(BiscuitError::Invalid(format!(
                    "attenuation block {index} must contain at most one agent fact"
                )));
            }
        }
    }
    Ok(VerifiedDelegationChain {
        effective_pop_key: effective_key,
        agent_id,
    })
}

fn parse_block(biscuit: &Biscuit, index: usize) -> Result<BlockBuilder, BiscuitError> {
    let source = biscuit.print_block_source(index).map_err(|error| {
        BiscuitError::Invalid(format!("cannot read Biscuit block {index}: {error}"))
    })?;
    BlockBuilder::new().code(&source).map_err(|error| {
        BiscuitError::Invalid(format!("cannot parse Biscuit block {index}: {error}"))
    })
}

/// A first-party edge-serving attenuation block must add checks and nothing
/// else.
///
/// Permitting an unsigned appended block is only safe while it cannot *assert*
/// anything: a fact is visible to every other block's checks, so one smuggled
/// in here could satisfy a check it was never meant to. Requiring the block to
/// be exactly one marker fact plus at least one check keeps "attenuation only
/// narrows" true of this shape, not merely intended.
fn validate_edge_attenuation_block(block: &BlockBuilder, index: usize) -> Result<(), BiscuitError> {
    let non_marker_facts = block
        .facts
        .iter()
        .filter(|fact| fact.predicate.name != crate::edge::EDGE_ATTENUATION_FACT)
        .count();
    if block.facts.len() != 1 || non_marker_facts != 0 {
        return Err(BiscuitError::Invalid(format!(
            "edge serving attenuation block {index} must assert nothing beyond its marker"
        )));
    }
    if !block.rules.is_empty() {
        return Err(BiscuitError::Invalid(format!(
            "edge serving attenuation block {index} must not derive facts"
        )));
    }
    if block.checks.is_empty() {
        return Err(BiscuitError::Invalid(format!(
            "edge serving attenuation block {index} narrows nothing"
        )));
    }
    Ok(())
}

/// Reject a token that asserts a predicate the verifier reserves for describing
/// the in-flight request.
///
/// Called only on paths that inject request facts. A bearer who could assert
/// [`crate::edge::EDGE_REQUEST_PREDICATE`] themselves would satisfy the scope
/// check with parameters of their own choosing rather than the ones the edge
/// was actually asked for.
pub(crate) fn reject_reserved_request_facts(biscuit: &Biscuit) -> Result<(), BiscuitError> {
    for index in 0..biscuit.block_count() {
        let block = parse_block(biscuit, index)?;
        let asserts_reserved = block
            .facts
            .iter()
            .any(|fact| fact.predicate.name == crate::edge::EDGE_REQUEST_PREDICATE)
            || block
                .rules
                .iter()
                .any(|rule| rule.head.name == crate::edge::EDGE_REQUEST_PREDICATE);
        if asserts_reserved {
            return Err(BiscuitError::Invalid(format!(
                "block {index} asserts the reserved request predicate {}",
                crate::edge::EDGE_REQUEST_PREDICATE
            )));
        }
    }
    Ok(())
}

fn validate_presence_attenuation_block(
    block: &BlockBuilder,
    index: usize,
) -> Result<(), BiscuitError> {
    let markers = block
        .facts
        .iter()
        .filter(|fact| fact.predicate.name == super::PRESENCE_ATTENUATION_FACT)
        .collect::<Vec<_>>();
    let [marker] = markers.as_slice() else {
        return Err(BiscuitError::Invalid(format!(
            "third-party block {index} must contain exactly one trusted presence marker"
        )));
    };
    let [
        Term::Str(subject),
        Term::Date(issued_at),
        Term::Date(expires_at),
    ] = marker.predicate.terms.as_slice()
    else {
        return Err(BiscuitError::Invalid(format!(
            "third-party block {index} has a malformed trusted presence marker"
        )));
    };
    if subject.is_empty()
        || expires_at <= issued_at
        || expires_at - issued_at > super::PRESENCE_TOKEN_TTL_SECS as u64
    {
        return Err(BiscuitError::Invalid(format!(
            "third-party block {index} has an invalid trusted presence lifetime"
        )));
    }
    let issued_at = chrono::DateTime::from_timestamp(*issued_at as i64, 0).ok_or_else(|| {
        BiscuitError::Invalid(format!(
            "third-party block {index} presence issue time is invalid"
        ))
    })?;
    let expires_at = chrono::DateTime::from_timestamp(*expires_at as i64, 0).ok_or_else(|| {
        BiscuitError::Invalid(format!(
            "third-party block {index} presence expiry is invalid"
        ))
    })?;
    let expected = super::presence_attenuation_block(subject, issued_at, expires_at)?;
    if block.facts != expected.facts
        || block.rules != expected.rules
        || block.checks != expected.checks
        || block.scopes != expected.scopes
        || block.context != expected.context
    {
        return Err(BiscuitError::Invalid(format!(
            "third-party block {index} is not the exact trusted presence attenuation shape"
        )));
    }
    Ok(())
}

fn decode_hex(value: &str, label: &str) -> Result<Vec<u8>, BiscuitError> {
    hex::decode(value)
        .map_err(|_| BiscuitError::Invalid(format!("{label} is not valid hexadecimal")))
}

fn decode_fixed_hex(
    value: &str,
    expected_len: usize,
    label: &str,
) -> Result<Vec<u8>, BiscuitError> {
    let decoded = decode_hex(value, label)?;
    if decoded.len() != expected_len {
        return Err(BiscuitError::Invalid(format!(
            "{label} must decode to {expected_len} bytes"
        )));
    }
    Ok(decoded)
}

// ---- Term-walk helpers ---------------------------------------------
//
// Working with `Authorizer::dump()` returns `Vec<Fact>`; each fact's
// `predicate.terms` is a `Vec<Term>`. These helpers pull the shapes
// we expect (single string, three strings, single date) and ignore
// anything that doesn't match. We deliberately don't error on shape
// mismatch — third-party blocks that smuggle in unexpected term
// types should be silently ignored, not crash the verifier.

fn first_string(terms: &[Term]) -> Option<String> {
    match terms.first()? {
        Term::Str(s) => Some(s.clone()),
        _ => None,
    }
}

/// Collect the `right(...)` facts an authorization decision may trust:
/// those that originate from the root-signed authority block (block 0),
/// plus rule-pack derivations chained off them. See the security note in
/// [`BiscuitFacts::extract`].
///
/// The `trusting authority` scope on the query rule is what enforces the
/// origin filter — `Authorizer::query` matches the rule body only
/// against facts whose origin the scope trusts (authority block +
/// authorizer), so an appended attenuation/third-party block cannot get
/// a `right(...)` fact into the result set. The rule head predicate is
/// namespaced (`_heddle_authority_right`) purely to avoid colliding with
/// any real predicate; the query does not persist it into the world.
fn authority_rights(authorizer: &mut Authorizer) -> Result<Vec<Right>, BiscuitError> {
    let rows: Vec<(String, String, String)> = authorizer
        .query("_heddle_authority_right($k, $p, $a) <- right($k, $p, $a) trusting authority")
        .map_err(|e| BiscuitError::Internal(format!("authority-scoped right query failed: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|(k, p, a)| Right::new(k, p, a))
        .collect())
}

/// True iff a `staff(...)` fact originates from the authority block.
/// Same origin-filtering mechanism as [`authority_rights`]: a `staff`
/// fact smuggled into an appended block is not trusted and therefore
/// cannot flip the operator (`is_staff`) decision (weft#513).
fn authority_staff_marker(authorizer: &mut Authorizer) -> Result<bool, BiscuitError> {
    let rows: Vec<(bool,)> = authorizer
        .query("_heddle_authority_staff($s) <- staff($s) trusting authority")
        .map_err(|e| BiscuitError::Internal(format!("authority-scoped staff query failed: {e}")))?;
    Ok(!rows.is_empty())
}

/// Web PoP-binding Option C: authority-scoped `request_signed_session(true)`
/// marker. Same origin-filtering as [`authority_staff_marker`] — a marker
/// smuggled into an appended block is not trusted, so an attenuated CLI /
/// service-account token cannot forge it to escape the legacy inline-PoP
/// gate in `token_id`.
fn authority_request_signed_session(authorizer: &mut Authorizer) -> Result<bool, BiscuitError> {
    let rows: Vec<(bool,)> = authorizer
        .query("_heddle_authority_req_sig($s) <- request_signed_session($s) trusting authority")
        .map_err(|e| {
            BiscuitError::Internal(format!(
                "authority-scoped request_signed_session query failed: {e}"
            ))
        })?;
    // Require the value to be `true`, not merely a `request_signed_session($s)`
    // fact of any arity/value (a `request_signed_session(false)` must not mark).
    Ok(rows.iter().any(|(s,)| *s))
}

/// Authority-scoped `root_established(true)` ceremony marker. Same origin
/// filter as [`authority_request_signed_session`]: an appended attenuation
/// block cannot forge independent-root standing.
fn authority_root_established(authorizer: &mut Authorizer) -> Result<bool, BiscuitError> {
    let rows: Vec<(bool,)> = authorizer
        .query("_heddle_authority_root_est($s) <- root_established($s) trusting authority")
        .map_err(|e| {
            BiscuitError::Internal(format!(
                "authority-scoped root_established query failed: {e}"
            ))
        })?;
    Ok(rows.iter().any(|(s,)| *s))
}

/// True iff a delegated-PoP transition originates in the authority block.
/// Transitions are meaningful only between blocks, so an authority occurrence
/// is invalid. Querying by origin avoids reparsing pretty-printed user strings
/// from the authority block.
fn authority_pop_delegation_present(authorizer: &mut Authorizer) -> Result<bool, BiscuitError> {
    let rows: Vec<(String, String, String)> = authorizer
        .query(
            "_heddle_authority_pop_delegation($p, $c, $s) <- \
             pop_delegation($p, $c, $s) trusting authority",
        )
        .map_err(|e| {
            BiscuitError::Internal(format!("authority-scoped pop_delegation query failed: {e}"))
        })?;
    Ok(!rows.is_empty())
}

/// Return the first value of a single-string authority-block fact, or
/// `None` when the predicate does not originate from the authority
/// block. Same origin-filtering mechanism as [`authority_rights`] /
/// [`authority_staff_marker`]: the `trusting authority` scope restricts
/// the rule body to facts whose origin is the authority block or the
/// authorizer, so a `<predicate>(...)` fact smuggled into an appended
/// attenuation/third-party block is not returned and therefore cannot
/// drive an authorization decision (weft#513).
///
/// `predicate` is a fixed, code-supplied identifier (never user input),
/// so interpolating it into the rule text is safe; the head predicate is
/// namespaced (`_heddle_authority_<pred>`) purely to avoid colliding with
/// a real predicate, and the query does not persist it into the world.
fn authority_string_fact(
    authorizer: &mut Authorizer,
    predicate: &str,
) -> Result<Option<String>, BiscuitError> {
    let rule = format!("_heddle_authority_{predicate}($v) <- {predicate}($v) trusting authority");
    let rows: Vec<(String,)> = authorizer.query(rule.as_str()).map_err(|e| {
        BiscuitError::Internal(format!("authority-scoped {predicate} query failed: {e}"))
    })?;
    Ok(rows.into_iter().next().map(|(v,)| v))
}

/// Return the unix-seconds value of a single-`Date` authority-block
/// fact, or `0` when the predicate does not originate from the authority
/// block. Date sibling of [`authority_string_fact`] — same `trusting
/// authority` origin filter, but the term is a Biscuit `Date`, extracted
/// via biscuit-auth's `TryFrom<Term> for SystemTime` conversion
/// (`Term::Date(secs) → UNIX_EPOCH + secs`). Used for `expires_at`, whose
/// forged appended value would otherwise leak into
/// `VerifiedBiscuit.expires_at` under the randomized-order dump race
/// (weft#513). `0` is returned both for "absent" and for the epoch, which
/// the sole consumer (`facts.exp`) already collapses to the same
/// "no expiry known" fallback.
fn authority_date_secs(authorizer: &mut Authorizer, predicate: &str) -> Result<i64, BiscuitError> {
    let rule = format!("_heddle_authority_{predicate}($v) <- {predicate}($v) trusting authority");
    let rows: Vec<(std::time::SystemTime,)> = authorizer.query(rule.as_str()).map_err(|e| {
        BiscuitError::Internal(format!("authority-scoped {predicate} query failed: {e}"))
    })?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|(t,)| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

struct IdentityObservationBounds {
    limited: bool,
    scope: String,
}

/// Derive the least-privileged scope ObserveIdentity may disclose. The route-specific
/// exemption makes ObserveIdentity callable through an otherwise restrictive caveat,
/// so the response cannot simply echo the authority block's broader scope.
fn identity_observation_bounds(
    checks: &[Check],
    rights: &[Right],
    is_staff: bool,
) -> IdentityObservationBounds {
    let mut limited = false;
    let mut resource_ceiling_groups: Vec<Vec<Rule>> = Vec::new();

    for check in checks {
        if !check.queries.iter().any(is_identity_observation_query) {
            continue;
        }
        let ceiling_queries = check
            .queries
            .iter()
            .filter(|query| !is_identity_observation_query(query))
            .cloned()
            .collect::<Vec<_>>();
        if ceiling_queries.iter().any(|query| {
            query
                .body
                .iter()
                .any(|predicate| predicate.name == "operation" || predicate.name == "resource")
        }) {
            limited = true;
        }
        let resource_queries = ceiling_queries
            .into_iter()
            .filter(|query| {
                query
                    .body
                    .iter()
                    .any(|predicate| predicate.name == "resource")
            })
            .collect::<Vec<_>>();
        if !resource_queries.is_empty() {
            resource_ceiling_groups.push(resource_queries);
        }
    }

    if !limited {
        return IdentityObservationBounds {
            limited,
            scope: render_scope_string(rights, is_staff),
        };
    }
    if resource_ceiling_groups.is_empty() {
        return IdentityObservationBounds {
            limited,
            scope: if is_staff {
                "spool:*".to_string()
            } else {
                render_scope_string(rights, false)
            },
        };
    }

    let mut candidates = std::collections::BTreeSet::new();
    for right in rights {
        candidates.insert(right.kind.clone());
        candidates.insert(right.path.clone());
    }
    for group in &resource_ceiling_groups {
        for query in group {
            for expression in &query.expressions {
                collect_ceiling_strings(&expression.ops, &mut candidates);
            }
        }
    }

    let mut resources = std::collections::BTreeSet::new();
    for kind in &candidates {
        for path in &candidates {
            if !authority_can_read(rights, is_staff, kind, path) {
                continue;
            }
            let within_every_ceiling = resource_ceiling_groups.iter().all(|group| {
                group
                    .iter()
                    .any(|query| resource_query_allows(query, kind, path))
            });
            if within_every_ceiling {
                resources.insert((kind.clone(), path.clone()));
            }
        }
    }

    let mut scope_tokens = resources
        .into_iter()
        .filter_map(|(kind, path)| (kind == "spool").then(|| format!("spool:{path}")))
        .collect::<Vec<_>>();
    if !scope_tokens.is_empty() {
        scope_tokens.push("read".to_string());
    }
    let scope = scope_tokens.join(" ");
    IdentityObservationBounds { limited, scope }
}

fn collect_ceiling_strings(ops: &[Op], candidates: &mut std::collections::BTreeSet<String>) {
    for op in ops {
        match op {
            Op::Value(Term::Str(value)) => {
                if let Some(path) = value.strip_suffix('/') {
                    candidates.insert(path.to_string());
                } else {
                    candidates.insert(value.clone());
                }
            }
            Op::Closure(_, closure_ops) => collect_ceiling_strings(closure_ops, candidates),
            _ => {}
        }
    }
}

fn is_identity_observation_query(query: &Rule) -> bool {
    query.body.iter().any(|predicate| {
        predicate.name == "operation"
            && matches!(predicate.terms.as_slice(), [Term::Str(value)] if value == crate::SELF_OBSERVATION_OPERATION)
    })
}

/// `BiscuitFacts::can_read` over a raw `&[Right]`, for the ObserveIdentity-scope bound
/// that runs before a `BiscuitFacts` exists. Shares the one inheritance walk
/// (weft#1130) so the disclosed scope can never claim more — or less — than the
/// read gate would actually allow.
fn authority_can_read(rights: &[Right], is_staff: bool, kind: &str, path: &str) -> bool {
    if is_staff {
        return true;
    }
    covered_through_inheritance(kind, path, |candidate_kind, candidate_path| {
        rights.iter().any(|right| {
            right.kind == candidate_kind
                && right.path == candidate_path
                && matches!(right.action.as_str(), "read" | "write" | "admin")
        })
    })
}

#[derive(Clone)]
enum CeilingValue {
    String(String),
    Bool(bool),
    Unknown,
}

fn resource_query_allows(query: &Rule, kind: &str, path: &str) -> bool {
    let [resource] = query.body.as_slice() else {
        return false;
    };
    let [Term::Variable(kind_var), Term::Variable(path_var)] = resource.terms.as_slice() else {
        return false;
    };
    if resource.name != "resource" {
        return false;
    }
    query.expressions.iter().all(|expression| {
        matches!(
            evaluate_ceiling_ops(&expression.ops, kind_var, kind, path_var, path),
            CeilingValue::Bool(true)
        )
    })
}

fn evaluate_ceiling_ops(
    ops: &[Op],
    kind_var: &str,
    kind: &str,
    path_var: &str,
    path: &str,
) -> CeilingValue {
    let mut stack = Vec::new();
    for op in ops {
        match op {
            Op::Value(Term::Variable(variable)) if variable == kind_var => {
                stack.push(CeilingValue::String(kind.to_string()));
            }
            Op::Value(Term::Variable(variable)) if variable == path_var => {
                stack.push(CeilingValue::String(path.to_string()));
            }
            Op::Value(Term::Str(value)) => {
                stack.push(CeilingValue::String(value.clone()));
            }
            Op::Value(Term::Bool(value)) => stack.push(CeilingValue::Bool(*value)),
            Op::Value(_) => stack.push(CeilingValue::Unknown),
            Op::Unary(Unary::Parens) => {}
            Op::Unary(_) => {
                let _ = stack.pop();
                stack.push(CeilingValue::Unknown);
            }
            Op::Closure(_, closure_ops) => stack.push(evaluate_ceiling_ops(
                closure_ops,
                kind_var,
                kind,
                path_var,
                path,
            )),
            Op::Binary(binary) => {
                let right = stack.pop().unwrap_or(CeilingValue::Unknown);
                let left = stack.pop().unwrap_or(CeilingValue::Unknown);
                stack.push(evaluate_ceiling_binary(binary, left, right));
            }
        }
    }
    match stack.as_slice() {
        [value] => value.clone(),
        _ => CeilingValue::Unknown,
    }
}

fn evaluate_ceiling_binary(
    binary: &Binary,
    left: CeilingValue,
    right: CeilingValue,
) -> CeilingValue {
    match (binary, left, right) {
        (
            Binary::Equal | Binary::HeterogeneousEqual,
            CeilingValue::String(left),
            CeilingValue::String(right),
        ) => CeilingValue::Bool(left == right),
        (
            Binary::Equal | Binary::HeterogeneousEqual,
            CeilingValue::Bool(left),
            CeilingValue::Bool(right),
        ) => CeilingValue::Bool(left == right),
        (Binary::Prefix, CeilingValue::String(value), CeilingValue::String(prefix)) => {
            CeilingValue::Bool(value.starts_with(&prefix))
        }
        (Binary::And | Binary::LazyAnd, CeilingValue::Bool(left), CeilingValue::Bool(right)) => {
            CeilingValue::Bool(left && right)
        }
        (Binary::Or | Binary::LazyOr, CeilingValue::Bool(left), CeilingValue::Bool(right)) => {
            CeilingValue::Bool(left || right)
        }
        _ => CeilingValue::Unknown,
    }
}

/// Render the rights list back into the access-token scope-string format
/// for consumers that haven't been swung over to BiscuitFacts
/// helpers yet (notably `clamp_derived_scope` until Phase 5 deletes
/// it, and external integration tests that do `.contains("admin")`
/// checks). Format mirrors the legacy parser:
///
///   * `spool:* staff` for an operator token
///   * `spool:{path}` for every literal spool grant, followed by the weakest
///     strongest action shared by every rendered path
///
/// We first retain the strongest action on each path because the rule pack
/// derives weaker actions. If a token contains mixed per-path strengths, the
/// scope grammar cannot express those groups independently, so the renderer
/// uses the weakest of those per-path maxima. That is a conservative display
/// and cannot widen authority if a legacy consumer parses it again.
fn render_scope_string(rights: &[Right], is_staff: bool) -> String {
    if is_staff {
        return "spool:* staff".to_string();
    }
    let mut strongest_by_path = std::collections::BTreeMap::<String, u8>::new();
    for r in rights {
        if r.kind != "spool" {
            continue;
        }
        let strength = match r.action.as_str() {
            "read" => 1,
            "write" => 2,
            "admin" => 3,
            _ => continue,
        };
        strongest_by_path
            .entry(r.path.clone())
            .and_modify(|current| *current = (*current).max(strength))
            .or_insert(strength);
    }
    let Some(common_strength) = strongest_by_path.values().copied().min() else {
        return String::new();
    };
    let mut tokens = strongest_by_path
        .into_keys()
        .map(|path| format!("spool:{path}"))
        .collect::<Vec<_>>();
    tokens.push(
        match common_strength {
            1 => "read",
            2 => "write",
            3 => "admin",
            _ => unreachable!("only canonical spool strengths are inserted"),
        }
        .to_string(),
    );
    tokens.join(" ")
}

#[cfg(test)]
mod reserved_predicate_tests {
    use biscuit_auth::{BiscuitBuilder, KeyPair};

    use super::*;

    /// A token that asserts the reserved request predicate in its **authority**
    /// block must be refused before any request fact is injected.
    ///
    /// On the root-signed path Biscuit's own block scoping already defeats this
    /// — a check in block N cannot see facts from a later block, so a bearer's
    /// appended fact is invisible to weft's attenuation check. The authority
    /// block is the exception: its facts are visible to every check, and on the
    /// client-minted path (`verify_client_minted_at_with_extra_facts`) the
    /// bearer signs that block themselves. This is the hole the scan closes.
    #[test]
    fn a_token_asserting_the_reserved_request_predicate_is_refused() {
        let keypair = KeyPair::new();
        let biscuit = BiscuitBuilder::new()
            .fact("user(\"mallory\")")
            .expect("subject fact")
            .fact(
                format!(
                    "{}(\"spool\", \"content\", \"team:alpha\", \"deadbeef\")",
                    crate::edge::EDGE_REQUEST_PREDICATE
                )
                .as_str(),
            )
            .expect("self-asserted request fact")
            .build(&keypair)
            .expect("build token");

        let error = reject_reserved_request_facts(&biscuit)
            .expect_err("an authority block must not describe the in-flight request");
        assert!(
            matches!(&error, BiscuitError::Invalid(message)
                if message.contains(crate::edge::EDGE_REQUEST_PREDICATE)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_ordinary_token_passes_the_reserved_predicate_scan() {
        let keypair = KeyPair::new();
        let biscuit = BiscuitBuilder::new()
            .fact("user(\"alice\")")
            .expect("subject fact")
            .build(&keypair)
            .expect("build token");

        reject_reserved_request_facts(&biscuit)
            .expect("a token that asserts nothing reserved must verify normally");
    }
}
