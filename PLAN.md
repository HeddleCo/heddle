# treadle#14 — HARVEST + WIRE plan

## Scope and command surface

Land the local/dogfood executor in heddle with no weft dependency:

```text
heddle ci run --local [--state <STATE>] [--config <PATH>] [--check <NAME>]...
```

- `--local` is the explicit execution-mode selector required by treadle#14. No
  hosted or fallback mode is added in this change.
- Without `--state`, checks run in the selected Heddle checkout and are bound to
  a Heddle tree built from that working tree.
- With `--state`, Heddle resolves the state, visibility-gates its checkout into
  an isolated temporary directory, and runs the same engine there.
- The default definition is the repository metadata file
  `<shared .heddle>/treadle.definition.bin` (canonical `TreadleDefinition`
  protobuf, the TypeScript SDK compile output). If
  `<shared .heddle>/treadle.lock.json` is present it must carry the matching
  hex BLAKE3 `definition_digest`. `--config` overrides the bin path; the
  lockfile is still `treadle.lock.json` next to that bin. This runner does
  not compile definitions and does not shell to node. This deliberately uses
  `Repository::heddle_dir()` rather than assuming `.heddle` is a directory in
  every checkout (isolated Heddle checkouts use a `.heddle` pointer file).
- `--check` selects by check name, not job name. It is repeatable and
  preserves definition order. Unlisted checks are omitted (named on stderr).
  An unknown name is a hard usage error. Execution is sequential: a required
  failure still runs later checks. This is not a `needs` DAG.
- Human output is the treadle check summary table. Heddle's global
  `--output json` emits the complete signed-verdict array.
- Exit nonzero when any authored `required` check ends in `failure`,
  `timed_out`, or `infra_error`. Advisory/informational failures remain visible
  but do not make the command fail.

## HARVEST

Lift the established treadle model rather than create another CI scheme:

1. `crates/config` becomes `crates/ci-config`:
   - schema-v1 `[meta]` + ordered `[[check]]` model;
   - argv-only commands, class, timeout, env, services, cache paths, retry,
     triggers, supersede, and isolation fields;
   - duplicate/empty-command/regex/cron validation and unknown-key warnings;
   - definition digest computed with Heddle's canonical typed blob hash.
2. `crates/engine` becomes `crates/ci-engine`:
   - `exec.rs`: sequential execution, deterministic argv, timeout, bounded
     combined capture, retries, and verdict-body assembly;
   - `proc_group.rs`: Unix process-group creation/kill so timeout reaps the
     entire `sh -> cargo -> rustc` tree;
   - `ansi.rs`: CSI/OSC/control stripping;
   - `classify.rs`: disposition-first Build/Test/Lint/Infra/Timeout classifier
     and 4 KiB UTF-8-safe tail excerpt;
   - `cache.rs`, `env.rs`, `service.rs`: explicit cache exports, hermetic child
     environment, and out-of-process service-provider boundary.
3. `crates/check-cli` becomes the `heddle ci run --local` orchestration:
   definition resolution, check filtering, engine invocation, signing, table
   rendering, JSON rendering, advisory warnings, and exit decision.
4. The treadle runner is not copied into heddle. Its useful local-mode
   invariants are wired directly: state-addressed materialization, one shared
   engine for local/runner parity, and signing only after execution facts exist.

## WIRE into heddle

1. Use `heddle-crypto`'s merged verdict-v2 types and
   `signed_verdict_from_signer`; do not migrate treadle's parallel schema or
   signing implementation.
2. Require the linked Heddle device identity. Do not accept an ephemeral key,
   arbitrary key file, or the per-repository auto-minted capture key. Stamp
   `SignerKind::Device`, making every local verdict advisory-only by v2 policy,
   and verify every envelope before emitting it.
3. Bind a working-tree run to real Heddle identities:
   - build the checkout tree using `Repository::build_tree`;
   - derive a non-persisted projection of the current state with that exact tree
     and its existing rewrite-stable `ChangeId`;
   - bind the body and v2 signature to the projected state id, `ChangeId`, and
     exact tree digest;
   - rebuild after the checks and refuse to sign if tracked/evaluated bytes
     changed during execution.
4. Bind `--state` to the resolved stored state id, its `ChangeId`, and stored
   tree digest. Run it from a visibility-gated temporary checkout and clean the
   temporary materialization record afterward.
5. Keep caches outside the evaluated tree under `.heddle/cache/ci`; a cache is
   an accelerator, never evidence. Local mode uses treadle's `NoopProvider`, so
   a definition requesting services produces an honest `infra_error` verdict.
   Docker/service execution remains available at the engine boundary but is not
   silently enabled by the local CLI.
6. Register `ci` / `ci run` in the CLI argument surface, dispatch, command
   contract, and contract tests. Use the existing global output-mode contract.

## Classifier decision from the required re-read

`FailureClass::Bench` and `FailureClass::MergeConflict` exist in verdict-v2, but
treadle's process-output classifier never emits them. `default_features` also
appears only as an example/fixture subclass; the classifier does not derive it.
Keep that exact behavior. A plain process exit has no sound basis for inferring
benchmark regression, speculative-merge conflict, or the semantic reason a
build failed, so this harvest will not invent those classifications.

## Verification

1. Focused config, engine, and CLI executor tests, including:
   parsing/filtering; pass/fail/advisory exit behavior; ANSI stripping;
   classification; excerpt cap; timeout; process-group kill on Unix; real v2
   device signature verification; working-tree identity binding; explicit-state
   materialization; and mutation refusal.
2. `cargo +nightly fmt` on touched Rust files.
3. `cargo build`.
4. `cargo clippy --all-targets -- -D warnings` for the new crates and CLI.
5. Focused executor tests (and broader tests as time permits).

## Explicitly deferred

- Weft scheduling, leasing, upload, supersession, trust-set evaluation, and
  authoritative gating.
- Container/microVM isolation and automatic Docker service activation.
- Merge-basis materialization and `FailureClass::MergeConflict` production.
- Canonical protected-policy CheckSet compilation / `check_set_digest` and node
  ids.
- Log/artifact upload and operational attempt sidecars.
- Bench/default-features failure subclasses; treadle has schema vocabulary but
  no sound classifier implementation to harvest.

These deferrals do not add fallbacks or compatibility shims. The delivered path
is one local executor, one Heddle verdict-v2 signing path, and one explicit trust
level (`device` / advisory-only).
