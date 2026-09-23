# Apex: capability tokens without a general Datalog runtime

Research and proposed design for [HeddleCo/heddle#1799](https://github.com/HeddleCo/heddle/issues/1799). Written 2026-09-23. **Design only; no implementation, benchmark, security audit, or deployment is claimed.**

## Recommendation

Build a small, versioned capability kernel with deterministic CBOR, an Ed25519 signature chain, typed grants, and a bounded caveat expression tree. Keep owner-history, recovery, resource visibility, storage, and transport policy in application adapters. Provide native Rust and native TypeScript implementations with the same normative vectors. Do not implement a general Datalog interpreter or a Biscuit compatibility layer.

The useful simplification is removing arbitrary fact derivation and implicit fact provenance, not removing delegation or owner-state verification. The latter are load-bearing. Sealed public evidence, proof-key transfers, exact request binding, and revocation knowledge must survive the replacement.

This is **conditionally worth doing**, not an obvious dependency cleanup. A small wire library is feasible; replacing the authorization boundary safely is a multi-repository security project. Budget roughly **20–34 engineer-weeks including integration and independent review**. If that commitment is unavailable, maintain a pinned Biscuit fork and keep the current TS encoder. A rushed in-house verifier is the worse option.

The most consequential findings are:

* The current shared rule pack contains three action-lifting rules. Resource inheritance is Rust code, not recursive Datalog. Purge's owner verifier compares exact subject facts and merely parses its one-rule pack; it does not execute that rule pack (`B/src/rules.biscuit:27–32`, `B/src/resource.rs:96–170`, `C/src/capability.rs:254–325`).
* `weft-capability-verifier` is historical in this checkout. Weft uses published `heddle-biscuit-verifier` 0.24.1 and `heddleco-capability-verifier` 0.20.0. The supplied Heddle checkout instead pins capability-verifier 0.4.0. Treating all four working trees as one coherent release would be wrong (`W/Cargo.toml:35,68–69`, `W/Cargo.lock:2379–2393,2789–2808`, `H/Cargo.lock:2808–2809`).
* #872's **final direction is event-driven revocation in the signed spool settings graph**, not periodic 15-minute checkpoints. Its final TTL decision is seven days. Existing code still has 30-day callers. See the issue chronology below and `H/crates/repo/src/owner_authorization.rs:21–23`, `W/crates/weft-authz/src/biscuit.rs:48–82`.
* Owner anchoring does not currently mean every authorization decision is portable or every server signing key is gone. Registered-key identity, hosted grants, custodial sessions, anonymous sessions, and owner-gated purge are different trust surfaces (`W/crates/weft-authz/src/biscuit/keys.rs:1–35`, `W/crates/weft-authz/src/biscuit/cache.rs:538–580,695–746`, `W/crates/weft-hosted/src/hosted_access.rs:23–65,400–430`, `C/src/capability.rs:108–199`).

## 1. Evidence boundary and prior art

### 1.1 Source notation and reproducibility

Every current-code statement below has a `file:line` citation. Ranges identify the inspected implementation, not a claimed test run. Prefixes expand as follows; they are deliberately versioned where the source is a registry package.

| Prefix | Source root |
| --- | --- |
| `H` | `/home/heddleco/HeddleCo/heddle`, HEAD `52bb0aaf063fdb76f9e392d1432d25df7705a8a7` |
| `W` | `/home/heddleco/HeddleCo/weft`, HEAD `1f16eea6130b041dd5d4e8e72f95904ee3cf9e2a` |
| `T` | `/home/heddleco/HeddleCo/tapestry`, HEAD `d9ab96b1b1935d4df345b6b481696f2c7d9fe184`, **working-tree contents, with substantial pre-existing staged changes** |
| `A` | `/home/heddleco/HeddleCo/api`, HEAD `28bc22f780c29cfe21304d8b014f0384f054d17b` |
| `R` | `/home/scratch/cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f` |
| `B` | `R/heddle-biscuit-verifier-0.24.1` |
| `C` | `R/heddleco-capability-verifier-0.20.0` |
| `C4` | `R/heddleco-capability-verifier-0.4.0` |
| `Q` | `R/heddle-thread-api-0.24.1` |
| `U` | `R/biscuit-auth-6.0.0` |
| `L` | `/home/heddleco/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/heddle-hosted-client-0.15.5` — historical published source, **not a dependency claimed for current H** |

Useful direct links: [shared verifier](/home/scratch/cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f/heddle-biscuit-verifier-0.24.1/src/lib.rs), [owner verifier](/home/scratch/cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f/heddleco-capability-verifier-0.20.0/src/capability.rs), [TS encoder](/home/heddleco/HeddleCo/tapestry/src/lib/client/biscuit.ts), [identity model](/home/heddleco/HeddleCo/weft/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md).

H, W, and A were clean at initial inspection; T was not. These are local source observations, **not proof of what production is running**. Cached registry sources were read to inspect the exact packages named by W's lockfile. No checkout, fetch, install, build, test generation, commit, issue comment, or repository code edit was performed. This file is the sole created artifact.

Historical `weft-capability-verifier` was inspected with `git show 15931be9c^:crates/weft-capability-verifier/src/lib.rs`; commit `15931be9c` deletes the crate. Historical citations use `W@15931be9c^`. It already injected the shared rules and public-key verification (`.../src/lib.rs:7–44`). The replacement import in current W is explicit (`W/crates/weft-authz/src/biscuit.rs:38–45,894–909`). The absent crate is not a separate live implementation to migrate again.

### 1.2 Issues read before implementation inventory

| Prior art | Finding and effect on this design |
| --- | --- |
| [tapestry#237](https://github.com/HeddleCo/tapestry/issues/237) | Authority signing needs an asynchronous external key. Public wire types did not expose a usable authority-signing seam. Existing TS work intentionally emits a narrow subset. Its requirement was exact payload construction and symbol interning, **not** identical protobuf serialization. Apex deliberately adopts a stronger byte-identity requirement. |
| [weft#834](https://github.com/HeddleCo/weft/issues/834) | Pin fixed keys, input facts, block bytes, signing payloads, symbol tables, and completed tokens separately. Preserve the independent checks, not the old wire format. |
| [weft#836](https://github.com/HeddleCo/weft/issues/836) | The discussion corrected a server-root proposal to owner anchoring and required an API-first cutover. Later comments and code distinguish self-rooted, server-rooted, and agent-rooted accounts. Do not claim zero server-held keys universally. |
| [weft#872](https://github.com/HeddleCo/weft/issues/872) | Read **all revisions**. The 15-minute owner-signed snapshot proposal was superseded first by event-based signed sets, then by signed versioned spool settings with grow-only revocations. The final decision sets maximum capability TTL to seven days, without a special purge ceiling. |
| [weft#1421](https://github.com/HeddleCo/weft/issues/1421) | Biscuit brings an older dalek stack, but the separate old AES-GCM chain comes from the iroh fork. Apex cannot honestly claim to remove that entire duplicate stack. |
| [weft#247](https://github.com/HeddleCo/weft/issues/247) | Wall-clock evaluation limits produced load-dependent failures. Replace them with structural limits and a statically computed work bound, not a larger timeout. |
| [heddle#1792](https://github.com/HeddleCo/heddle/issues/1792) | Disabling Datalog macros in 6.0.0 encounters an upstream feature-gating bug. Removing default features alone is not a working repair. The issue's original “zero macros” refers to Heddle, not all Weft tests. |

For #872, the controlling revisions are [event-based](https://github.com/HeddleCo/weft/issues/872#issuecomment-5118444746), [settings graph](https://github.com/HeddleCo/weft/issues/872#issuecomment-5119031636), and [seven-day decision](https://github.com/HeddleCo/weft/issues/872#issuecomment-5120696961). The last comment leaves the issue parked as design work. This spec does not resurrect the earlier heartbeat proposal.

The private-module and feature-gating complaints are independently visible in `U/src/lib.rs:247–252`, `U/src/crypto/mod.rs:486–504`, `U/src/token/builder.rs:165–173`, and the unconditional import in `U/src/token/builder/fact.rs:14`. A read-only upstream check found [PR #306](https://github.com/eclipse-biscuit/biscuit-rust/pull/306) merged 2025-09-16 and the repository's latest published GitHub release still [6.0.0](https://github.com/eclipse-biscuit/biscuit-rust/releases/tag/biscuit-auth-6.0.0), dated 2025-07-16. That release observation is not a new build reproduction or a guarantee about every unpublished branch.

### 1.3 Model documents are guidance, not a substitute for source

The canonical document explicitly distinguishes CURRENT/TARGET and contains successive amendments (`W/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md:3–51`). Its native-TS migration is still labelled TARGET at lines 132–135 although the native encoder exists (`T/src/lib/client/biscuit.ts:84–123`). Its original destructive-operation design is explicitly superseded (`W/docs/design/836-owner-anchored-destructive-authz.md:3–11`). The replacement says purge alone is owner-gated and ordinary redaction/visibility are writes (`W/docs/design/1496-authz-transport-cutover.md:10–21`), consistent with the direct-purge verifier (`C/src/capability.rs:108–199`).

Read visibility separately: inherited embargoes and exact source reads remain an application graph problem, not token inference (`W/docs/v2-visibility-port.md:3–24`; `W/crates/weft-hosted/src/hosted_access.rs:35–51`). No exact `authz-visibility-model` filename was found in the four tracked-file inventories; the identity model, contradictions document, the two authz designs above, and the v2 visibility document were consulted instead.

## 2. What is actually used

“Used” below means an inspected emitter, evaluator, or caller. **Live path**, **library/foundation**, **test**, and **historical** are intentionally distinct. Existence of a public function does not establish that a production request reaches it.

### 2.1 Consumers and block shapes

| Surface | Actual dependency on Biscuit | Evidence |
| --- | --- | --- |
| Weft minting | Authority facts, mandatory expiry check, optional operation fences; agent/ephemeral delegated credentials; binary token encoded as URL-safe base64 | `W/crates/weft-authz/src/biscuit.rs:396–524,591–784` |
| Shared session verification | Signature chain, all block checks, action rule pack, authority-scoped extraction, PoP chain, optional verifier request facts | `B/src/lib.rs:189–208,245–325`; `B/src/facts.rs:422–543,814–948` |
| Ordinary attenuation | Appending an agent label, expiry, operation/resource constraints, and a separately signed PoP transition | `W/crates/weft-authz/src/biscuit.rs:1202–1251`; `B/src/delegation.rs:65–134` |
| Edge/provider attenuation | First-party checks-only narrowing, pinning repository, facet, audience, extent-set digest, operation, and expiry | `W/crates/weft-authz/src/biscuit.rs:1266–1302`; `B/src/edge.rs:548–609`; `B/src/facts.rs:843–853,964–993` |
| Third-party blocks | Exact terminal presence block with an external trusted signature, unchanged effective PoP key, and a maximum five-minute lifetime; general third-party forge-denial tests also exist | `W/crates/weft-authz/src/biscuit.rs:525–559,2250–2260,2502–2570`; `B/src/lib.rs:36–50,94–129`; `B/src/facts.rs:814–841,1023–1075` |
| Third-party reachability caveat | Presence construction and verifier support remain, but the found hosted `append_presence_attenuation` call is inside `#[cfg(test)]`; do not call production issuance demonstrated | `W/crates/weft-hosted/src/server/hosted/auth.rs:244–257,476–489`; search across W's source found no non-test caller besides the library definition |
| Artifact attestation | A **separate authority token** signed by the device, not a special Biscuit block kind or a third-party block; six facts; Rust authority-scoped verification | `T/src/lib/client/biscuit.ts:165–181,237–248`; `W/crates/weft-authz/src/biscuit/artifact.rs:24–94` |
| Artifact reachability caveat | Native mint/verify and vectors exist. Found TS call is the dev-e2e driver; no normal product caller of `mintAttestation` or W's `verify_artifact_envelope` was found | `T/src/routes/dev-e2e/biscuit-driver/+page.svelte:105–123`; `W/crates/weft-authz/src/biscuit/artifact.rs:97–165`. Treat product integration as unproven. |
| Sealing | Load-bearing for public thread-control and boundary-authority evidence; receivers reject an unsealed token. Creation requires a final exact narrowing block and sealed evidence | `C/src/thread_control_authority.rs:21–50,206–235`; `C/src/boundary_authority.rs:132–135`; `C/src/creation.rs:226–237,341–365` |
| Direct owner purge | Owner root/history and a signed direct capability are verified independently; the subject Biscuit must have exactly one block and exactly the expected facts. No attenuation of this purge capability | `C/src/capability.rs:137–199,254–325,329–386`; same subject check in `C4/src/capability.rs:254–325` |
| Thread API | Carries credentials and proof, resolves actual method and spool context, calls shared verifier, checks a right, consumes a durable nonce, and rechecks streams | `Q/src/credentials.rs:20–151`; `Q/src/authority.rs:30–123` |
| CLI | Locally mints root authorities; derives agents offline; parses unverified facts for local credential display/installation; those parsers are not server authorization | `H/crates/cli/src/hosted_runtime/root_mint.rs:69–106`; `H/crates/cli/src/hosted_runtime/device_flow.rs:656–746`; `H/crates/cli/src/hosted_runtime/auth.rs:555–639` |
| Hosted-client | Historical published 0.15.5 also mints roots and emits agent expiry, operation-deny checks, resource checks and display scopes. This crate is absent from supplied H's current workspace | `L/src/hosted_runtime/root_mint.rs:81–106`; `L/src/hosted_runtime/device_flow.rs:690–778`; `H/Cargo.toml:1–30` and current `H/crates/cli/src/hosted_runtime/` |
| TS session/per-tab | Native handwritten protobuf encoder, async WebCrypto authority signing, unsealed append. Session mint and tab attenuation have auth-flow callers | `T/src/lib/client/biscuit.ts:17–65,84–155`; `T/src/lib/client/auth-flow.ts:107–113,281–285` |
| TS owner subject | A second native Biscuit implementation emits/verifies owner-subject facts. Its older `grant.actions` shape is not evidence of parity with W's current single-action verifier | `T/src/lib/owner-authorization/subject-biscuit.ts:69–120`; compare `C/src/capability.rs:123–133,292–311` |

The TS per-tab block has no `pop_delegation`; current B requires one in every ordinary post-authority block except the exact presence/edge exceptions. Consequently the historical TS session/tab API and current shared session verifier must **not** be assumed interoperable without a real integration check (`T/src/lib/client/biscuit.ts:218–234`; `B/src/facts.rs:814–863`). This discrepancy is recorded, not repaired by loosening the proposed verifier.

### 2.2 Complete application predicate inventory for the inspected paths

This table covers named application predicates found in the emitters/evaluators above and their supporting modules. Predicate **values** such as RPC names and arbitrary `Right::new` actions remain an open string vocabulary today; enumerating every possible string is impossible (`B/src/facts.rs:61–84`). Upstream's default symbol table is not a list of used predicates (`W/crates/weft-authz/tests/biscuit_cross_language_vectors.rs:17–46`).

Types below: `s` string, `d` Biscuit Date/whole Unix seconds, `i` integer, `b` boolean. “Root” describes trusted extraction, not permission to trust any self-signed root.

| Predicates and arity | Emission/evaluation and origin | Apex representation |
| --- | --- | --- |
| `user(s)`, `session(s)` | Mandatory root identity/session extraction; W and H emit; T root emits. `W/.../biscuit.rs:623–624`; `B/src/facts.rs:483–485,525–528`; `T/.../biscuit.ts:209–214` | Root claims `user`, `session`; verified identity comes from admitted anchor. Session alias is revocable. |
| `issued_at(d)`, `expires_at(d)` | W emits both; H root emits expiry; T emits both, and tab expiry. Root expiry extraction is distinct from the executable expiry check. `W/.../biscuit.rs:625–626,755–760`; `H/.../root_mint.rs:96–101`; `B/src/facts.rs:501–511` | Mandatory signed validity interval; block intervals intersect. Display uses effective expiry. |
| `amr(s)` | Root mint and any-block audit/display collection, not a demonstrated step-up gate. `W/.../biscuit.rs:675–677`; `B/src/facts.rs:400–414` | Origin-labelled repeated metadata. Step-up policy must use a separately trusted attestation. |
| `subject_kind(s)`, `subject(s)`, `subject_user_uuid(s)` | Anon/user substrate; W emits; B extracts root-only, and hosted code rebinds user from key ownership. `W/.../biscuit.rs:646–658`; `B/src/facts.rs:453–454,499–500,621–629`; `W/.../cache.rs:695–746` | Typed principal kind and ID in root claims; untrusted display claims cannot replace admitted identity. |
| `right(s,s,s)` | Concrete kind/path/action authority; three shared lifting rules; resource inheritance outside Datalog. `W/.../biscuit.rs:679–686`; `B/src/rules.biscuit:30–32`; `B/src/facts.rs:644–710` | Typed `Grant(selector, actions)`, explicit action implication table. |
| `staff(b)` | W mint API emits; root query exists; any-block marker also detects forbidden client assertions. Hosted verification discards token staff and overlays registered grant. `W/.../biscuit.rs:689–695`; `B/src/facts.rs:381–386,546–550,608–618`; `W/.../cache.rs:695–746` | Hosted-local policy input only. A token metadata field never creates staff authority. |
| `delegated_from(s)`, `device(s)`, `service_account(s)`, `credential_id(s)` | W emits root identity/revocation bindings, root-only extraction. `W/.../biscuit.rs:697–714`; `B/src/facts.rs:453–464,483–492` | Typed immutable root claims; device/credential revocation aliases. |
| `agent_provider(s)`, `agent_model(s)` | Root provenance, not forgeable descendant display labels. `W/.../biscuit.rs:703–707`; `B/src/facts.rs:493–498` | Root attribution claims retained with origin. |
| `device_pop_key(s)` | Hex Ed25519 public key; unverified root selector and verified PoP anchor. `W/.../biscuit.rs:715–721`; `B/src/lib.rs:136–158`; `B/src/facts.rs:486–490` | Raw typed `holder_key`, independently admitted `issuer_key_id`; never overload their roles. |
| `bootstrap_session(b)`, `request_signed_session(b)`, `root_established(b)`, `signup_bootstrap_email(s)` | Emitted by W; root-scoped where authorization-bearing; bootstrap marker additionally used for client-mint rejection. `W/.../biscuit.rs:723–752`; `B/src/facts.rs:389–397,463–464,530–537` | Profile-specific root claims. Independent-root policy requires an admitted ceremony plus delegation depth zero, not a caller boolean. |
| `agent(s)`, `agent_expires_at(d)`, `agent_scope(s,s)` | Agent label in shared builder and verified PoP-hop attribution; additional expiry/scope metadata in H CLI, read by H local credential parser. `B/src/delegation.rs:65–78`; `B/src/facts.rs:927–945`; `H/.../device_flow.rs:656–671,734–744`; `H/.../auth.rs:630–639` | Hop annotations plus actual interval/resource caveats; annotation alone never enforces scope. |
| `pop_delegation(s,s,s)` | Parent block revocation ID, child key, parent-PoP signature; forbidden at authority; precisely one per ordinary hop. `W/.../biscuit.rs:1229–1251`; `B/src/facts.rs:855–925,1180–1192` | Dedicated `holder_transfer` and signature over the **entire hop**, not a string fact. |
| `tab(s)` | TS per-tab metadata only. `T/.../biscuit.ts:218–234` | Hop annotation `tab`. |
| `weft_presence_attenuation_v1(s,d,d)` | Exact external-signature terminal narrowing marker; library/tests as noted above. `B/src/lib.rs:46,111–128`; `B/src/facts.rs:1023–1075` | `EndorsedNarrow` block with presence schema and terminal flag. |
| `weft_edge_serving_attenuation_v1(i)` | Sole permitted fact in checks-only edge block; version marker. `B/src/edge.rs:67,595–600`; `B/src/facts.rs:972–993` | Typed narrowing block; marker unnecessary on wire. |
| `time(d)`, `operation(s)`, `resource(s,s)` | Authorizer-owned request inputs; exact resolved request, not credentials. `B/src/lib.rs:254–275` | `Context.now`, `Context.method`, `Context.resource`; token cannot define context. |
| `edge_extent_request_v1(s,s,s,s)`, `edge_extent_request_v2(s,s,s,s)` | Verifier tuple of repository/facet/audience/digest; token assertions/rule heads rejected. `B/src/edge.rs:56–58,575–582`; `B/src/facts.rs:1003–1018` | Exact typed context tuple comparison. Byte digest plus resource UUID; profile distinguishes v1/v2 interpretation. |
| `heddle_spool_creation_request_v1(s)` | Verifier-computed creation statement hash, with exact `CreateSpool` check; token assertion forbidden. `C/src/creation.rs:26,226–237,347–363,416–433` | `Context.creation_digest` equality plus method ceiling. |
| `heddle_boundary_acceptance_request_v1(s,s,s,s,s,s)` | Exact kind/account/thread/original-subject/publisher/agent tuple; reserved against token facts and rule heads. `C/src/boundary_authority.rs:13,223–251` | Single `Context.original_subject` tuple; preserves correlation. |
| `artifact_signature_version(s)`, `artifact_repository(s)`, `artifact_kind(s)`, `artifact_digest(s)`, `artifact_signer_key(s)`, `artifact_signed_at(i)` | Separate artifact authority; each queried exactly once from authority and signer key checked. `T/.../biscuit.ts:237–248`; `W/.../artifact.rs:50–94` | `ArtifactStatement` schema: version, resource, kind, digest, signer, claimed signing time. |
| `owner_subject(i,s,s)`, `owner_capability(s)`, `owner_validity(i,i)`, `owner_grant(s,s,b,i)` | Exact subject proof against separately signed owner capability. `C/src/capability.rs:292–315`; `T/src/lib/owner-authorization/subject-biscuit.ts:69–118` | Root holder/principal, authority ID, validity, direct exact grant. Replace redundant subject Biscuit with request/operation PoP. |
| `owner_purge(s,s)` | Rule-pack head parsed but not evaluated in current C. `C/src/rules.biscuit:7`; `C/src/capability.rs:319–325` | Direct owner grant match in purge profile; no derivation engine. |
| `_heddle_authority_*`, `_artifact_*`, `query` | Internal query/projection heads, not granted token vocabulary. `B/src/facts.rs:1128–1232`; `W/.../artifact.rs:71–87`; `T/.../biscuit.ts:367–375` | Typed accessors; no wire predicates. |

Throughout this document `W/.../biscuit.rs`, `W/.../cache.rs`, `W/.../artifact.rs`, and `W/.../keys.rs` mean respectively `W/crates/weft-authz/src/biscuit.rs`, `W/crates/weft-authz/src/biscuit/cache.rs`, `W/crates/weft-authz/src/biscuit/artifact.rs`, and `W/crates/weft-authz/src/biscuit/keys.rs`; `H/.../` means `H/crates/cli/src/hosted_runtime/`; `T/.../biscuit.ts` means `T/src/lib/client/biscuit.ts`.

The owner capability parser permits an `AnyAnonymous` principal and then requires the subject Biscuit to be absent, but actual purge verification requires a subject signing key. Thus parser acceptance is not evidence of an anonymous purge path (`C/src/capability.rs:172–189,254–268`; `C/src/operation.rs:140–158`). The proposed purge profile requires PoP from the outset. Current owner capabilities can contain up to 64 exact purge grants, not just one (`C/src/capability.rs:137–157,196–198`; `C/src/limits.rs:20–21`).

Do not preserve two misleading vocabularies as requirements: `admin()` appears in the rule-pack comments but there is no corresponding live extraction branch; `staff` and concrete `right` are what the inspected extractor uses (`B/src/rules.biscuit:15–18`; `B/src/facts.rs:400–418,439,546–550`). `right_prefix` is a **target**, not an emitted predicate (`W/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md:290–297`). Neither needs a compatibility opcode.

### 2.3 Language features: necessary versus merely available

| Feature | Evidence-based assessment |
| --- | --- |
| Rules/recursion | The three `right` rules refer to `right` in both head and body, so the predicate dependency graph is technically self-recursive. Their action values form a fixed acyclic implication graph (`admin → write → read`). No general recursive relation or unbounded transitive closure is needed. Resource ancestry runs in Rust (`B/src/rules.biscuit:30–32`; `B/src/resource.rs:96–170`). |
| Joins | At most small positive conjunctions in inspected policy: request resource plus authority right for CI verdict; operation plus creation digest. Edge/acceptance fields are deliberately composite tuples (`B/src/lib.rs:277–290`; `C/src/creation.rs:230–235`; `C/src/boundary_authority.rs:223–233`). No arbitrary relational join engine is needed. |
| Expressions | `==`, `!=`, `<`, `<=`, `&&`, `||`, constant true/false, and `starts_with` occur. `!=` is value inequality, **not** negation-as-failure. TS date check is inclusive; Rust emitters generally use exclusive expiry (`H/.../device_flow.rs:664–731`; `B/src/delegation.rs:79–132`; `T/.../biscuit.ts:342–375`). |
| Dates/numbers/booleans | Whole-second dates and integer artifact time; boolean markers and owner selector flag; integer owner principal/action/version. No floating-point authorization was found (`W/.../biscuit.rs:600–604,694`; `T/.../biscuit.ts:324–337`; `C/src/capability.rs:292–311`). |
| Negation, aggregates, regex, arbitrary arithmetic, extern calls | No production emitter requiring these was found in the inspected sources. `device_proof_valid()` is an aspirational comment, not an extern implementation (`W/.../biscuit.rs:715–721`). `regex-full` is enabled in manifests; that is not evidence that an emitted caveat uses regex (`W/Cargo.toml:52–55`; `U/Cargo.toml:47–49,159–162`). Application glob matching is a separate concern (`W/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md:251–269`). |
| Checks/policies | Conjunctive checks across blocks, alternatives within checks, followed by `allow if true` and application permission decisions; no demonstrated ordered allow/deny policy program carried in tokens (`B/src/lib.rs:254–316`; `B/src/delegation.rs:94–132`). |
| Trust scopes | Authority-only queries are essential. Any-block collection is explicitly unsafe for identity/rights. Explicit externally trusted presence signature is a separate path (`B/src/facts.rs:367–438,814–841`). |
| Macros | Not a production language requirement in the inspected H/W paths. W has actual test `fact!` use, so “none anywhere” would be false (`W/crates/weft-hosted/tests/v2_replication/owner_management.rs:788–794`). `datalog-macro` remains explicitly enabled in C (`C/Cargo.toml.orig:30,43`). |

Negative search results are scoped to the source roots/versions above, including supporting modules and tests. Public APIs accept arbitrary Biscuit programs today; a caller can supply more language than our own emitters do. Apex intentionally rejects that extra expressiveness at the hard cut.

### 2.4 Crypto, serialization, revocation, and trust

* **Token keys:** inspected application emitters use Ed25519. TS explicitly emits algorithm 0, block version 3, signature version 0 (`T/.../biscuit.ts:17–20,184–196,445–460`); Rust key loaders specify Ed25519 (`W/.../keys.rs:73–82,94–103`; `H/.../root_mint.rs:13,69–77`). Biscuit's P-256 support is compiled in (`U/Cargo.toml:108–128,141–151`), but no application Biscuit P-256 mint was found. WebAuthn ES256/RSA support and C's ES256 delegation verification are separate, real dependencies, not removable just because Apex chooses Ed25519 (`W/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md:125–130`; `C/src/passkey_delegation.rs:163–183`).
* **Two secret roles:** TS authority/session keys are non-extractable, but the unsealed chain seed is intentionally exportable (`T/.../biscuit.ts:84–98,139–154,494–515`). A design that demands every next-key seed be non-extractable while embedding that seed in a transferable token is contradictory. Apex separates public presentation from the attenuation handle.
* **Serialization:** Rust uses Biscuit protobuf and base64 helpers; TS hand-encodes protobuf, interns symbols at 1024, and signs exactly the stored block. Byte-identical encoding is not the current Biscuit contract (`W/.../biscuit.rs:780–782`; `T/.../biscuit.ts:10–20,263–280,313–375,670–729`). Q normally carries raw bytes, but its `owned_device` branch base64-encodes the sealed token into its byte vector: this is an explicit integration inconsistency to resolve, not a format precedent (`Q/src/credentials.rs:24–33,69–79,113–123`; `Q/src/authority.rs:77–85`).
* **Revocation IDs:** one identifier per signed block; helpers hex-encode upstream identifiers, and session/credential aliases are checked too (`W/.../biscuit.rs:1141–1148`; `B/src/facts.rs:350–353,530`; `W/.../cache.rs:923–959`). Revoking an ancestor must kill descendants. Public-key revocation is an additional index, not the same identifier.
* **Online revocation:** current cache has fail-closed health/lease behavior and checks sessions, block IDs, and root keys (`W/.../cache.rs:840–876,923–959`). Replacing Datalog must not turn an unhealthy online revocation cache into an empty allow-list.
* **Owner revocation foundation:** signed governance states and grow-only entries are stored today (`W/migrations/001_baseline.sql:5892–5908`; `W/crates/weft-registry/src/pg_hosted_registry/owner_governance.rs:562–621`). This proves storage/verification plumbing exists, **not** that complete offline revocation distribution and consumption is finished; the model still calls offline distribution open (`W/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md:479–484`).
* **Rotation:** the session verifier tries supplied keys; selected device keys narrow the set through the cache (`B/src/lib.rs:189–208`; `W/.../cache.rs:538–580`). No emitted numeric Biscuit root-key ID was found in the inspected mint builders; TS can decode an optional ID (`T/.../biscuit.ts:41–46`). Owner rotation/recovery is a different signed-history protocol (`C/src/owner.rs:34–85,428–504,621–672`), not an expanded session trust list.
* **PoP:** the existing delegation payload is a domain plus immediate parent revocation ID plus child key; B verifies the parent PoP signature in block order. Request signing and replay protection happen outside Datalog (`W/.../biscuit.rs:84–105,1229–1251`; `B/src/facts.rs:875–925`; `Q/src/credentials.rs:127–150`; `Q/src/authority.rs:40–54`). Apex must bind the exact caveats in a proof-key transfer too, not merely the recipient key.

## 3. Requirements and deliberate non-requirements

The following are proposed acceptance requirements, not descriptions of shipped Apex:

1. Async external signing with no private-key export; usable with WebCrypto Ed25519 `CryptoKey` and a remote signer implementing the exact signing suite. No synchronous callback disguised as async.
2. Identical canonical bytes in Rust and TS for identical inputs, including fixed randomness. Identical parse rejection, authorization outcome, and stable reason code for every conformance vector.
3. Offline append, verification, sealed public evidence, artifact attestations, and explicit external attestations. No network inside token parsing/evaluation.
4. Owner-derived authority; a supplied root key or issuer name alone never establishes trust. Direct-only purge remains direct-only. Server-custodial/public-service profiles are separate trust realms, never fallback owner roots.
5. Monotonic attenuation, explicit fact provenance, cryptographic holder transfers, and request PoP. Ordinary append cannot create rights, replace identity, inject request facts, or remove an earlier constraint.
6. Event-driven signed revocation views integrated with #872's settings graph; ancestor and subject-key revocation; persistent rollback/fork handling; operation receipts name the view used. No claim of instant offline revocation.
7. Deterministic finite work and allocation ceilings, with a preflight work estimate. Clock time is an explicit validity input, never an execution budget.
8. Versioned semantics and fail-closed unknown critical data. Library policy vocabulary may grow without a text parser or embedded scripting language.
9. Small dependency surface, native TS, Rust `no_std + alloc`, verification on wasm without OS entropy or a runtime. No general Protobuf/Datalog/regex dependency in the kernel.
10. A hard cut across clients, servers, stored evidence, protocol, fixtures, and caches. No dual Biscuit/Apex verifier and no “try the older verifier on failure.”

Non-requirements: Biscuit wire compatibility, arbitrary upstream Datalog, a text DSL compiler, a general policy engine, cryptographic primitive implementation, a new recovery FSM, an online authorization service, recall of already-read data, or a proof that a disconnected peer knows the latest revocation state.

## 4. Proposed wire format and canonical encoding

### 4.1 Choice

| Option | Judgment |
| --- | --- |
| Deterministic CBOR subset | **Choose.** Standard primitive encodings and existing inspection tools; arrays remove map-order ambiguity; a deliberately restricted codec can be small in both languages. Canonical acceptance must be enforced, not requested as a serializer option. |
| Protobuf | Already present outside the library, but deterministic protobuf is not cross-implementation canonical serialization. Unknown fields, defaults, ordering, and duplicate singular fields require a new strict profile anyway. It retains schema/compiler/runtime weight in a standalone kernel. |
| Bespoke binary/TLV | Could save some bytes and initially some code. It invents every length, unknown-field, and integer rule and loses standard inspection tooling. Savings are not yet measured; insufficient reason to invent another primitive encoding. |

CBOR's deterministic requirements provide the primitive starting point; Apex's restrictions below are **additional protocol decisions**, not properties of generic CBOR. See [RFC 8949 §4.2](https://www.rfc-editor.org/rfc/rfc8949.html#section-4.2). Protobuf's own documentation explicitly warns that deterministic serialization is not canonical: [Proto Serialization Is Not Canonical](https://protobuf.dev/programming-guides/serialization-not-canonical/).

### 4.2 Canonical profile

`C(x)` below means this exact CBOR profile:

* Definite-length arrays, byte strings, UTF-8 text strings, signed integers, booleans, and `null` only. No maps, floats, CBOR tags, indefinite lengths, undefined/simple values, or trailing bytes.
* Shortest integer and length encoding; values in signed 64-bit range. Timestamps and counters are nonnegative and at most `2^63-1`. All arithmetic checks overflow. TS represents these values as `bigint`, including JSON-vector decimal strings.
* Exact array arity for each schema. Optional fields occupy their slot with `null`; omission is not equivalent. Bytes are bytes, not strings containing base64/hex.
* Strict UTF-8. Reject malformed UTF-8 and JS unpaired UTF-16 surrogates at construction. Do not silently normalize Unicode. Opaque labels compare by encoded bytes. Field names use ASCII `[a-z][a-z0-9_.]{0,63}`. No locale-sensitive sorting.
* A set is an array sorted by unsigned lexicographic comparison of each element's **complete canonical encoded bytes**, without duplicates. A field bag is sorted by its ASCII field name and contains no duplicate field names. A list preserves order. These distinctions are schema-defined.
* Paths are arrays of nonempty UTF-8 segments, max 64 segments, max 255 bytes per segment, max 1024 encoded path bytes in the default token profile. Reject `.`/`..`, slash, backslash, NUL, control characters, and empty segments. Resource lookup resolves aliases and supplies the canonical path. A UUID/resource anchor prevents path reassignment from moving authority across roots.
* Unsupported semantic fields, node tags, suites, object kinds, and profiles reject. Do not decode/re-encode and accept a noncanonical original. A bounded reader validates the original bytes before signature verification.

All normative arrays below are ordered tuples, not illustrative JSON objects. `Key = [algorithm, public_bytes]`; v1 algorithm `1` means Ed25519, exactly 32 bytes. `Hash` is 32 bytes. `Claim = [name, Value]`. `Value` is a primitive above, or a one-level tuple of primitives (max eight elements); nested arbitrary objects are forbidden. Grants and expression nodes use their own schemas.

### 4.3 Public token and private delegation package

```text
Token = [Header, [Entry...], Tail]
Header = [1, suite, profile, realm, issuer_key_id, owner_state_hash, token_nonce]
Entry = [Core, chain_signature, holder_transfer_signature, endorsement_signature]
Core = [kind, index, previous_hash, next_key, validity,
        holder_transfer, grants, claims, caveats, endorsement_key, extensions]
Tail = [mode, signature]

validity = [not_before, expires_at]         # Unix seconds; upper bound exclusive
holder_transfer = null | Key
extensions = [[id, critical, payload_bytes]...]  # ascending unique integer id
mode = 0 (delegable presentation) | 1 (sealed presentation)
kind = 0 (authority) | 1 (narrow) | 2 (endorsed narrow)
profile = 1 (owner capability) | 2 (hosted identity) | 3 (direct purge)
suite = 1 (Ed25519 + SHA-256 + this CBOR profile)

DelegationPackage = [1, Token, next_seed]
```

`realm` is a 32-byte application trust-domain identifier, not a caller-supplied hostname used as a trust root. `issuer_key_id = H(D("key") || C(Key))`. `owner_state_hash` is a required admitted state hash for owner/direct-purge profiles; hosted identity uses its admitted account/session authority-state hash. `token_nonce` is 32 cryptographically random bytes, or fixed fixture input.

Entry 0 must have kind 0, index 0, and `previous_hash = H(D("header") || C(Header))`. It supplies a non-null holder key except for explicitly admitted anonymous/bearer hosted identities. Its grants and claims are immutable. Root grants are a canonical sorted unique set of Grant encodings. Claims are a unique-name sorted field bag. Later entries have consecutive indices, the immediately previous entry hash, and kind 1 or 2; `grants` must be empty. Later claims are annotations in that block's namespace and cannot override root claims. Ordinary narrowing preserves the effective holder when `holder_transfer = null`. For kinds 0 and 1, `endorsement_key` and `endorsement_signature` must be null; for kind 2 both must be present. A null holder transfer requires a null transfer signature; a root initial holder also has no transfer signature. A bearer-only parent cannot introduce a holder transfer in v1.

The public Token has **no attenuation secret**. An optional private DelegationPackage transports the final next-key seed; never put this package in an RPC, log, attestation, or public sidecar. A non-extractable next key can instead remain in an application-owned handle. Public verification requires only public-key operations. This costs one terminal signature and a separate handle, but removes private-key derivation/signing from verification and makes public evidence safe to publish without relying on callers to remember to strip a seed.

Offline sharing remains possible: hand the child a DelegationPackage and, for a PoP-bound credential, transfer the holder to the child's key. The package is confidential credential material; encryption/key storage belongs to the caller. No built-in KMS/network client or password-encryption format is introduced.

Text form, only where required: `apx1.` followed by unpadded base64url of canonical Token bytes. Decode strictly; reject padding, whitespace, non-url alphabet, and nonzero unused tail bits. Protocol fields carry raw Token bytes. There is no raw/text auto-detection and no protobuf inside signed Apex bodies.

### 4.4 Versioning

The header version covers framing. The suite fixes every cryptographic detail. The profile fixes the authorization vocabulary and resource/action interpretation. All are signed. New algorithms require a new suite; a verifier's local allow-list selects acceptable suites before crypto. No inference from key length, algorithm fallback, or mixed-algorithm chain in v1.

Unknown critical extensions reject. Unknown noncritical extensions are signed opaque diagnostic data, exposed only as such; they cannot satisfy a caveat or supply a grant. If a future extension changes authorization, it must be critical or require a new profile. Noncritical payload total is capped at 1024 bytes. v1 registers no authorization extension outside the caveat schema.

## 5. Crypto chain and exact signature payloads

`H` in this section is SHA-256, not the repository alias. `D(label)` is the UTF-8/ASCII byte string `Apex/v1/` followed by the exact lowercase label and one zero byte. There are no interpolated lengths in D. All variable-length data below is inside canonical CBOR. `Sign(k,m)` is **ordinary Ed25519 of message m**; it is not Ed25519ph and a KMS adapter must not silently prehash it again.

For an entry core `c_i` and header digest `h`:

```text
h   = H(D("header") || C(Header))
t_i = H(D("core") || C([h, c_i]))

holder_transfer_signature =
  null                                  if i=0 or holder_transfer=null
  Sign(effective_parent_holder,
       D("holder") || t_i)              otherwise

endorsement_signature =
  null                                  unless kind=2
  Sign(endorsement_key,
       D("endorsement") || t_i)         for kind=2

m_i = D("block") || H(C([h, c_i,
                         holder_transfer_signature, endorsement_signature]))
chain_signature_i = Sign(issuer_key, m_i) if i=0
                    Sign(previous_next_key, m_i) otherwise

entry_hash_i = H(D("entry") || C([h, Entry_i]))
Tail.signature = Sign(last_next_key,
                      D("tail") || H(C([h, entry_hash_last, Tail.mode])))
```

All signatures are 64 bytes. An entry's signer is determined by its position, never chosen by the entry. Root verification obtains `issuer_key` from the independently admitted trust context and confirms its key ID. Every entry signs its own next public key, previous hash, validity, all caveats/annotations, and any transfer/endorsement. Including both signatures in the chain signature prevents their replacement. Including the full core in the holder signature prevents moving a child key transfer onto weaker sibling restrictions.

Every newly generated next key uses fresh CSPRNG material. Builders reject reuse of a key visible earlier in their chain; this is defense in depth, not a claim that all malicious signers' key histories are knowable. A next key is an append/signature-link key, **never automatically a PoP holder or owner key**.

Verification checks the terminal signature in both modes. A child cannot truncate to an earlier block and reuse that block's chain signature as a tail signature: the domains and messages differ. The prior tail is not included in the child token. A delegator who retained the parent token/next secret can still exercise its original authority; attenuation cannot retroactively constrain someone who already possessed a stronger credential.

`seal()` signs mode 1 and drops the handle from the returned public object; `append()` only accepts mode 0 plus the matching next-key handle. Receivers of portable authority evidence require mode 1. Sealing is about the published snapshot: it cannot prove every former holder erased a saved parent or next key. A holder retaining that key can create another branch, just as a saved pre-seal Biscuit remains usable. Public sealed evidence is not a reusable bearer authorization when its profile requires separate request PoP.

### 5.1 Ed25519 acceptance profile and agility

Suite 1 requires canonical encodings of both public key A and signature point R, scalar `S < L`, and non-identity prime-order-subgroup A and R. Check group membership through reviewed crypto libraries, not hand-written curve arithmetic. Verify the ordinary RFC 8032 equation on those accepted points. This stricter input domain avoids relying on differences among permissive/cofactored verification APIs. Standard generated signatures satisfy it except negligible cases; a signer producing a rejected signature must return a hard error, not switch suites.

In Rust, use dalek plus curve25519-dalek point validation. In TS, use noble point decoding/subgroup checks followed by verification or WebCrypto verification. Do not assume `zip215:false` and `verify_strict` alone mean identical policies: the current packages expose different checks (`R/ed25519-dalek-3.0.0/src/verifying.rs:367–386`; `T/node_modules/@noble/ed25519/index.js:658–699`). The cross-provider malformed-point suite is a release gate. See [RFC 8032](https://www.rfc-editor.org/rfc/rfc8032.html) and [dalek's verification documentation](https://docs.rs/ed25519-dalek/3.0.0/ed25519_dalek/struct.VerifyingKey.html).

Algorithm agility is an allocated suite namespace, not speculative implementation. P-256-only KMS products cannot sign suite 1. An adapter must demonstrate raw Ed25519 support; a later P-256 suite needs its own signature canonicalization and conformance design. WebAuthn remains an owner/mint-root attestation mechanism, not an arbitrary-byte Ed25519 signer.

## 6. Authorization language and deterministic evaluation

### 6.1 Alternatives and choice

| Alternative | What it buys | What it costs |
| --- | --- | --- |
| Restricted Datalog | Familiar facts and queries; could reproduce much of the current API | Still needs variable binding, provenance, stratification/termination rules, a relation store, and carefully bounded joins. Calling it a subset does not remove these obligations. |
| Typed grants plus a small caveat language | Direct representation of the observed constraints; no derived facts or fixpoint; easy to bound and port | New policy types and deliberate rejection of arbitrary existing Biscuit programs. New semantic operations require a profile revision. |
| Hybrid: typed caveats plus selected Datalog rules | A migration bridge for arbitrary callers | Two evaluators and two provenance models; preserves the largest source of complexity without an observed production requirement. |

**Choose the second.** Flexibility comes from composing small expressions, opaque action names, typed context fields, and new explicit profiles, not from arbitrary executable code. This is a capability verifier, not a replacement for every possible Biscuit application.

### 6.2 Grants and resource matching

```text
Grant    = [Selector, actions]
Selector = [resource_kind, anchor_id, path_segments, descendants]
actions  = sorted unique nonempty array of ASCII action names
```

`resource_kind` is an ASCII identifier of at most 64 bytes; action names are at most 64 bytes. `anchor_id` is exactly 32 bytes identifying the stable owner-governed resource namespace. The application derives it from a domain-separated canonical resource identity, e.g. `H(D("resource") || C([realm, spool_uuid_bytes]))`, never from a mutable display path. Empty path denotes the anchored root; `descendants` is a boolean. A selector matches an exact resolved target, or a segment prefix when `descendants=true`. Byte prefix alone is insufficient: `org/a` must not match `org/ab`.

The adapter supplies the exact requested target and up to 65 authorized ancestry targets, each with its stable anchor and kind. It establishes parentage from admitted resource state. It must not manufacture ancestors from an untrusted string. A grant on an ancestor can match only where the profile's inheritance table allows it. For the Heddle profile, spool ancestry and the thread/context-to-spool relationship reproduce the existing Rust walk (`B/src/resource.rs:96–175`); an owner boundary starts a new anchor unless a separately verified relationship authorizes inheritance. This boundary rule is a proposed tightening and needs explicit integration fixtures.

Action implication is a tiny profile table: `admin ⇒ write ⇒ read`, including reflexivity. All other action strings match only themselves unless allocated in a later profile. In particular, `ci-verdict:write`, `merge`, `approve`, and `purge` receive no implication from `admin`. The exact CI action spelling comes from `B/src/lib.rs:38–43`; action lifting and merge/approval constructors come from `B/src/rules.biscuit:30–32` and `B/src/facts.rs:107–124`. The adapter maps each exact RPC method to the required action and selector. Unknown methods deny; a caller-supplied `required_action` cannot override that mapping.

Root grants are a requested ceiling, never evidence that a self-signed issuer owns the resource. For owner capabilities, each successful match must also be permitted by the issuer's owner-derived mint authorization. For hosted identities, any token ceiling intersects the independently established hosted grant decision. Child `grants=[]` is mandatory; children restrict through caveats. No implicit wildcard is attached to an empty grant list. A hosted identity with no portable grants may prove identity for an explicitly identity-only method, but cannot independently authorize an offline resource operation.

The direct-purge profile requires one entry, one to 64 exact spool/path grants, `descendants=false` on each, only action `purge`, an owner-authority issuer permitted by the admitted owner history, and a holder signature over the exact purge operation. An ordinary device/mint attachment is insufficient. Match only the actual target, never an ancestry target. It has no child blocks and no ordinary `admin` shortcut. This preserves the inspected direct-only/exact-only contract (`C/src/capability.rs:108–199,330–379`; `C/src/operation.rs:140–158`).

### 6.3 Expression schema

`caveats` is a list of expressions, all of which must be true. Array order is preserved and signed. A `Ref` is `[source, name]`, where source 0 is the verifier-created request context and source 1 is an immutable root claim. Names and types are registered by the profile; unknown names or ill-typed constants reject at validation. There is no reference to an arbitrary descendant claim, mutable effective holder, or a dynamically collected set of facts.

| Node encoding | Meaning |
| --- | --- |
| `[0]`, `[1]` | False, true |
| `[2, ref, value]`, `[3, ref, value]` | Exact typed equality, inequality |
| `[4, ref, integer]`, `[5, ref, integer]` | Signed integer `<`, `<=`; dates use integer seconds |
| `[6, ref, values]` | Membership in a sorted unique literal set, at most 16 elements |
| `[7, ref, segments]` | Segment-prefix match on a registered path-valued field |
| `[8, ref, ascii_prefix]` | ASCII byte prefix on a profile-approved text field; never a resource-path substitute |
| `[9, [expr...]]`, `[10, [expr...]]` | All, any; one to 16 children |
| `[11, selector]` | Match the actual request resource against this selector, using the same trusted resolution as grants |

Each comparison requires an existing correctly typed input. A missing optional input makes **every comparison false, including inequality**. A type mismatch is an invalid context/program, not a coercion. No string-to-number conversion, locale comparison, truthiness, floating point, or wildcard regex. Empty allow-lists compile to `[0]`; omission of a constraint is a different builder operation. `all([])` and `any([])` are invalid, so callers cannot obtain accidental truth from a malformed empty construction.

Tuple equality/membership compares a complete ordered tuple, preserving correlation. The current edge request is one tuple `(spool, facet, audience, digest)`, and boundary acceptance is one six-element tuple. Do not split these into independent per-column membership sets, which would authorize their Cartesian product. The context also has typed fields for method, now, creation digest, audience, and resolved resource; specific profiles register additional data. Text-prefix support covers the observed simple expression primitive; builders should use selector/segment matching for resources.

Root claims are typed and unique by name. Repeated audit values such as `amr` are represented by a bounded tuple in the claim, not duplicate field names. A descendant's `agent`, `tab`, `amr`, or display scope stays in that entry's annotation namespace. Accessors return its origin and block index; annotations do not become evaluator inputs. A future step-up requirement must name a verified evidence type, rather than allowing a child to add an `amr` string.

### 6.4 Expressiveness check and deliberate semantic changes

The predicate-by-predicate mapping in §2.2 is the migration checklist; nothing in that table requires a rule engine. These examples cover the nontrivial evaluation shapes:

```text
Expiry:             every block has mandatory not_before <= now < expires_at
Method allow-list:  In(context.method, ["/.../Read", "/.../Observe"])
Method deny:        Ne(context.method, "/.../Purge")
Resource narrowing: Any(Matches(spool_A_prefix), Matches(spool_B_exact))
Edge extent:        Eq(context.edge_request, [spool, facet, audience, digest])
Creation:           All(Eq(context.method, CREATE), Eq(context.creation_digest, d))
Boundary:           Eq(context.original_subject, [kind, account, thread,
                                                 subject, publisher, agent])
CI verdict:         normal grant gate requires exactly "ci-verdict:write"
Self observation:   explicitly registered identity-only ObserveIdentity method
```

Owner-subject facts become an owner grant plus a separate holder proof; artifact facts become a standalone signed statement; presence becomes a narrow externally signed block. These are structured protocol objects rather than facts in a common global pool. That distinction is necessary to retain their provenance.

The current shared agent builder gives `ObserveIdentity` a resource-scope exception for self inspection and treats `Some([])` as deny, while an absent list is unrestricted (`B/src/delegation.rs:83–132`). Encode the exception explicitly in the profile: an identity-only method may reveal only the already verified subject/credential and must never return resource data. Do not silently skip all caveats when the method is empty. H's older CLI skips an empty resource vector and still maps `namespace` to `repo`, unlike the shared `spool` vocabulary (`H/.../device_flow.rs:696–731`; `B/src/resource.rs:31–68`). At the hard cut use one shared builder contract and the current resolved resource model; do not preserve these mismatches.

All v1 expiry is exclusive. Existing TS date checks and C's owner-capability check accept exactly the expiry second; that boundary behavior is intentionally removed (`T/.../biscuit.ts:342–375`; `C/src/capability.rs:368–373`). Current arbitrary client Datalog, old textual `repo` aliases, and unregistered request facts reject. These are stated breaking changes, not accidental losses of compatibility.

### 6.5 Evaluation order and invariant

The public API separates `decode`, `verify_chain`, and `authorize`; successful decoding or signature verification is never an authorization result. Only a verified admitted trust context can construct the input to `authorize`.

1. Check framing, canonical encoding, schema, structural limits, and a computed work bound. Do not perform signature verification or allocate according to an unchecked wire length.
2. Bind the header to an independently admitted realm, profile, issuer and owner state. Reject unsupported suites/profiles/extensions. Structural errors precede cryptographic errors in stable error reporting.
3. Verify the root, ordered chain, every holder transfer, allowed endorsements, and the tail. Compute entry IDs, effective holder, and validity intersection. Reject unknown endorsement authorities even when their self-declared signature is correct.
4. Check owner/mint authority, all relevant revocations, effective time, and profile restrictions. Direct purge, independent-root gates and public-evidence sealing are explicit profile checks.
5. Evaluate every inherited caveat against the same immutable context and root claim view. Grant matching uses one actual method/resource resolution and the root ceiling intersected with external admitted authority. The adapter must check every guarded target of a multi-target request.
6. Verify the exact request/operation PoP when required, atomically consume its nonce or admit its operation ID, and return a decision carrying the validity ceiling, identity/holder provenance, owner-state and revocation-view hashes. Recheck mutable local authorization state at commit/stream checkpoints.

Implementations may short-circuit for speed after preflight, but not change acceptance or the precomputed charge. Within each stage, select the first error in wire/field order. Successful authorization is a conjunction. For a fixed admitted trust/revocation view and request context, appending constraints cannot enlarge the authorized **operation set**. Changing the holder is a separately authorized delegation, not a way to rewrite inherited root facts. This invariant is lost if inherited caveats can see mutable leaf labels; that is why they cannot.

### 6.6 Deterministic bounds

Default Heddle v1 ceilings are normative profile constants; deployments may configure lower ceilings and report them explicitly. They may not silently raise them while claiming the same profile.

| Item | Ceiling |
| --- | --- |
| Canonical public token bytes | 65,536 |
| Entries, including authority | 16 |
| Externally endorsed entries | 4 |
| Root grants; independently admitted grant ceiling; actions per grant | 64; 64; 16 |
| Claims/annotations total; one encoded Value | 64; 1,024 bytes |
| Caveat AST nodes total; depth | 256; 16 |
| Set/boolean fanout; tuple width | 16; 8 |
| Resolved resource targets, including ancestors | 66 |
| Actual guarded targets in one multi-target request | 64; all evaluations share the total work budget |
| Request-context bytes inspected by the evaluator | 16,384 |
| Path segments; segment bytes; encoded path bytes | 64; 255; 1,024 |
| Noncritical extension bytes total | 1,024 |
| Root/chain + holder + endorsement + tail + request signatures | at most `16 + 15 + 4 + 1 + 1 = 37` |

Use an iterative parser and evaluator with bounded explicit stacks. No recursion in the host call stack, fixpoint iterations, derived facts, regular expressions, text parsing, backtracking, or user callbacks during evaluation.

Preflight calculates a public-input work vector, not milliseconds. Count all branches even if a runtime evaluator would short-circuit. Charge every scalar/string/tuple comparison by the maximum bytes it can inspect (sum of operand encoded lengths plus one per component); membership sums every comparison. Grant action tests are charged once per grant, followed by every possible selector/ancestry comparison, for both the root ceiling and independently admitted ceiling. Field lookup uses sorted bounded bags and is charged for its maximum comparisons. Add all decoding bytes, key/ID comparisons and hashing input bytes. Multi-target requests sum every target evaluation into the same budget and verify the shared chain once. Reject above **16,777,216 byte-work units**, 256 AST nodes, or 37 signature checks. Some inputs within individual dimension limits will still exceed the combined bound and must reject consistently. Conformance vectors include the exact bound and one-unit-over case.

This is a conservative algorithmic bound, not an equivalence between one byte and a CPU cycle. Curve validation is charged within each signature's fixed-size work; each suite fixes the allowed algorithms. The parser must also have a linear byte/allocation bound before AST preflight. Request-body hashing occurs in the adapter and is subject to its separate transport-size limit. Owner-history verification and revocation merging are **not free**: adapters must separately bound them (initially at most 256 supplied transitions and a 1 MiB proof bundle, consistent with the scale of current limits in `C/src/limits.rs:14–27`), or use already verified pinned state. An untrusted supplied “checkpoint” cannot bypass history admission.

No verifier outcome depends on host descheduling. Applications may cancel work for availability, but cancellation yields no authorization and is not a language failure. The constants need measurement on low-end browsers and wasm before v1 freeze; no latency or binary-size claim is made here.

## 7. Owner trust, time, revocation, attestations, and PoP

### 7.1 Owner roots and rotation

The kernel takes an `AdmittedAuthority` from the application owner verifier. It includes the realm, profile, admitted issuer key, owner-state hash, allowed grant ceiling, mint-authority validity, permitted endorsement keys/purposes, and revocation indexes. It is not constructed from the token's own assertions. A self-signed token and an embedded owner history cannot establish a new trusted owner.

The owner adapter must start with an independently pinned genesis/owner identity, validate transitions and current recovery state, and resolve the authority allowed at verification time. Owner rotation, key retirement, recovery timelocks, and explicit retained-key windows remain the existing protocol's responsibility (`C/src/owner.rs:34–85,94–110,428–504,621–672`). Apex does not replace this with “accept any key ever in history.” Its `owner_state_hash` identifies the issuance basis; it does **not** demand equality with the latest hash after an unrelated settings change. The adapter establishes ancestry/admission and checks current retirement/revocation policy.

An owner may sign a token directly or authorize a mint key through an owner-signed attachment. That attachment is external evidence, with explicit purposes, resource ceiling, validity and maximum token lifetime. A mint key cannot create a longer-lived or broader credential than its attachment permits. The effective validity includes the mint authorization. Offline verifiers must possess the attachment and admitted owner chain, or already have verified them. Header key IDs are exact lookup hints into this admitted set; no scan through unrelated global roots and no fallback to a server key.

Keep the account tiers explicit. For self-rooted accounts, compromise of Weft may withhold/replay state but cannot mint owner authority. Server-rooted/custodial accounts deliberately give the per-account custodian minting power; a compromised custodian can exercise it. Public/anonymous service tokens use a separately admitted hosted realm and cannot pass an owner-protected gate. The current model distinguishes those tiers (`W/docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md:301–328,489–505`); promising universal “zero server keys” would contradict it.

Hosted mutable grants require special care. Today resource access also consults registry data (`W/crates/weft-hosted/src/hosted_access.rs:400–430`). Keeping that as the sole authority for an operation keeps the hosted server in that operation's trust boundary. The Apex format alone does not fix this. Portable owner-protected operations must require the owner-derived grant evidence; purely hosted identity/access decisions can remain explicitly hosted. Removing Weft as *all* resource authority would require a separate signed-grant migration beyond swapping tokens. Do not quietly include that unbounded project in a “minimal library” estimate.

### 7.2 Time and expiry

Every entry has an explicit half-open interval `[not_before, expires_at)`, with `not_before < expires_at` and nonnegative signed-64-bit seconds. Root lifetime is at most **604,800 seconds** in the Heddle profile, implementing #872's seven-day decision. A child interval must be contained in its parent's effective interval; reject an attempted extension instead of silently intersecting misleading metadata. Direct purge has the same seven-day ceiling, not a newly invented shorter one. Presence has its separate 300-second ceiling. Integrations may mint shorter sessions.

`now` comes from the verifier, never a token fact or signer-supplied operation date. There is no implicit leeway in the kernel. If a transport accepts clock skew for PoP timestamps, that does not extend token expiry. Persist a maximum accepted local time/state where appropriate and use monotonic elapsed time for an open stream, while recognizing that local disk/clock compromise defeats those measures. A device unable to establish an acceptable clock must report that condition; it cannot authorize indefinitely by replaying an old `now`.

Seven days bounds the lifetime of a particular credential under an honest clock. It does **not** bound the authority of a compromised mint key that a stale verifier still accepts: that key may mint fresh credentials until its independently limited mint authorization expires or revocation arrives. Capability TTL, mint-key attachment lifetime, and owner recovery are separate decisions. This is a release-gating product/trust-policy question, not something a token format can solve.

For archival signatures, `signed_at` is the signer's claim, not a trusted timestamp. A currently expired/revoked credential does not become valid because an attacker backdates a new operation. Previously committed operations use independently retained acceptance evidence and causal admission rules; verify their original evidence as history, not as a new grant to execute now. This distinction already exists in API comments on boundary acceptance (`A/proto/heddle/api/v1alpha2/sync.proto:387–404`).

### 7.3 Revocation IDs and the #872 checkpoint contract

```text
block_revocation_id_i = H(D("revoke") || entry_hash_i)
key_id               = H(D("key")    || C(Key))
token_id             = H(D("token")  || C(Token))
authority_id         = H(D("authority") || entry_hash_0)
RevocationEntry      = [kind, realm, owner_namespace_id, target_id]
```

Revocation kind is 0 block, 1 public key, 2 session, 3 credential, 4 owner-capability ID (`authority_id`). Block/key/authority IDs are 32 bytes. Session/credential IDs are exact opaque bytes, at most 128. `owner_namespace_id` is the admitted 32-byte owner/account authority identity, stable across its key rotations; it is not a display username or a token-selected namespace. Root `session` and `credential_id` claims map to these indexes explicitly. `authority_id` is computed after signing, not inserted recursively into its own root core. The old `owner_capability` binding maps to this accessor and to request/operation evidence. Unknown revocation kinds fail settings validation; do not ignore an entry that could affect acceptance.

Check every chain block ID, issuer/mint key, every holder key in the delegation ancestry, every next key used for append/terminal proof, and every required endorser key, plus root session/credential/capability aliases. Revoking an ancestor block kills all descendants because its entry hash remains in their chain. Key revocation deliberately has broader scope than revoking one branch. Sealing changes `token_id`, but not the block revocation IDs. Random token nonce and next keys prevent accidental ID reuse across independent credentials; exact replay keeps the same ID.

For offline distribution, the “checkpoint” is an **admitted owner-signed settings-graph state/frontier and its grow-only revocation closure**, not a fresh periodically signed certificate. Use the existing graph's canonical signed bytes, resource inheritance and merge protocol, not a second Apex counter or distribution service. The adapter supplies a verified set and the ordered `(scope, state_hash)` references used for its inherited view. That view has a digest `H(D("revocation-view") || C(sorted_scope_hash_pairs))` for receipts. Neither a token nor a network peer may choose a weaker view than the verifier has already admitted.

Admission requirements:

* Verify each settings node's owner authority and causal parents under the pinned owner state. Persist the accepted frontier and grow-only revocation union. Merges may add entries and reconcile normal concurrent updates; they may never subtract or “un-revoke.” Issue new credentials instead.
* A remote ancestor state is usable as historical evidence, never as a replacement for newer locally pinned state. Detect an actual attempt to regress the active view, conflicting authenticated state, or invalid merge and fail closed for affected authorization until reconciled. Do not let an unauthenticated peer cause a durable fork alarm by advertising random hashes.
* A DAG can have legitimate concurrent branches. Do not label every divergent hash an attack or choose a “highest hash.” Until the existing signed graph protocol validates a merge/union, quarantine the ambiguous authorization view. Defining that distinction with #879's actual graph code is an integration prerequisite; §2.4 only establishes existing governance storage, not a complete implementation of this contract.
* Attempt a bounded best-effort refresh before dangerous effects when sources are reachable. Network unavailability does not impose a new heartbeat/freshness denial. Known invalid or conflicting signed state is different from no newer state being available. The network attempt has an operational deadline outside the deterministic evaluator.
* Record the view digest and referenced state hashes in each operation's signed authorization evidence. This shows what the actor/verifier used. Seeing a newer state later proves that views differed; **without trusted chronology it does not alone prove malicious suppression or that the actor knew the newer state at execution time**.

The product promise is: **revocations take effect at a peer once it admits them; peers retain and merge that knowledge; offline peers may accept credentials revoked elsewhere until expiry or until knowledge arrives.** No number of signatures proves that an isolated verifier knows the globally latest state. Online Weft must continue to fail closed on an unhealthy revocation cache; deliberate offline policy is not a license to turn an online cache failure into an empty set (`W/.../cache.rs:840–876`).

#872's final comment treats offline purge damage as repairable because hosted Weft retains a canonical copy. Preserve that stated topology assumption, but do not elevate it to a cryptographic guarantee. Pure peer-to-peer deployments, data never uploaded, synchronized deletion propagation, and retention policies need their own analysis. A remotely authorized deletion on another offline peer is not automatically equivalent to full local-machine compromise. Seven days is the chosen operational lifetime; it neither recalls disclosed data nor ensures recovery of every deleted object. See the [final decision](https://github.com/HeddleCo/weft/issues/872#issuecomment-5120696961).

Revocation sets grow without bound over the lifetime of an account. The kernel consumes a locally indexed, verified view; it does not require every request to carry the whole set. The initial view API is an immutable sorted index, admitted and checked for uniqueness outside the request loop, with at most `2^32-1` entries and at most 32 binary-search comparisons per lookup. A token requires at most 56 lookups (16 block IDs, issuer, 16 holder keys, 16 next keys, 4 endorser keys, and 3 aliases); charge all comparison bytes to preflight. The index is trusted state, not an arbitrary callback or unchecked token-supplied list. Capacity overflow is an explicit operational failure, never permission to forget revocations. Settings synchronization may paginate verified history without discarding entries. Safe compaction requires a preserved authenticated commitment and an admitted local checkpoint; deleting old key revocations based solely on token TTL is unsafe when that key can mint new tokens. A portable succinct non-membership proof is **not** in v1. Offline bootstrap to a never-seen large account remains a measurable cost.

### 7.4 Third-party narrowing and standalone attestations

An `EndorsedNarrow` block requires two independent approvals: the previous next key signs the chain entry and an admitted external key signs `D("endorsement") || t_i`. The external key's permitted purpose comes from the trust context, not from its presence in the token. It may assert signed annotations and impose constraints, but cannot add a grant or override identity. The client sends the proposed core hash/payload to the external signer; the response is one signature. The next-key holder then finalizes the entry and tail offline.

Presence is the initial registered purpose. Its exact shape has no holder transfer, no grants, and exactly one annotation `presence = [root_session_id, not_before, expires_at]`; the latter two must equal the block validity. The signed validity lasts at most 300 seconds and is contained within the parent interval. The external key must be admitted specifically for presence in this realm. This block must be final and the tail sealed. These structural checks implement the current terminal presence special case without an exception to PoP provenance (`B/src/facts.rs:814–841,1023–1075`). A compromised presence signer can lie about presence; it cannot create a root grant or a chain block without the parent's append key.

Generic endorsed narrowing can be registered by a future profile or allowed purpose in the initial profile, but unknown purposes reject. v1 does **not** implement free-standing macaroon discharge tokens, arbitrary third-party facts, network callbacks, or nested discharge recursion. There is no current demonstrated caller that needs them. If later required, design audience, parent-token binding, use count and replay separately; do not accept a signed statement merely because its signer is trusted for something else.

Artifact attestation is a distinct object, never decoded as a capability:

```text
AttestationBody = [1, suite, realm, purpose, issuer_key_id,
                   owner_state_hash, statement]
SignedAttestation = [AttestationBody, signature]
signature = Sign(issuer, D("attestation") || H(C(AttestationBody)))

purpose = 1  # artifact
statement = [1, resource_anchor, artifact_kind, digest_algorithm,
             digest_bytes, signer_key, signed_at]
```

Artifact version 1 uses SHA-256 digest algorithm 1, 32-byte digest, a bounded ASCII kind, raw typed signer key, and nonnegative integer seconds. `signer_key` must match the admitted issuer and key ID. All six old artifact predicate values map to this typed body, with repository identity resolved to a stable anchor and an explicit digest algorithm added (§2.2). Verification can return “valid signature by this key” without claiming ownership; an authorized-artifact result additionally requires the owner/mint-purpose attachment and resource binding. It never grants request rights. Maximum object size is 8 KiB, with one signature and no capability caveat program.

The CBOR top-level object shapes and signing domains are disjoint; API fields are typed rather than auto-detecting artifacts, tokens, or private delegation packages. This prevents using an artifact signature as authority or a public evidence package as an attenuation handle.

### 7.5 Request PoP and durable operation proofs

The effective holder is the root holder followed by verified transfers. Every ordinary Heddle authenticated call requires that holder's signature. A root signer, chain next key, device owner key and request holder may be different keys; compare the intended role, not merely whether some signature verifies. Bearer-only anonymous access is an explicit hosted profile and never a downgrade path for a supplied invalid authenticated credential. The API already states that downgrade prohibition (`A/proto/heddle/api/common/contract.proto:117–121`).

The proposed new RPC proof is:

```text
RequestBody = [1, realm, audience, method, resource_bindings,
               body_digest, token_id, owner_view_digest,
               revocation_view_digest, issued_at, nonce]
RequestProof = [RequestBody, signature]
signature = Sign(effective_holder, D("request") || H(C(RequestBody)))
```

Audience is a pinned logical service/recipient identifier, not an unchecked Host header. Method is the full exact method path. Resource bindings are the sorted unique actual target IDs, not display names; at most 64. The encoded RequestProof is at most 8 KiB. `body_digest` is SHA-256 of a domain-separated **exact transport payload** defined by the API: method-specific request bytes excluding the proof/authentication envelope. The verifier must authorize and execute the same decoded payload. If the transport reserializes protobuf, either retain the exact original bytes or specify a separate canonical application signing message; “deterministic protobuf” is insufficient. This API contract and cross-transport fixtures are a cutover gate.

Nonce is 32 random bytes. RPC profile acceptance requires `abs(now - issued_at) <= 120 seconds` with checked arithmetic, plus current token validity. Atomically consume `(realm, audience, holder_key_id, nonce)` before committing effects and retain it through the acceptance window (at least until `issued_at + 120`, with cleanup accounting for clock policy). For idempotent retry, persist the original request digest and result; a matching nonce with a different payload always denies. Resource/action checks and the recorded authorization views must be checked again in the commit transaction when mutable state could have changed. There is no claim of global exactly-once execution across disconnected recipients; audience scoping and application operation IDs define that boundary.

The client records the owner/revocation view it used, but the receiver computes authorization against its own admitted view, at least as new as its local pin. A client-signed old view cannot force rollback. Record both actor and receiver views if they differ; otherwise the receipt would misrepresent the decision.

Durable peer-to-peer operations use an allocated `D("operation")` domain and signed operation ID, destination/resource, exact operation digest, capability ID and used state hashes. They are admitted once by the application's causal operation store, **not** by reusing the 120-second RPC freshness window. Newly applied dangerous effects are authorized at receiver time unless already covered by independently verified historical acceptance. Streaming connections periodically recheck effective expiry, local revocations and owner changes, and require a fresh holder proof when credentials change. This preserves the role of the current rechecks (`Q/src/authority.rs:40–54,111–123`).

## 8. Rust/TS APIs, dependency budget, and wasm

### 8.1 Package boundary and API shape

Proposed packages are `apex-core` (codec, schemas, signature payloads, bounds and pure evaluator), `apex-crypto-dalek` (reviewed crypto adapter and optional local signing keys), and an application adapter in the existing owner/transport crates. The native TS package is `@heddleco/apex`, exposing the same types and vector version. Its byte codec and evaluator do not depend on a protobuf library or wasm. Keep the initial implementation in Heddle's shared-library source/publishing workspace, with the normative schema/vector corpus owned there; locate that actual current workspace before coding because it is not represented coherently by the supplied H checkout (§1.1).

Illustrative Rust API, with the semantics above normative and exact public type names provisional:

```rust
// core::future::Future; no async-trait, runtime, or private-key accessor.
pub trait AsyncSigner {
    type Error;
    fn key(&self) -> Key;
    fn sign(&self, request: SigningRequest<'_>)
        -> impl core::future::Future<Output = Result<Signature, Self::Error>>;
}

// request.message is the exact Ed25519 message from section 5 or 7.
// purpose is informational for signer policy; it does not alter those bytes.
pub struct SigningRequest<'a> {
    pub suite: Suite,
    pub purpose: SigningPurpose,
    pub message: &'a [u8],
}

pub trait CryptoVerifier {
    fn sha256(&self, bytes: &[u8]) -> Hash;
    fn verify_ed25519_strict(&self, key: &Key, msg: &[u8], sig: &Signature)
        -> Result<(), CryptoError>;
}

decode_canonical(bytes, limits) -> Result<ParsedToken, Error>;
preflight(parsed, context_shape, profile) -> Result<WorkBound, Error>;
verify_chain(parsed, admitted_authority, crypto) -> Result<VerifiedToken, Error>;
evaluate(verified, immutable_context, admitted_authority, revocations)
    -> Result<PendingAuthorization, Error>;

mint(root_input, issuer_signer, next_key_handle, nonce, crypto).await
    -> Result<DelegableCredential<NextHandle>, Error>;
attenuate(parent, restriction, new_next_handle, optional_holder_signer,
          optional_endorser, crypto).await
    -> Result<DelegableCredential<NextHandle>, Error>;
seal(credential, crypto).await -> Result<PublicToken, Error>;
sign_attestation(statement, signer, crypto).await -> Result<Attestation, Error>;
```

An async signer may wrap non-extractable hardware/WebCrypto, an HSM, or a network call. Key discovery/rotation occurs before preparing a signing request; a changing remote alias cannot silently replace the key after `key()` was read. Verify every returned signature against the frozen advertised key before returning a token. A local dalek signer implements the same interface with an immediately ready future. The core imposes no `Send` requirement; executor-specific adapters can add it. Signing errors, cancellation, and retries do not produce partially usable credentials.

Also expose the state-machine API: `prepare_core → attach_holder/endorsement_signatures → prepare_chain_signature → finalize_entry → prepare_tail → finalize_token`. A pending object's canonical bytes are immutable owned buffers. Never hand out a mutable byte array and later sign a changed version. Each stage checks the signatures and exact pending digest; completion consumes that pending instance. This is essential for hardware confirmation flows, remote signers, and concurrent JS code, not an optional internal implementation detail.

```typescript
interface AsyncSigner {
  readonly key: Key;
  sign(request: Readonly<{
    suite: 1;
    purpose: SigningPurpose;
    message: Uint8Array; // a defensive copy of the frozen message
  }>): Promise<Uint8Array>; // exactly 64 bytes
}

const signer = webCryptoSigner(nonExtractableEd25519PrivateKey, publicKey);
const credential = await apex.mint({root, issuer: signer, next, nonce});
const child = await apex.attenuate(credential, {
  validity, caveats, transferTo: childHolder,
  holderSigner, next: childNext
});
const publicToken = await apex.seal(child);
const verified = await apex.verify(publicToken.bytes, admittedAuthority);
const pending = apex.evaluate(verified, context, revocationView);
// Transport verifies PoP and atomically admits nonce/operation before effects.
```

TS integers are `bigint`; byte inputs are copied at trust boundaries and never alias a caller-mutable buffer. Root/holder/mint keys are nominally distinct wrapper types. A private delegation package requires an explicitly named export operation; `toBytes()` on a public token cannot accidentally export the seed. Raw seeds are zeroized best-effort when local handles are dropped; JS cannot promise physical erasure. No exception message or `Debug` implementation prints a seed/package.

`PendingAuthorization` carries the required PoP key, token ID, target/action, effective expiry, work bound and state references. It is **not** permission to perform an effect until the adapter verifies the proof and atomically applies replay/state checks. No pure library can consume a durable nonce in a database transaction. Make that responsibility obvious in type names and in one shared transport adapter rather than duplicating it at call sites. Typed extraction is only available from `VerifiedToken`; unverified inspection uses an explicitly untrusted display type.

Error families are stable small enums: `Malformed`, `NonCanonical`, `UnsupportedVersion/Suite/Profile/CriticalExtension`, `LimitExceeded`, `UntrustedIssuer`, `InvalidSignature`, `InvalidTransfer`, `InvalidEndorsement`, `InvalidProfileShape`, `Expired/NotYetValid`, `Revoked`, `OwnerStateConflict`, `CaveatFailed`, `NoGrant`, `MissingContext`, `BadProof`, `Replay`. Diagnostic field paths/block indices are optional local data. Remote responses need not reveal which private caveat or revocation caused rejection.

### 8.2 Allowed dependency list

The budget applies to the **new Apex library graph**, not all four applications. Existing transport/protobuf/WebAuthn/owner-history code has its own dependencies. Defaults must not pull those into the kernel. Version lines below are proposed initial pins matching the inspected contemporary stack; an implementation must resolve an audited lockfile and verify the feature graph before declaring the budget met.

| Package/layer | Direct dependencies allowed |
| --- | --- |
| Rust `apex-core` | **None** beyond `core` and `alloc`. Handwritten restricted CBOR and strict base64url/hex utilities; no general serializer/parser. Crypto supplied through the narrow provider. |
| Rust `apex-crypto-dalek` | `ed25519-dalek = 3.0.x` with defaults off and `alloc,zeroize`; `curve25519-dalek = 5.0.x` with defaults off and `alloc,zeroize` for point validation; `sha2 = 0.11.x` with defaults off; `zeroize = 1.x` with defaults off and `alloc`. Optional `fast` enables precomputed tables after measuring size. |
| Rust app adapter | Apex crates plus the already necessary application owner/API types. It must not re-export Biscuit or acquire a Datalog/CBOR framework. WebAuthn/P-256/protobuf stay here, outside the Apex budget. No new third-party adapter package is pre-authorized by this design. |
| TS runtime | **Only `@noble/ed25519 = 3.2.0`**, for point checks and portable Ed25519 verification. Native WebCrypto supplies SHA-256/SHA-512 and external signing. No runtime dependency on `@noble/hashes`, protobuf, a CBOR package, wasm glue, a bundler, or Buffer polyfill. |
| Conformance tooling | Rust built-in test harness; JS/Node built-in test runner; npm dev dependency **`typescript`** only. Store canonical fixtures in a simple line-oriented ASCII format described below, so a JSON parser is unnecessary in the Rust harness. Existing application test frameworks may run integration tests but are not library dependencies. |

Allowed Rust **transitive/build/target** crates for that selected crypto graph, exhaustively by package name: `ed25519`, `signature`, `subtle`, `cfg-if`, `digest`, `block-buffer`, `crypto-common`, `hybrid-array`, `typenum`, `cpufeatures`, `libc` on targets where CPU detection needs it, `rustc_version`, `semver`, `curve25519-dalek-derive`, `proc-macro2`, `quote`, `syn`, and `unicode-ident`, plus the four direct crypto crates above. The derive/proc-macro chain is a dalek target-specific build dependency, not a promise of a macro-free Rust dependency graph. The optional fiat backend is excluded (`fiat-crypto` would require an explicit budget change). No optional `serde`, PKCS#8/PEM, random generation, batch verification, legacy compatibility, or digest-signature feature is enabled.

This list comes from the cached manifests for `R/ed25519-dalek-3.0.0/Cargo.toml:62–99,130–174`, `R/curve25519-dalek-5.0.0/Cargo.toml:92–130,154–165`, `R/sha2-0.11.0/Cargo.toml:45–80`, and their dependency manifests; the digest and target-detection branches are visible in `R/digest-0.11.3/Cargo.toml:42–90` and `R/cpufeatures-0.3.1/Cargo.toml:57–71`. It is a proposed allow-list, **not a generated/resolved Apex lockfile**. CI must reject an unexpected package/version family rather than silently treating transitive growth as free. Current B already brings dalek 3 while Biscuit brings dalek 2 (`W/Cargo.lock:2379–2393`; `U/Cargo.toml:119–123`), which makes convergence plausible. SHA-2 0.10 and other stacks can still remain in application dependencies, as C's manifest demonstrates (`C/Cargo.toml.orig:35–38`); removing Biscuit does not prove a single-crypto-stack workspace.

The TS dependency currently exists in T (`T/package.json:54`). Use noble's async APIs with WebCrypto hashing, not an implicit global sync-hash override. Its documented async API avoids another hash package: [noble-ed25519](https://github.com/paulmillr/noble-ed25519). Browser external signing follows the [Web Cryptography API](https://www.w3.org/TR/webcrypto/). Actual supported browsers must be tested; absence of Ed25519 WebCrypto produces a clear unsupported-signing error, not an export of a private key to JS as a fallback. A public-only verifier can still use noble where WebCrypto hashing is available.

Entropy is caller-supplied: 32 secure bytes per nonce/next seed from the host CSPRNG, or a host-generated non-extractable key handle. No implicit `rand`, `getrandom`, OS clock, HTTP client, KMS SDK, `tokio`, `async-trait`, `chrono`, `time`, `thiserror`, `prost`, or `regex` in Apex. A KMS adapter belongs to the host and takes responsibility for credentials, retries, exact raw-signing mode and latency.

### 8.3 `no_std`, wasm, and size gates

`apex-core` supports `#![no_std]` with `alloc`; a no-allocation embedded API is deferred. Verification needs no entropy, clock read, filesystem, executor, network or private key. Native crypto must compile under `no_std + alloc` and wasm32; hashing and signature verification are deterministic synchronous operations there. Async signing uses `core::future` and does not require a runtime in the core.

Native TS is the browser default; there is no mandatory Rust wasm bundle. Applications wanting Rust wasm can compile the kernel/crypto adapter and supply their own thin ABI wrapper. `wasm-bindgen` is **not** in the initial library dependency budget. Contrast the current C wasm target, which enables Biscuit's entropy backend even for verification (`C/Cargo.toml.orig:41–48`). Do not promise a ready-made JS wasm wrapper while excluding its glue dependencies.

Measure clean build time, incremental build time, native stripped verifier size, wasm size, compressed TS import size, parse/evaluate time, 1/8/16-block verification time, and allocations. Report Apex alone and whole-application deltas separately. Provisional targets: under 32 KiB compressed TS including its crypto import and under 128 KiB optimized verifier wasm, excluding owner-history/API adapters. These are investigation gates, **not measured results or correctness reasons to weaken crypto**. If small-code and fast-table dalek builds differ materially, publish both feature configurations and their exact dependency lists.

## 9. Security analysis

### 9.1 Threat model and invariants

Assume standard Ed25519 unforgeability and SHA-256 collision resistance; an honest verifier and its pinned owner/revocation state; an acceptable verifier clock; and correct application mapping of the actual operation/resource. Attackers may control token bytes, hold weaker delegated credentials, replay/reorder/truncate blocks, control network relays, present stale signed histories, run a malicious external endorser, and submit concurrent requests. Test those capabilities without granting attackers the owner's secret key by assumption.

| Invariant | Mechanism and important limit |
| --- | --- |
| Only admitted roots grant authority | Header key ID is resolved through a pinned owner/custodian context; embedded keys/histories cannot create trust. Initial TOFU enrollment remains a separate risk. |
| Appending cannot widen scope | Root-only grants, conjunctive inherited constraints, child-contained validity, immutable context/root claim references, no derived facts. The parent holder retains any parent credential it saved. |
| Blocks cannot be removed/reordered/spliced by a child | Signed predecessor hash, index, next key, full header binding and a distinct signed tail. A party holding an earlier append key can intentionally create a sibling branch. |
| Holder transfer is explicit and restriction-bound | Parent holder signs the entire new core, then the chain signer binds that signature. Changing recipient, validity, constraints or next key invalidates the transfer. |
| Tokens cannot assert the request | Separate typed context namespace, constructed from resolved/executed request data; no token-defined request facts or rule heads. Bad application resource resolution is still fatal. |
| Purpose cannot be confused | Separate signing domains, object schemas, profiles, realms and audiences for blocks, tails, transfers, attestations and requests. Profiles are part of every chain signature via the header digest. |
| Revocation knowledge does not regress | Persisted admitted settings frontier/union and current-state checks; ancestor/key/alias revocation. This guarantees use of known revocations, not discovery of unseen ones. |
| Offline evidence is publicly verifiable | Public tokens contain no delegation secret, sealed evidence has a terminal signature, algorithms and signer roles are explicit. Sealing cannot erase retained private material. |
| Evaluation terminates deterministically | Finite AST, structural limits, fixed crypto suite, preflight work vector and bounded adapter inputs. Runtime cancellation denies; scheduler delay never changes the logical answer. |
| Request proof is not replayable as a different action | Binds token, audience, method, resource, exact body, nonce and state views; replay admission is atomic with effects. A compromised holder can sign new allowed requests. |

Out of scope as guarantees: a malicious verifier enforcing restrictions on itself; cryptographic recall of plaintext; availability against a withholding server; universal real-time offline revocation; proof that a root signer is not compromised; cross-recipient exactly-once execution; trustworthy timestamps from an ordinary signer; or recovery from a corrupted local trust database without an independent pin/backup.

### 9.2 Pitfalls avoided and remaining risk

* **Fact-origin confusion:** no any-block global fact bag; grants and identity have an immutable origin. External assertions do not acquire authority merely by sharing a predicate name. Current code's special authority queries and reserved-fact scans show why this matters (`B/src/facts.rs:367–438,1003–1018,1128–1232`).
* **Accidental policy success:** there is no general `allow if true` policy stage that an integrator can mistake for resource permission. The API distinguishes chain verification, caveats, grant admission and committed request authorization. Existing shared authorization still delegates permission decisions after extraction (`B/src/lib.rs:304–316`; `B/src/facts.rs:644–710`).
* **Bearer attenuation mistaken for recipient delegation:** possessing an append secret is not possessing the holder key. Restriction changes and holder changes have different signatures. The parent signature covers the whole hop, avoiding a transfer signature reusable across differently restricted sibling blocks.
* **Third-party discharge confusion:** v1 uses exact parent-bound countersignatures and purpose admission; it has no recursive/free-floating discharge search, verifier callback or universal “trusted key” bucket. Supporting an arbitrary attestation later is a security feature, not adding one claim name.
* **Macaroon trust tradeoff:** Apex chooses asymmetric public verification and deliberately pays for signatures and point-validation dependencies. A symmetric root-MAC design would put root-verification secrets into distributed verifier trust and require a different key-custody model. It is not a cheap equivalent for independently verifying owner-signed public evidence.
* **Encoding/signature ambiguity:** canonical bytes, strict integers and points, exact array schemas, domain separation, no protobuf reserialization for signed content. Different valid Ed25519 provider behavior is tested adversarially, not assumed equivalent.
* **Path and tuple bugs:** stable resource anchors, segment matching, exact composite context tuples, no cross-product of independently accepted columns, and shared RPC/resource resolution. Renames/reparenting require admitted resource-state semantics, not a string rewrite.
* **Privacy:** Apex is signed, not encrypted. Claims, caveats, resource names and delegation metadata are visible. Do not put a signup email or confidential model/provider detail in public evidence merely because an old Biscuit could carry it; adapters should minimize claims for each use.
* **Resource exhaustion before crypto:** size/arity/depth checks precede allocating containers or hashing attacker-advertised lengths. Byte comparisons and owner/revocation proof processing have separate bounds. Signature verification is still the expensive operation; rate limiting belongs at the transport boundary.
* **Cached success becoming stale:** cache cryptographic parsing by canonical token hash, but key authorization decisions by owner view, revocation view, request context and time ceiling. Never reuse a prior `allowed=true` after owner rotation, grant change, expiry or a new revocation. No descendant borrows the parent's cached permission result.
* **TOCTOU and replay:** library success cannot replace a database/operation-store admission transaction. Multi-target RPCs and changing streams must retain all their gates. Existing method metadata and proofs are more than token plumbing (`A/proto/heddle/api/common/contract.proto:60–89,117–121`; `Q/src/authority.rs:30–123`).

The largest new cryptographic risk is the custom chain protocol and its composition with owner state, not the choice of an audited Ed25519 crate. It needs independent protocol review before production. A smaller dependency graph does not by itself mean a smaller security risk.

## 10. Cross-language conformance and validation plan

One versioned corpus is the normative contract for Rust, TS, native crypto, browser WebCrypto and any later KMS adapter. Both implementations read the same immutable fixture files; neither generates the other's expected outputs during the conformance test. Add manually inspected intermediate payloads and published primitive test cases so that agreement between two implementations is not mistaken for correctness.

Use a small ASCII fixture syntax: `name=value` lines; lower-case hex for every byte field, unsigned canonical decimal for counts, restricted ASCII enumerations for expected outcomes, and `#` comments. No duplicate fields, escapes, multiline values, or implicit whitespace trimming. Structured inputs/contexts are canonical CBOR encoded as hex plus human-readable comments. A standalone documentation dump can be generated later, but is not the normative input. This keeps `serde_json` and a JSON-number interpretation out of the core test dependency graph.

Each positive fixture records fixed **test-only** seed(s), public keys, nonce, header/core CBOR, every holder/endorsement/block/tail signing message, every signature, entry hashes, revocation IDs, complete raw/text token, admitted trust/revocation view, request context/proof, effective subject/holder/expiry, exact work vector and expected result. Source seed keys are visibly labelled unsafe for production. No genuine non-extractable key is exported to generate a fixture; WebCrypto imports fixed test seeds only in tests and separately tests a generated non-extractable signer.

Required groups:

1. **Encoding:** shortest integer edges at 23/24, 255/256, 65,535/65,536, `2^53±1`, signed-64-bit limits; empty bytes/text versus null; malformed UTF-8 and JS surrogates; tags/floats/maps/indefinite lengths; truncated inputs, excessive arity/depth/length, trailing data; duplicate/unsorted sets and fields; noncanonical base64url tail bits. Round-trip must reproduce the exact accepted bytes.
2. **Crypto chain:** root only; 1/8/16 entries; deterministic same-input Ed25519 output; header tamper; transplant across realm/profile/owner state; reorder, drop-middle, truncate, append, wrong next key, wrong index, reused keys; tail mode tamper; missing/bad terminal signature; sealed public evidence rejection when unsealed. Never rely on a retained parent being unusable.
3. **Ed25519 boundary:** noncanonical public/R encodings, small-order points, mixed-order/torsion points, identity, `S=L` and greater, invalid lengths, all-zero signatures, valid and invalid standard vectors. Confirm the same outcome with dalek, noble and WebCrypto after the common strict point checks; an adapter that accepts extra cases is not conformant.
4. **Authority/provenance:** child user/staff/right/AMR injection, duplicate root fields, unknown required fields, owner/mint/holder-role swap, unadmitted root, wrong genesis/realm, retired mint key, retained key outside its window, custodial realm used as owner realm, server-key fallback, unsigned DB grant presented as owner authority.
5. **Caveats and rights:** every row of §2.2; all action implications and CI/merge/approve exclusions; methods omitted/unknown; optional context absent including `Ne`; every node; empty/absent allow-list distinction; resource exact/prefix/ancestor boundaries; alias resolution and moved resources; tuple cross-product denial; `ObserveIdentity` exception returns no resource data; independent-root checks reject delegated equivalents.
6. **Monotonicity properties:** enumerate/generate bounded contexts and append restrictions; every accepted child operation must be accepted by its parent under the same authority/time/revocation view, ignoring the intentionally changed holder identity. Also demonstrate that a saved parent still authorizes its original scope. Random generation uses a deterministic test harness, not production randomness.
7. **PoP transfer and request replay:** wrong parent key, transfer moved onto weaker sibling caveats, inherited holder, revoked intermediate holder, next key used as holder, token swap, body/method/audience/resource swap, race between duplicate nonces, clock boundaries, idempotent retry with changed payload, new owner state between check and commit, credential replacement in a live stream.
8. **Third-party/artifact:** trusted purpose versus merely known key, changed proposed core, wrong parent, presence over 300 seconds, nonterminal presence, added grants/holder changes, forged root identity, artifact accepted as capability, wrong artifact kind/digest/signer/realm, claimed old signing time after key revocation.
9. **Revocation and owner history:** ancestor ID kills all descendants but not an independent root; branch-specific ID; session/credential aliases; mint, holder and endorser key revocation; inherited spool revocation; grow-only merge; attempted un-revocation; active-view rollback; authenticated conflicting branches; unauthenticated fake frontier does not poison persistent state; valid merge of concurrent updates; owner recovery/rotation and chain-capacity limits.
10. **#872 negative case with a fixed answer:** an isolated verifier with an otherwise valid token and old admitted revocation view **accepts** a token revoked only in an unseen new view. After admitting that view it **rejects**. Known fork/rollback denies. No heartbeat age alone denies. The operation records the exact view used. Separately test an unhealthy online cache denies, so the two behaviors cannot be conflated.
11. **Bounds and load:** exactly-at/one-over each byte/count/work limit; large literal memberships and ancestry fanout; no stack exhaustion; evaluator result unchanged under scheduler delays; max admitted owner-proof work; malformed input rejected before large allocations/signature work. Check Rust/TS work vectors exactly, not just eventual success.
12. **Provider/platform matrix:** native Rust, `no_std + alloc` build, wasm verifier, browser main thread/worker and the product's supported browsers, Node TS; external Ed25519 signer returning errors/wrong key/wrong mode, cancellation and delayed completion; real KMS qualification only for selected supported products. No advertised KMS support based solely on an `async` function signature.

Use deterministic differential fuzzing over codecs and evaluators, then coverage-guided parser fuzzing in a separately reviewed CI harness. That future harness's packages require their own explicit tooling allow-list; it must not expand the runtime budget. Before cutover, independently review the protocol and adapter boundaries, benchmark worst accepted inputs, and exercise actual end-to-end authorization through CLI, Tapestry and hosted/thread APIs. None of those tests has been run in this research-only task.

## 11. Migration and hard-cut sequencing

### 11.1 Ownership and call-site map

| Repository/package | Required change | Current evidence |
| --- | --- | --- |
| `api` common credentials and RPC proofs | New explicitly versioned Apex credential/proof/authority-evidence contract; exact raw byte representation; body-signing contract; no legacy selector scan or dual decoder | `A/proto/heddle/api/common/contract.proto:8–25,60–89` |
| `api` identity/owner/portable-operation contracts | Rename/document token-specific fields, allocate new evidence versions, keep owner history separate, define typed artifact/operation domains and signed state-view references | `A/proto/heddle/api/common/contract.proto:71–89`; `A/proto/heddle/api/v1alpha2/sync.proto:369–404,449–455`; `A/proto/heddle/api/v1alpha2/thread.proto:341–347` |
| Shared `heddle-biscuit-verifier` | Replace with Apex codec/chain/evaluator and Heddle profile adapter; port typed identity/right extraction, PoP provenance, edge and presence constraints; remove Datalog builder/query strings | `B/src/lib.rs:189–325`; `B/src/facts.rs:367–543,814–1075`; `B/src/delegation.rs:65–134`; `B/src/edge.rs:548–609` |
| `heddleco-capability-verifier` | Keep owner/recovery/passkey validation; replace subject Biscuit, thread-control sealed token, boundary/creation caveats with typed Apex evidence; require exact token binding | `C/src/capability.rs:254–325`; `C/src/thread_control_authority.rs:21–50,206–250`; `C/src/boundary_authority.rs:223–251`; `C/src/creation.rs:226–237,341–365` |
| `heddle-thread-api` | Carry raw public token and owner evidence, sign exact request body/audience, centralize nonce/state rechecks; resolve owned-device base64/raw mismatch | `Q/src/credentials.rs:20–151`; `Q/src/authority.rs:30–123` |
| `weft-authz` | Replace root minting/builders, key selection, token parsing/extraction, delegation/edge/presence/artifact helpers, cache IDs and revocation aliases; enforce seven-day ceiling | `W/crates/weft-authz/src/biscuit.rs:48–82,591–784,894–909,1141–1302`; `W/.../keys.rs:1–35,200–219`; `W/.../cache.rs:538–580,840–876,923–959`; `W/.../artifact.rs:24–94` |
| Weft hosted/resource/owner adapters | Continue correct principal/key binding, current grant checks and independent-root gates; add admitted owner revocation views, commit-time checks and receipts; do not grant rights from token labels | `W/.../cache.rs:695–746`; `W/crates/weft-hosted/src/hosted_access.rs:35–51,400–430`; `W/crates/weft-hosted/src/server/hosted/auth.rs:235–240`; `W/crates/weft-registry/src/pg_hosted_registry/owner_governance.rs:562–621` |
| Heddle CLI and local daemon/repo | Replace root mint and agent derivation, untrusted local display parser, credential storage/text prefix, request proofs and owner-purge calls; unify path/method builder behavior | `H/crates/cli/src/hosted_runtime/root_mint.rs:69–106`; `H/.../device_flow.rs:656–746`; `H/.../auth.rs:555–639`; `H/crates/repo/src/owner_authorization.rs:21–23,62–104` |
| Published/historical hosted-client | Identify actual supported consumers before publishing changes; if retained, port the analogous root/delegation code. Do not edit a registry cache or resurrect an absent local crate as an accidental migration task | `L/src/hosted_runtime/root_mint.rs:81–106`; `L/src/hosted_runtime/device_flow.rs:690–778`; source-version caveat in §1.1 |
| Tapestry | Replace native Biscuit protobuf encoder and owner subject encoder with native Apex; WebCrypto signer/holder/next-key handles, session persistence, tab delegation, artifact fixtures and public-evidence UI | `T/src/lib/client/biscuit.ts:84–181,199–248,445–515`; `T/src/lib/client/auth-flow.ts:107–113,281–285`; `T/src/lib/owner-authorization/subject-biscuit.ts:69–120` |
| Old `weft-capability-verifier` | No current crate to port; remove old references in docs/fixtures where relevant and keep the new shared ownership clear | Historical deletion `W@15931be9c`; current dependency `W/Cargo.toml:68–69` |

The supplied H/W/T/A versions must be reconciled before implementation. In particular, W uses newer published shared crates than H pins, and T's older subject/tab formats cannot be used as the expected result for the new verifier. Package provenance/release source is a prerequisite, not a reason to guess paths or change cached registry files.

### 11.2 Order

1. **Freeze the contract in `api` first.** Resolve profile action/resource semantics, exact request bytes, raw token fields, owner attachment/state references, revocation view and replay rules. Allocate new protocol/evidence versions and reject prior versions. Publish schema/types and the first golden corpus for downstream implementation, without enabling a half-migrated serving path. Retire old schema fields/versions explicitly rather than assigning old wire tags unrelated meanings.
2. **Build/review the shared Heddle packages.** Implement Rust core/crypto adapter, TS package, vectors and typed Heddle profile together. Port current owner verifier and thread API against the new API contract. Qualify supported signing providers. Obtain independent protocol review while change is still cheap.
3. **Integrate Weft and local Heddle.** Port all mints, caches, public evidence, request proof handlers, direct purge and revocation-view handling. Replace every 30-day capability policy with the agreed seven-day maximum while allowing shorter purpose TTLs. Keep application visibility and hosted grants in their explicit adapter layer. Exercise the new path in an isolated integration environment using only Apex fixtures/credentials; a test comparison harness may exercise old behavior, but no serving dual verifier is shipped.
4. **Integrate Tapestry/CLI and any supported hosted-client.** Use the shared builders and vector corpus, not a third independent scope policy. New session/tab issuance must actually interoperate with the shared verifier. Browser non-extractable keys remain under application custody; exportable append keys are stored as private handles/packages, separate from public tokens.
5. **Make the cut atomically at the protocol boundary.** Stop old issuance/serving admission, close/re-authenticate long-lived streams, invalidate old sessions/tokens/caches/quotas tied to Biscuit IDs, reset the disposable pilot's signed evidence and local browser/CLI stores, and activate the coordinated Apex server/client/protocol versions. Old clients receive an explicit unsupported-version/re-authentication response. Offline old clients cannot continue syncing until upgraded and re-enrolled. A maintenance window is simpler than a covert compatibility layer.
6. **Remove the old implementation/dependencies.** Delete Biscuit builders/parsers, old subject encoders, old fixture corpus from active tests and unused macro feature workarounds. Remove direct Biscuit dependencies and inspect the complete resolved graph for surviving transitives and duplicate crypto stacks. Protobuf remains where the application API still needs it; removal from Apex is not removal from the product. Publish the new supported package/version matrix and operator reset procedure.

Stored signed evidence is part of the hard cut. Replacing a token inside an old operation invalidates signatures and cannot preserve historical claims of acceptance. Because the pilot is disposable, the default is to reset those stores and recreate authorized evidence. If some data must be retained, export only non-authoritative application content and explicitly reauthorize/reimport under the new protocol; do not relabel Biscuit evidence as Apex or trust an old signature through a hidden migration shim.

Rollback after new authoritative operations is also a protocol/data event. For this pilot, rollback means resetting the isolated pilot state and returning to the previous coherent release. It does not mean letting the old release accept new Apex history, or introducing a permanent “try both” decoder. Plan the reset/export before cutover.

### 11.3 Completion gates

* Every inspected predicate/caller in §2 has a port, an explicit retirement decision, or a demonstrated non-production-only status; current code/source versions are pinned coherently.
* Rust and TS produce identical normative bytes and all providers have identical malformed-input outcomes. Every signature payload is independently inspectable and reviewable.
* Owner identity cannot be established by embedded history, a raw public key selector, a token boolean, or hosted server fallback. Direct purge remains exact/direct. Hosted-only authority is labelled honestly.
* #872 acceptance case, clock assumptions, maximum mint-authority lifetime, signed graph merge semantics, online-cache failure behavior and durable revocation persistence are tested and documented.
* All real transport methods bind and enforce the actual body/target, including multi-target calls, streams and public-if-unauthenticated methods. Nonce/operation admission is atomic with effects.
* Structural/work limits and size/dependency gates pass measured builds. Independent security review has no unresolved high-severity findings. “Both implementations agree” alone is insufficient.
* An active dependency/source scan finds no Biscuit parser/verifier reachable through the new credential path. Old pilot credentials, evidence stores and streams are invalidated together.

## 12. Effort, risks, and comparison with retaining Biscuit

### 12.1 Estimate

These are planning ranges for experienced engineers familiar with the existing owner/transport model. They include cross-language implementation and negative tests, exclude a new general authorization model, and assume a disposable pilot rather than preservation of old signed history.

| Component | Engineer-weeks | Main uncertainty |
| --- | ---: | --- |
| Canonical codec/schema and API contract | 2–3 | Strict rejection parity, request body canonicalization, coherent package ownership |
| Rust/TS chain, async signing, provider parity and sealing | 3–4 | Strict Ed25519 profile, holder/append separation, real external signer behavior |
| Typed grants/caveats and bounded evaluation | 2–3 | Resource/ancestor semantics, method registry, expression/context parity |
| Owner/revocation adapters and state evidence | 4–6 | Actual #879 graph admission/merge behavior, offline view persistence, mint-key lifetime policy |
| Four-repository integration and hard cut | 5–9 | Published/local version skew, transport/stream paths, pilot state disposal, active hosted-client consumers |
| Shared vectors, adversarial testing, performance and independent review | 4–9 | Review findings, provider discrepancies, end-to-end authorization races |
| **Total** | **20–34** | Re-estimate after contract/protocol review |

With two engineers and an available independent reviewer, roughly 10–17 calendar weeks is a reasonable initial planning envelope; it is not a parallelizable arithmetic promise. Security review and the API/profile decisions sit on the critical path. A toy mint/verify demo could appear much sooner and would not count as a replacement.

The biggest risks, in order, are owner/resource semantics drifting between versions; request/evidence/revocation integration; the new crypto protocol escaping review with a subtle binding bug; provider-specific Ed25519 acceptance; and a “minimal” language expanding to accommodate unspecified arbitrary Datalog callers. Size savings are secondary until those are closed. If signed portable grants beyond the present owner-protected surfaces are added to scope, estimate them separately.

### 12.2 Keep Biscuit with a pinned fork/vendor patch

| Dimension | Retain/patch Biscuit | Apex proposal |
| --- | --- | --- |
| Immediate build repair | Patch the feature-gating issue or enable the macro feature; pin the fork/revision. Small and reviewable relative to replacing the protocol. | Requires a new library and integration, not an immediate build fix. |
| External signing | Keep current native TS approach; a supported Rust async signing seam needs a fork/API change and payload-contract tests. | Prepared immutable payloads and async signer are first-class from v1. |
| Rust/TS wire parity | Existing protobuf representation remains; can standardize one emitter and compare payloads, but canonical cross-language encoding requires an extra profile. | Canonical accepted byte representation is part of the protocol. |
| Dependency/build weight | Patch unnecessary features and possibly crypto versions; still carries generic language/wire/crypto machinery. Actual savings require measurement. | Smaller intended graph and one selected token crypto family; application dependencies still remain. |
| Evaluation behavior | Existing fact/iteration limits plus a very large wall-time backstop; a fork can remove/disable the time budget and constrain programs without a wire replacement. | Finite typed AST and statically charged work by construction. |
| Expressiveness | Preserves arbitrary Biscuit programs and a broader ecosystem. | Preserves inspected needs, intentionally rejects general Datalog and unsupported discharge patterns. |
| Security/change risk | Established upstream protocol and existing local tests, plus ongoing local patch liability. Existing integration hazards still need work. | Less evaluator machinery, but a new chain, codec, API and four-repo cutover to review and maintain. |
| Owner roots/revocation | Can implement #836/#872 without replacing the token format. Biscuit itself does not prevent owner-root admission or signed settings views. | Explicit adapter boundary improves clarity, but does not solve offline freshness or hosted authority automatically. |
| Long-term maintenance | Track fork/upstream releases, public/private API friction, dual-language encoder contract. | Own every format, semantics, test corpus, provider adaptation and security response indefinitely. |

Current dependency cost is visible in Biscuit's manifest: parser/quote, protobuf, multiple signature families, RNG, regex and date support (`U/Cargo.toml:92–178`). Not all are exercised by our emitters. However, a fair comparison also preserves the advantages of the already implemented TS authority signer (`T/.../biscuit.ts:108–123`) and current shared verifier hardening. Those are real sunk work, not reasons to call the current implementation unusable.

A reasonable fork-only experiment is **a few days to one week** for the feature/build repair and measured dependency baseline; **2–5 engineer-weeks** for a maintained external-signing seam, deterministic policy restrictions and expanded cross-language vectors, depending on how much upstream API surgery is needed. These are estimates, not performed experiments. Upgrading Biscuit's crypto dependencies may be substantially harder than changing version numbers, and does not remove iroh's separate duplicate AES stack identified by [weft#1421](https://github.com/HeddleCo/weft/issues/1421).

Choose Apex if the lasting product requirement is a small auditable multi-language capability kernel with native external signing and a deliberately limited policy language, and fund the review/integration work. Choose the pinned fork if the immediate objective is restoring builds and reducing maintenance risk this quarter. Do not justify a 20–34-week security project solely with the unreleased feature-gating fix or claim that switching formats is necessary for owner anchoring.

## 13. Uncertainties that must remain visible

1. **Working tree versus shipped system.** This report inspects the versions listed in §1.1. It does not establish production reachability, released browser behavior or a coherent current source workspace for all published crates. In particular, presence/artifact product issuance and current hosted-client consumers need confirmation.
2. **Transport/profile decisions.** Exact method-to-action mapping, namespace/anchor behavior at owner boundaries, independent-root ceremonies, multi-target canonical request bytes and historical operation evidence need a reviewed API/profile registry. This spec gives the kernel and required contract, not a generated registry of every RPC.
3. **Signed settings implementation.** Existing owner-governance storage and #879's closure do not prove complete #872 consumption, fork reconciliation, authenticated compaction or local durable pins. Audit those concrete paths before committing the revocation estimate.
4. **Crypto protocol review.** The strict Ed25519 policy, domain/signature transcript, parent retention behavior, terminal proof and full-hop holder binding are proposed, not audited. External review can require changing the wire before v1 freeze.
5. **Performance/dependency claims.** No Apex code, lockfile, browser bundle, wasm binary, conformance implementation, benchmark or build reproduction exists in this deliverable. The allow-list and ceilings are targets to verify, not results.
6. **Offline loss and compromise.** No globally fresh revocation promise follows from signed checkpoints. Seven-day token TTL does not constrain a still-admitted compromised mint signer to seven days. The hosted canonical-copy argument does not automatically extend to pure peer-to-peer or unsynced data.

Research validation for this document consisted of read-only issue/comment retrieval, file and manifest inspection, source/version comparison, predicate/call-site searches, and review of the cited primary format/crypto documentation. No repository code was changed and no build/test command that could create outputs was run. The only created file is this `SPEC.md`.
