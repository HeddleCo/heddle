# Verification And Git Projection Cleanup Plan

Status: cleanup plan from the July 2026 branch review and follow-up design interview.

This plan removes duplicate verification ownership, retires obsolete Bridge Mirror workflows, and keeps the codebase aligned with the current Git Overlay model. It is not a line-count exercise. The goal is one deep Repository Verification State interface, high locality for Git/Heddle proof logic, and no legacy paths future work has to reason around.

Related ADRs:

- `docs/adr/0042-retire-persistent-bridge-mirror.md`
- `docs/adr/0043-workflow-command-vocabulary.md`

## Vocabulary

Use the glossary terms in `CONTEXT.md` exactly.

- Git Overlay: Heddle sidecar over an existing Git checkout. Active Git reads and writes use the checkout's real `.git`.
- Bridge Mirror: the legacy bare Git repository at `.heddle/git`, used by explicit bridge import/export/sync, reconstruction, fsck, and maintenance paths. It is not active Git Overlay state.
- Raw Git Object Residual: verbatim Git object bytes preserved when Heddle cannot reconstruct an object byte-for-byte from native state.
- Git Projection Mapping: durable repository metadata mapping Heddle states to Git object ids, refs, and projection metadata.
- Repository Verification State: the proof surface for repository mode, Git/Heddle agreement, worktree dirt, remote drift, active operations, workflow guidance, and Machine-Contract Proof.
- Machine-Contract Proof: proof that command catalog metadata, JSON envelopes, schema introspection, documentation drift checks, and op-id support agree.

## Resolved Decisions

### Verification Ownership

- Core owns Repository Verification State construction.
- Core does not own the CLI command catalog. Machine-Contract Proof is supplied as an input snapshot by the embedding surface.
- The CLI supplies Machine-Contract Proof from the command catalog. Non-CLI embedders may supply an equivalent snapshot or mark it `not_applicable` / `not_checked`.
- Hardcoded machine-contract counters in core are temporary drift and should be deleted.
- A renamed CLI adapter module such as `verification_cli` or `verification_advice` may remain, but it must not build Repository Verification State. Its job is rendering, `RecoveryAdvice`, command-catalog action templates, command-specific wording, and temporary migration shims.

### Plain Git

- `PlainGitImportHint` is deleted as a null-only sidecar; public `status` and `doctor` JSON no longer emits legacy `git_overlay_import_hint` sidecars. Internal callers may still compute import-hint data for text/advice until the core verification migration absorbs it.
- Useful observe-only plain-Git context belongs in a shared core `PlainGitProbe`: root, active branch, dirty/index summary, commit/ref shape, and recommended setup action.
- Existing committed plain-Git repositories recommend `heddle adopt --ref <branch>` or `heddle adopt`; unborn Git recommends `heddle init`.

### Public JSON And Actions

- Heddle is alpha, so cleanup may break JSON field names when it produces the better model.
- Public JSON should use `verification` as the canonical Repository Verification State field.
- `git_overlay_health` has been removed from public `status` and `doctor` JSON; remaining internal uses should migrate behind Repository Verification State rather than re-exporting compatibility aliases.
- Avoid `trust` in public JSON because it is overloaded.
- Code, schemas, docs, and tests should move together by command-family slice.
- Action selection takes an explicit audience input, such as `ActionAudience::Human`, `Agent`, or `Script`; do not infer action semantics from TTY/JSON mode alone.
- Structured action templates are canonical. `recommended_action` strings are display compatibility only.
- Agents may fill placeholders only when action metadata says `agent_may_fill: true`.
- Mutating commands fail closed when repository verification is degraded or blocked. Observe-only commands may render degraded proof.

### Module Shape

- The target core module name is `verification`. The CLI command/report verb can remain `verify`.
- Rename `GitOverlayHealth`-centered internals toward Repository Verification State and narrower verification-check names.
- Keep action precedence centralized. `status/next_action.rs` can remain during migration, but the target home is a general verification action-selection module.
- Treat the 300-line guideline as a review smell, not a hard gate. Split modules by cohesive functionality and interface depth, not arbitrary line count.

### Workflow Vocabulary

- `land` is the long-term managed-thread landing verb. Do not keep `ship` as a long-term alias.
- `commit` is the ordinary human save path and should compose shared lower-level save/projection modules directly.
- `capture` remains public as an advanced granular savepoint for agents and advanced users. It is not legacy.
- `checkpoint` remains public as a Git-facing milestone primitive for agents and advanced workflows. It is not legacy.
- For `needs_checkpoint`, preserve the precise proof state and keep `checkpoint` as an executable machine/agent action. Human-facing general guidance should bias toward `commit` unless the command context is explicitly Git/checkpoint oriented.
- `ready` may auto-capture, but it must route through the shared save primitive and receive next actions from structured verification action selection.
- `thread refresh` and `thread resolve` should become advanced-only or retire from ordinary breadcrumbs. Normal guidance should prefer top-level `sync`, `resolve`, `continue`, `abort`, `ready`, and `land`.
- `docs/spikes/save-verb-consolidation.md` remains historical context, not the active plan as written.

### Bridge Mirror And Git Projection

- The persistent `.heddle/git` Bridge Mirror is not the desired end state.
- Normal Git Overlay flows should never create `.heddle/git`.
- Public `bridge git` commands are retired in favor of `adopt`, `import git`, `export git`, and top-level remote verbs routed by remote capability.
- `bridge git init` is removed; it exists to initialize the persistent mirror that the target model deletes.
- Legacy bridge status is removed from public UX; useful diagnostics move into `verify`, `fsck`, import/export dry-runs, or explicit diagnostics.
- `import git` becomes `import git`; `export git` becomes `export git`. `adopt` remains the friendly existing-checkout onboarding path.
- `bridge git push` / `pull` are removed in favor of top-level `push` / `pull`; `bridge git sync` is removed in favor of `sync git` for explicit bidirectional Git projection.
- `export git` must require an explicit destination or named remote target. It must not create hidden repo-local Git state.
- `import git` and `export git` expose their current JSON contracts without restoring bridge-git UX.
- Lossy import must stay explicit with strong naming, such as `--allow-lossy`. If byte-identical Git export is expected, import captures residuals rather than silently degrading fidelity.

### Remotes

- Remote routing is capability-based, not symmetric by brand.
- Heddle remotes may support Heddle state, collaboration data, and Git object/projection data.
- Git remotes support only Git projection data.
- Top-level `push`, `pull`, and `sync` route by remote capability; users should not choose bridge subcommands to select Git interoperability.
- Pushing to a Git remote exports Git refs directly from Heddle state plus Raw Git Object Residuals. It must not require a local `.heddle/git` staging repo.
- Pulling from a Git remote imports into Heddle state plus residuals with the same fidelity guarantees as `import git`, then writes through to the checkout's real `.git` only after verification succeeds.
- Do not add a normal Git-only pull mode; that would advance `.git` while Heddle mapping lags.
- `sync` may do bidirectional Git remote reconcile where safe, but it must be proof-driven and fail closed on divergence or fidelity blockers.

### Raw Git Object Residuals

- Residuals are Heddle-native durable repository metadata, not immutable source history data, and should not affect source state hashes.
- Residuals are content-addressed by Git object id plus object format and can store any Git object type.
- Store canonical Git object content plus type and object format. Loose-file compression or pack layout is not durable truth.
- Git Projection Mapping records whether each mapped oid is reconstructable, residual-backed, or unavailable.
- A mapped Git oid that cannot be reconstructed and lacks residual bytes is a hard verification/fsck failure.
- Residuals sync/export by default for any served/imported history that depends on them.
- Residual garbage collection is reachability-based from current mappings, served refs, and retained imported history.
- Rewrites must preserve residuals while any retained mapping needs them.
- New Heddle-authored Git projections should not create residuals by default.
- Existing `.heddle/git` mirrors migrate lazily: verification, fsck, import, or export can copy needed residuals out of the old mirror and then report that the mirror is removable.
- Deletion of migrated mirrors should happen through explicit maintenance cleanup during the replacement phase.
- Hosted Heddle remotes that advertise Git projection support must store/sync Git Projection Mapping and Raw Git Object Residuals so clones preserve Git byte fidelity.

## Implementation Tracks

### Track A: Verification Cleanup

1. Establish the core verification shape: `MachineContractInput`, `ActionAudience`, structured actions, richer `PlainGitProbe`, `verification` naming, and command-catalog proof injection.
2. Shipped for `status`, `verify`, and `doctor`: public JSON uses `verification`; legacy `git_overlay_health` / `git_overlay_import_hint` sidecars are internal render/advice data rather than JSON contract fields.
3. Migrate remaining CLI callers from CLI-owned proof builders to the core proof interface: `diagnose`, `ready`, `thread`, remote commands, merge/rebase/operator preflights, and post-operation envelopes.
4. Split and rename modules by cohesive responsibility after ownership is stable.
5. Delete old CLI proof builders by migrated slice. Temporary equivalence tests may exist during migration, then should be removed or inverted into reachability tests proving no CLI-owned proof builder remains reachable.

### Track B: Bridge Mirror Retirement

1. Add Raw Git Object Residual durable objects and fsck checks without deleting `.heddle/git` yet.
2. Add replacement public command shells: `import git` and `export git` route to current internals first and establish parity. Shipped as top-level wrappers, legacy `bridge git import/export` is now removed.
3. Route top-level Git remote `push` / `pull` / `sync` by remote capability.
4. Teach checkout write-through, export, push, sync, clone, and reconstruction paths to compose reconstructed Heddle state plus residuals without requiring a persistent bare mirror.
5. Replace mirror fsck with Git Projection Mapping and Raw Git Object Residual validation. First slice shipped: `fsck --repair git` performs explicit metadata-only Git Projection Mapping repair and reruns Git projection checks; residual validation remains follow-on work.
6. Public `bridge git` commands are retired after replacement import/export and fsck diagnostics.
7. Lazy-migrate old `.heddle/git` mirrors into residual storage and leave deletion to explicit maintenance cleanup.

### Track C: Workflow Verb Cleanup

1. Replace `ship` with `land` in docs, tests, command routing, and breadcrumbs.
2. Keep `capture` and `checkpoint` as advanced public primitives.
3. Route `commit`, `capture`, `checkpoint`, and `ready` auto-capture through shared lower-level save/projection modules.
4. Move ordinary breadcrumbs away from `thread refresh` / `thread resolve` and toward top-level workflow verbs.
5. Assert structured actions by audience rather than exact `recommended_action` strings wherever possible.

## Git Repair Surface

- The Git repair surface is `fsck --repair git`. The current implementation is metadata-only and does not import history, reconcile refs, or write the worktree.
- `verify` proves and recommends; it must not mutate Git Projection Mapping or Raw Git Object Residuals unless an explicit repair mode is requested.
- `fsck` owns integrity checks and repair flows for Git Projection Mapping, Raw Git Object Residuals, and migrated Bridge Mirror state.
- The Git repair mode may synthesize missing Git Projection Mapping only when a Heddle state match or Git note/provenance link proves the mapping unambiguously. It must not guess.
- The Git repair mode does not import missing Git commits by default; history expansion belongs to `adopt`, `import git`, or `pull`.
- The Git repair mode may migrate needed residual bytes from an old `.heddle/git` Bridge Mirror into Raw Git Object Residual storage.
- The Git repair mode reports when an old mirror is removable but does not delete it; deletion belongs to maintenance cleanup or an explicit cleanup flag.
- Metadata-only Git repair is allowed in dirty worktrees. Any repair that would write real `.git` refs, index, or worktree state requires clean verification or explicit confirmation.
- Normal user output should not expose Git Projection Mapping internals. Verbose, fsck, import/export dry-run, and diagnostics can.
- `log` / `show --verbose` should keep showing the Git Checkpoint commit ID as the user-facing Git handle.

## Issue Breakdown

1. Verification core shape
   - Core accepts `MachineContractInput`.
   - Core accepts explicit `ActionAudience`.
   - Structured actions are canonical.
   - Core exposes richer `PlainGitProbe`.
   - No broad command-family migration is required in this issue.
2. Status/verify public JSON migration
   - `status` and `verify` output `verification`.
   - `git_overlay_health` is removed from those JSON outputs.
   - Schema, docs, and tests update in the same slice.
   - CLI runs supply command-catalog Machine-Contract Proof.
   - Non-CLI embedders can mark Machine-Contract Proof not applicable.
3. Replace public bridge import/export commands (done)
   - `import git` and `export git` exist and route to current internals.
   - `export git` requires an explicit destination or remote target.
   - Dry-run JSON exists for both.
   - README and command docs no longer teach `bridge git`.
4. Raw Git Object Residuals
   - Residual durable object model exists.
   - Fsck verifies mapped non-reconstructable objects have residuals.
   - Export/write-through can use residuals instead of the mirror for lossy objects.
   - Old `.heddle/git` mirrors can lazily migrate needed residuals.
5. Retire public bridge/mirror workflow (done for public bridge-git)
   - The public bridge-git workflow is removed; diagnostics, repair, and ingest behavior live on replacement surfaces.
   - Top-level `push`/`pull`/`sync` route Git remotes.
   - Normal flows no longer create persistent `.heddle/git`.
   - Maintenance cleanup can remove migrated mirrors.
6. Workflow verb cleanup
   - `land` replaces `ship`.
   - `capture` and `checkpoint` are documented as advanced primitives, not legacy.
   - Ordinary breadcrumbs avoid `thread refresh` / `thread resolve`.
   - Tests assert structured actions by audience.

## Tests

Keep these tests until their replacement track has landed:

- Plain-Git observe/adopt/init loop tests; they protect the `init` versus `adopt` split.
- The regression proving active Git Overlay no longer creates an eager `.heddle/git` mirror.
- Bridge Mirror behavior tests until Raw Git Object Residuals and replacement import/export/remote surfaces cover the same behavior.
- Machine-contract integration tests; they become proof that core receives command-catalog coverage rather than hardcoded counters.
- Checkpoint guidance tests; `checkpoint` is an advanced primitive, not legacy.

Move or invert these tests during migration:

- Move CLI `git_overlay_health` unit tests to core `verification` or action-selection tests.
- Rewrite JSON tests from `git_overlay_health` assertions to `verification` assertions in the same slice that changes public JSON.
- Replace most exact `recommended_action` string tests with structured action assertions: action kind, argv template, required inputs, `agent_may_fill`, and audience.
- Invert temporary equivalence tests after migration: instead of proving CLI and core agree, prove no CLI-owned verification builder remains reachable.
- After a command family consumes shared verification proof, reduce redundant local preflight tests. Keep one integration test per command family proving it consumes shared proof and fails closed.

## Immediate Cleanup Candidates

- Rename stale wording that calls `.heddle/git` the active or canonical Git store to Bridge Mirror language.
- Do not delete `.heddle/git` bridge paths wholesale until Raw Git Object Residual storage, replacement commands, and fsck coverage exist.
