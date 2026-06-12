# Heddle JSON output schemas

This document is the canonical reference for every CLI verb that emits
machine-readable output. Every entry below pairs a literal sample
output with a field-by-field table — the contract callers can rely
on.

## Runtime introspection

Schemas in this document are mirrored at runtime by
`crates/cli/src/cli/commands/schemas.rs`. Generate the canonical JSON
Schema for any verb with:

    heddle schemas                    # list registered schema verbs
    heddle schemas <verb>             # e.g. heddle schemas status
    heddle schemas log --reflog       # subcommands taking --flags work too
    heddle schemas merge --preview --output text
    heddle schemas bridge git status

(Indented as plain text rather than a fenced block so the
`heddle doctor docs` flag-checker doesn't flag `--reflog` as
unknown — the schemas verb takes its argument as
`trailing_var_arg`. Schema output is always JSON; trailing global
flags such as `--output text` are ignored for lookup.)

CI runs `heddle doctor schemas` on every PR and validates each literal
JSON sample below against the registered schema. Drift — a sample
field the schema doesn't declare, or vice versa — exits non-zero so
this doc cannot silently fall behind the implementation. Pair with
`heddle doctor docs` (which covers flag-level drift) for full doc
coverage.

## Discipline

Every `--output json` output in Heddle's CLI follows the same rules. These
rules are load-bearing — agents and tooling reason over the wire shape
and assume the discipline holds.

1. **Stable, well-named fields.** Identifiers for states use
   `change_id` (the underlying type is `objects::object::ChangeId`).
   Timestamps for state creation use `created_at`. Confidence values
   use `confidence`. The same concept always uses the same field name
   across commands.
2. **Optional fields are explicit `null`, not omitted.** A semantically
   permanent field that happens to be unset for the current request is
   still serialized — `"current_state": null` rather than dropping the
   key. The exception is genuinely conditional fields whose presence
   itself carries meaning (e.g. `git_commit_preview`, only present in
   `--preview` mode); those are documented as conditional.
3. **No leakage of unrelated context.** Bridge import-hint information
   lives only in `heddle bridge git status --output json` (and the
   comprehensive `heddle doctor --output json`). Per-command outputs do not
   carry it. Transports do not silently piggy-back state.
4. **Empty collections serialize as `[]` / `{}`, not omitted.** An
   empty `blockers: []` is more useful than a missing field, and the
   discipline prevents tooling from writing brittle "key exists?"
   guards.
5. **Pretty printing is reserved for `heddle show`.** Every other verb
   emits compact, single-line JSON suitable for line-oriented streaming
   (one document per line for `heddle watch`, etc.).
6. **Action fields (`next_action`, `recommended_action`) follow one
   presence contract.** `null` means "this output carries the field and
   no action is needed right now"; an *absent* field means "not
   applicable to this output shape"; the empty string is **never**
   emitted. Agents can branch on `action == null` for "nothing to do"
   without an emptiness guard. Enforced at the serialization boundary
   (`validate_next_actions_at_path` rejects `""`) and by the
   `action_fields_follow_presence_contract_in_every_schema` conformance
   test over the schema registry; emitters normalize empty selections
   through `next_action::normalized_action` /
   `serialize_empty_action_as_null` (HeddleCo/heddle#645).

The schemas below are hand-curated rather than auto-generated. We
chose this over `schemars`-based introspection because the surface is
modest, and a curated doc lets us pin the user-facing contract to the
field-naming rules above without coupling to internal struct shapes
that the compiler is happy to reorder.

## Stability commitments

Heddle is pre-OSS. The shapes below may break between releases, but
each break will be documented in the release notes. The discipline
itself (rules 1–5) is stable: no future shape will silently regress
empty-collection omission or move into per-command import-hint
leakage.

## State-ID acceptance

Every CLI verb that takes a state argument accepts the same set of
specifiers. Pass any of them — they all resolve to the same change ID:

* **Full change ID** — the 32-character form printed by `show --output json`'s
  `change_id_full`, e.g. `hd-sqr398dvx9ayt9bf8bf5gz0jg8`.
* **Short change ID** — the 12-character prefix printed by every other
  `--output json` verb's `change_id` field, e.g. `hd-sqr398dvx9ay`. Any
  unambiguous prefix of length 4 or more works; ambiguous prefixes
  yield an `ambiguous state ID prefix '<X>' matches: <list>` error.
* **Marker name** — anything created by `heddle marker create <name>`,
  e.g. `failed-build-2026-05-09`.
* **`HEAD`, `@`, `HEAD~N`, `@~N`** — relative walks from the active
  thread's tip.
* **Thread name** — resolves to that thread's tip.

Verbs covered: `show`, `diff`, `revert`, `cherry-pick`,
`goto`, `blame --state`, `log --since`, `review show`,
`review sign`, `discuss open|list|resolve --state`, `retro --since`.
The `heddle log --output json` `change_id` field is the canonical short form
that downstream verbs consume.

---

## `heddle init --output json`

Initialize Heddle metadata. In a plain Git repository this creates the
`.heddle` sidecar and updates the local `.git/info/exclude` file for Heddle
metadata only; it does not import Git history or write Git-tracked files.

### Sample

```json
{
  "output_kind": "init",
  "status": "initialized",
  "action": "init",
  "path": "/repo/.heddle",
  "repository_mode": "git-overlay",
  "git_detected": true,
  "heddle_initialized": true,
  "installed_heddleignore": false,
  "principal_configured": false,
  "side_effects": [
    "created Heddle sidecar for the existing Git repository",
    "updated .git/info/exclude for Heddle metadata",
    "left Git-tracked files untouched"
  ],
  "message": "Initialized Heddle data in /repo/.heddle for Git-overlay workflows",
  "next_action": "heddle adopt --ref main",
  "recommended_action": "heddle adopt --ref main"
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `status`, `action` | string | required | Always `initialized` / `init` on success. |
| `path` | string | required | Path to the initialized `.heddle` metadata directory. |
| `repository_mode` | string | required | Repository capability after init, e.g. `git-overlay` or native Heddle storage. |
| `git_detected`, `heddle_initialized` | bool | required | Whether init detected an existing Git repo, and whether Heddle metadata is now present. |
| `installed_heddleignore`, `principal_configured` | bool | required | Side effects outside `.heddle`, if any. `installed_heddleignore` is currently false; init does not install ignore-policy files. Git-overlay init uses local Git excludes only for Heddle metadata. |
| `side_effects` | array<string> | required | Human-readable, machine-preserved list of what init changed or intentionally left untouched. |
| `message` | string | required | Human summary. |
| `next_action`, `recommended_action` | string \| null | required | Primary verification-guided next command. In a Git repo this is the explicit `heddle adopt --ref <branch>` command. |

Note: the `verification` block is intentionally omitted from mutation
replies. Run `heddle verify --output json` (or `heddle status --output
json`) for the canonical verification surface.

---

## `heddle status --output json`

Snapshot of the repository's current thread, worktree state, and any
in-progress operation.

### Sample

```json
{
  "output_kind": "status",
  "repository_capability": "git-overlay",
  "repository_label": "Git + Heddle",
  "storage_model": "git+heddle-sidecar",
  "hosted_enabled": false,
  "operation": null,
  "remote_tracking": null,
  "git_overlay_health": {
    "status": "clean",
    "clean": true,
    "summary": "Git overlay and Heddle agree",
    "recovery_commands": [],
    "checks": []
  },
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "git-overlay",
    "heddle_initialized": true,
    "git_branch": "main",
    "heddle_thread": "feature/parser-fast",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "machine_contract_coverage": {
      "status": "available",
      "verified_scope": "everyday_and_agent",
      "advanced_scope": "advanced_internal_admin",
      "summary": "203 command(s), 174 JSON command(s), 104 mutating command(s), 103 mutating JSON command(s); verified everyday/agent machine surface has 38 concrete schema-backed JSON command(s); advanced/internal/admin surfaces carry 56 accepted opaque schema(s) outside clean verification",
      "catalog_commands_total": 203,
      "catalog_mutating_commands_total": 104,
      "json_commands_total": 174,
      "json_mutating_commands_total": 103,
      "json_commands_with_schema": 118,
      "json_commands_with_accepted_opaque_schema": 56,
      "json_commands_without_schema": 0,
      "verified_scope_json_commands_total": 38,
      "verified_scope_json_commands_with_schema": 38,
      "verified_scope_json_commands_with_accepted_opaque_schema": 0,
      "verified_scope_json_commands_without_schema": 0,
      "advanced_scope_json_commands_total": 136,
      "advanced_scope_json_commands_with_accepted_opaque_schema": 56,
      "mutating_commands_total": 103,
      "mutating_commands_with_schema": 74,
      "mutating_commands_with_accepted_opaque_schema": 29,
      "mutating_commands_without_schema": 0,
      "verified_scope_mutating_commands_total": 23,
      "verified_scope_mutating_commands_with_schema": 23,
      "verified_scope_mutating_commands_with_accepted_opaque_schema": 0,
      "verified_scope_mutating_commands_without_schema": 0,
      "advanced_scope_mutating_commands_total": 80,
      "advanced_scope_mutating_commands_with_accepted_opaque_schema": 29,
      "schema_verbs_total": 177,
      "documented_schema_verbs_total": 177,
      "undocumented_schema_verbs_total": 0,
      "opaque_schema_verbs_total": 56,
      "accepted_opaque_schema_verbs_total": 56,
      "unaccepted_opaque_schema_verbs_total": 0,
      "supports_op_id_total": 99,
      "jsonl_commands_total": 5,
      "missing_schema_examples": [],
      "missing_mutating_schema_examples": [],
      "verified_scope_missing_schema_examples": [],
      "verified_scope_accepted_opaque_schema_examples": [],
      "advanced_scope_accepted_opaque_schema_examples": [
        "transaction begin",
        "transaction abort",
        "transaction status",
        "conflict list",
        "conflict show",
        "redact apply",
        "redact list",
        "redact show"
      ],
      "accepted_opaque_schema_examples": [
        "transaction begin",
        "transaction abort",
        "transaction status",
        "conflict list",
        "conflict show",
        "redact apply",
        "redact list",
        "redact show"
      ],
      "unaccepted_opaque_schema_examples": [],
      "undocumented_schema_examples": []
    },
    "summary": "Git overlay and Heddle agree",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  },
  "thread": "feature/parser-fast",
  "base_state": "hd-abc123",
  "base_root": "hd-abc123",
  "current_state": "hd-def456",
  "path": "/repo",
  "execution_path": "/repo",
  "actor": {"provider": "anthropic", "model": "claude-opus-4-7"},
  "harness": "claude-code",
  "thinking_level": null,
  "usage_summary": null,
  "last_progress_at": null,
  "report_flush_state": null,
  "attach_reason": null,
  "thread_mode": "lightweight",
  "thread_state": "active",
  "freshness": "current",
  "target_thread": null,
  "parent_thread": null,
  "child_threads": [],
  "task": null,
  "promotion_suggested": false,
  "impact_categories": [],
  "heavy_impact_paths": [],
  "changed_path_count": 0,
  "blockers": [],
  "recommended_action": "",
  "recovery_commands": [],
  "thread_health": "clean",
  "coordination_status": "clean",
  "is_isolated": false,
  "parallel_threads": [],
  "state": {"change_id": "hd-def456", "content_hash": "deadbeef", "intent": null},
  "git_checkpoint": null,
  "changes": {"modified": [], "added": [], "deleted": []}
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `repository_capability` | string | required | Core machine capability, e.g. `"git-overlay"`, `"native-heddle"`, or `"plain-git"`. |
| `repository_label` | string | required | Human-facing repository identity. In managed Git-overlay child checkouts this is `"Git + Heddle isolated checkout"` even though core capability remains `native-heddle`. |
| `repository_context` | object | optional | Present for managed child checkouts; includes `kind`, `parent_repository`, and any recorded `target_thread` / `parent_thread`. |
| `storage_model` | string | required | E.g. `"git+heddle-sidecar"`. |
| `hosted_enabled` | bool | required | Whether the repo is connected to a hosted server. |
| `operation` | object \| null | required | In-progress operation (`merge`, `rebase`, …) or `null`. |
| `remote_tracking` | object \| null | required | Remote drift summary or `null`. |
| `git_overlay_health` | object | required | Compatibility health view derived from the shared verification checks. |
| `verification` | object | required | Full `RepositoryVerificationState`; status next actions defer to this when verification is blocked. |
| `thread` | string \| null | required | Current thread name; `null` for detached HEAD. |
| `base_state`, `base_root` | string \| null | required | Thread base anchor change-ids. |
| `current_state` | string \| null | required | Thread tip change-id. |
| `path` | string \| null | required | Materialized worktree path. |
| `execution_path` | string \| null | required | Effective execution root. |
| `actor` | object \| null | required | `{provider, model}`. `null` when no agent is attached. |
| `thread_mode` | enum \| null | required | `lightweight` / `materialized` / `virtualized`. |
| `thread_state` | enum \| null | required | Thread lifecycle: `active` / `ready` / `blocked` / `merged` / `abandoned` / `promoted`. Same values and meaning as `thread list`; repository-health/verification blockers surface via `coordination_status`, not here. |
| `freshness` | enum \| null | required | `current` / `stale` / `unknown`. |
| `child_threads` | array<string> | required | Names; empty array if none. |
| `impact_categories` | array<enum> | required | Empty array if none. |
| `heavy_impact_paths` | array<string> | required | Empty array if none. |
| `blockers` | array<string> | required | Human-readable blockers; empty array if clean. |
| `recommended_action` | string | required | Primary next command; verification blockers take precedence. |
| `recovery_commands` | array<string> | required | Recovery commands from `verification`; empty when verified. |
| `coordination_status` | enum | required | `clean` / `ahead` / `diverged` / `blocked` / `merge-ready`. |
| `parallel_threads` | array<object> | required | Empty array if none. |
| `state` | object \| null | required | Current state summary. |
| `git_checkpoint` | object \| null | required | Latest git checkpoint, when configured. |
| `changes` | object | required | Worktree status: `{modified: [], added: [], deleted: []}`. |

**Note:** Bridge import-hint information is not part of this output.
Use `heddle bridge git status --output json`.

---

## `heddle verify --output json`

Concise proof that Git, Heddle, mapping, worktree, remotes, operations, clone checks, and machine contracts agree.
`verify` is strict by default: clean verification writes this JSON object to
stdout and exits `0`; blocked verification writes no stdout, exits nonzero,
and emits one JSON error envelope on stderr with `kind: "verify_failed"` and a
nested `verification` object containing this same verification state shape. Use
`heddle status --output json` for observe-only automation that needs the
blocked verification state on stdout.

### Sample

```json
{
  "output_kind": "verify",
  "clean": true,
  "repository_label": "Git + Heddle",
  "verified": true,
  "status": "clean",
  "repository_mode": "git-overlay",
  "heddle_initialized": true,
  "git_branch": "main",
  "heddle_thread": "main",
  "worktree_dirty": false,
  "worktree_state": "clean",
  "import_state": "clean",
  "mapping_state": "clean",
  "remote_drift": "clean",
  "active_operation": null,
  "default_remote": null,
  "clone_verification": "not_applicable",
  "machine_contract": "available",
  "workflow_status": "idle",
  "workflow_summary": "No ready thread is waiting to merge",
  "summary": "Git overlay and Heddle agree",
  "checks": [
    {
      "name": "Git",
      "status": "clean",
      "clean": true,
      "summary": "Git worktree is clean",
      "recommended_action": null,
      "recovery_commands": [],
      "details": {}
    }
  ],
  "recommended_action": null,
  "recommended_action_template": null,
  "recovery_commands": [],
  "recovery_action_templates": []
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `output_kind` | string | required | Always `verify`; lets agents identify the proof payload without a wrapper object. |
| `repository_label` | string | required | Human-facing repository identity; managed Git-overlay child checkouts use `"Git + Heddle isolated checkout"`. |
| `repository_context` | object | optional | Present for managed child checkouts; includes `kind`, `parent_repository`, and any recorded `target_thread` / `parent_thread`. |
| `verified` | bool | required | `true` only when all verification checks are clean or not applicable. |
| `clean` | bool | required | Alias of `verified` for agents that sort command results into clean/blocked buckets. |
| `status` | string | required | Overall verification status, e.g. `clean`, `needs_import`, or `dirty_worktree`. |
| `repository_mode`, `heddle_initialized`, `git_branch`, `heddle_thread`, `worktree_dirty`, `worktree_state`, `import_state`, `mapping_state`, `remote_drift`, `active_operation`, `default_remote`, `clone_verification`, `machine_contract`, `machine_contract_coverage`, `workflow_status`, `workflow_summary` | mixed | required except nullable fields | The flattened `RepositoryVerificationState`; `heddle verify --output json` is the canonical verification state, not a wrapper around another `verification` object. |
| `summary` | string | required | Human-sized explanation of the top verification state. |
| `checks` | array<object> | required | Public checklist rows for Git, Heddle, Mapping, Worktree, Remote, Operation, Machine contract, and Clone. |
| `recommended_action` | string \| null | required | Display command for the primary next step. `null` when no action is needed. |
| `recommended_action_template` | object \| null | required | Fillable template for `recommended_action` — `argv_template` (executable argv, current Heddle executable path as argv[0]), `required_inputs`, `agent_may_fill`. Present for every valid action; `null` only when the display command is null. The canonical machine-readable action shape — the always-null `_argv` sidecar was dropped (HeddleCo/heddle#254). |
| `recovery_commands` | array<string> | required | Display commands for recovery, in priority order. Empty when verified. |
| `recovery_action_templates` | array<object> | required | Fillable templates mirroring `recovery_commands`. |
| `checks[].recommended_action_template`, `checks[].recovery_action_templates` | object/array/null | required | Structured fillable action metadata scoped to the check row. |

### Blocked JSON verify

When verification is blocked, stdout is empty. The stderr envelope carries the
standard recovery fields plus nested verification proof:

```text
{
  "error": "Repository is not verified: dirty_worktree",
  "exit_code": 1,
  "hint": "Run `heddle commit -m <message>` to clear the primary verification blocker.",
  "kind": "verify_failed",
  "unsafe_condition": "worktree has unsaved changes",
  "would_change": "`heddle verify` is a strict proof gate and returns nonzero until every verification check is clean",
  "preserved": "verify is observe-only; repository objects, refs, index, and worktree files were left unchanged",
  "primary_command": "heddle commit -m <message>",
  "primary_command_template": {
    "action": "heddle commit -m <message>",
    "argv_template": ["heddle", "commit", "-m", "<message>"],
    "required_inputs": ["message"],
    "agent_may_fill": true
  },
  "recovery_commands": ["heddle commit -m <message>", "heddle verify"],
  "recovery_action_templates": [
    {
      "action": "heddle commit -m <message>",
      "argv_template": ["heddle", "commit", "-m", "<message>"],
      "required_inputs": ["message"],
      "agent_may_fill": true
    }
  ],
  "verification": {
    "clean": false,
    "verified": false,
    "status": "dirty_worktree",
    "repository_mode": "git-overlay",
    "summary": "worktree has unsaved changes",
    "checks": []
  }
}
```

---

## Core loop mutation schemas

These verbs are the everyday loop agents use after discovery through
`heddle commands --output json`: capture state, save it as a
Git-compatible commit when needed, undo/redo the last logical
operation, and ask whether a thread is ready. The lower-level
`checkpoint` command is documented here as an explicit Git-adapter
surface; the native first-run loop should prefer `commit`.

`heddle capture --output json` emits:

```json
{
  "output_kind": "capture",
  "status": "captured",
  "action": "capture",
  "change_id": "hd-capture123",
  "content_hash": "deadbeef",
  "intent": "tighten parser validation",
  "confidence": 0.86,
  "principal": {"name": "Ada Agent", "email": "ada-agent@example.com"},
  "agent": {
    "provider": "codex",
    "model": "gpt-5-codex"
  },
  "promotion_suggested": false,
  "heavy_impact_paths": [],
  "message": "captured state hd-capture123"
}
```

`heddle checkpoint --output json` emits:

```json
{
  "output_kind": "checkpoint",
  "status": "checkpointed",
  "action": "checkpoint",
  "change_id": "hd-capture123",
  "git_commit": "abc123",
  "summary": "wrote Git checkpoint abc123 for hd-capture123",
  "capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "committed_at": "2026-05-23T00:00:00Z"
}
```

`heddle commit --output json` emits:

```json
{
  "output_kind": "commit",
  "status": "committed",
  "action": "commit",
  "change_id": "hd-capture123",
  "git_commit": "abc123",
  "summary": "captured Heddle state and wrote Git checkpoint",
  "confidence": 0.9,
  "principal": {"name": "Ada Agent", "email": "ada-agent@example.com"},
  "agent": {
    "provider": "codex",
    "model": "gpt-5-codex"
  },
  "next_action": null,
  "next_action_template": null,
  "recommended_action": null,
  "recommended_action_template": null
}
```

`heddle undo|redo --output json` emit:

```json
{
  "output_kind": "undo",
  "status": "completed",
  "action": "undo",
  "message": "restored previous logical operation",
  "batches": [],
  "next_action": null,
  "next_action_template": null,
  "recommended_action": null,
  "recommended_action_template": null
}
```

`heddle undo --list --output json` emits the history view (its own
`output_kind: "undo_list"` discriminator — distinct from the `undo`/`redo`
payload above):

```json
{
  "output_kind": "undo_list",
  "batches": []
}
```

In native Heddle repositories, `git_commit` is `null` and the command
saves a Heddle state without recommending a Git checkpoint.

`heddle ready --output json` emits:

```json
{
  "output_kind": "ready",
  "status": "completed",
  "action": "ready",
  "message": "Thread 'feature/parser' is ready to integrate",
  "blockers": [],
  "warnings": [],
  "next_action": "heddle land --thread feature/parser --no-push",
  "recommended_action": "heddle land --thread feature/parser --no-push",
  "captured": true,
  "captured_state": "hd-sqr398dvx9ay",
  "thread_state": "ready",
  "report": {}
}
```

`heddle land --output json` emits:

```json
{
  "output_kind": "land",
  "status": "landed",
  "action": "land",
  "message": "Landed thread 'feature/parser'",
  "thread": "feature/parser",
  "captured": false,
  "checkpointed": true,
  "git_commit": "abc123",
  "synced": false,
  "integrated": true,
  "pushed": false,
  "pushed_remote": null,
  "performed_steps": ["merge", "checkpoint"],
  "skipped_steps": ["capture(no changes)", "sync(current)", "push(not requested)"],
  "merge_state": "hd-land123",
  "chosen_path": "capture_sync_merge_checkpoint"
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `change_id` | string | required when present | Stable Heddle state ID for the captured or committed state. |
| `content_hash` | string | required for `capture` | Short content hash for the captured state. |
| `intent` | string \| null | required for `capture` | User-provided intent/message, when supplied. |
| `confidence` | number \| null | required for `capture` | Agent or human confidence score, when supplied. |
| `principal`, `agent` | object / object \| null | required for `capture`/`commit` | Accountable principal and optional agent/model provenance recorded on the captured state. |
| `promotion_suggested`, `heavy_impact_paths` | bool / array<string> | required for `capture` | Thread-promotion signal. Empty array if none. |
| `output_kind`, `status` | string \| null | required when present | Stable output discriminator and machine status; `undo`/`redo` report `completed` or `preview`. |
| `message`, `summary` | string \| null | required when present | Human-readable result. |
| `next_action`, `recommended_action` | string \| null | required | Primary next command, if one is known. |
| `next_action_template`, `recommended_action_template` | object \| null | required | Fillable template metadata (`argv_template`, `required_inputs`, `agent_may_fill`) for the next/recommended command; present for every valid action, `null` when none. |
| `git_commit` | string \| null | required for `checkpoint`/`commit` | Git commit OID produced by the checkpoint path; `null` for native Heddle commits. |
| `capability`, `storage_model`, `committed_at` | string | required for `checkpoint` | Repository mode, storage model, and checkpoint timestamp. |
| `status` | string | required for `capture`/`checkpoint`/`commit`/`ready`/`land` | Machine-stable success status for the operation. |
| `action` | string | required for `capture`/`checkpoint`/`commit`/`undo`/`redo`/`land` | Logical operation name. |
| `batches` | array<object> | required for `undo`/`redo` | Oplog batches affected by the operation. Empty if none are reported. |
| `thread_state`, `report` | string \| null / object | required for `ready` | Readiness result and structured readiness report. |
| `thread`, `captured`, `checkpointed`, `synced`, `integrated`, `pushed`, `pushed_remote` | string / bool / string \| null | required for `land` | Thread landed, which local/publish steps completed, and the remote name pushed when publish ran. |
| `performed_steps`, `skipped_steps`, `merge_state`, `chosen_path` | array<string> / string \| null / string | required for `land` | Machine-readable path through the land loop and the merge state landed, when one exists. |
| `verification` | object \| null | required | Post-operation verification proof. `null` only for undo/redo paths that cannot compute it. |

---

## `heddle diff --output json`

Structured diff between two Heddle states, or between the current
state and worktree/default comparison target.

### Sample

Worktree-mode diff (`heddle diff` with no revision args) groups the
per-file changes into `{modified, added, deleted}` category arrays,
mirroring the `status` command's `changes` shape so a UI can derive
add/modify/delete badges from `diff` alone:

```json
{
  "output_kind": "diff",
  "from_state": "hd-base123",
  "to_state": null,
  "changes": {
    "modified": [{"path": "src/lib.rs", "kind": "modified"}],
    "added": [{"path": "src/new.rs", "kind": "added"}],
    "deleted": [{"path": "src/old.rs", "kind": "deleted"}]
  }
}
```

A state-to-state diff (`heddle diff <a> <b>`) instead emits `changes` as a
flat `array<object>` (the shape `merge --with-diff` embeds):

```json
{
  "output_kind": "diff",
  "from_state": "hd-base123",
  "to_state": "hd-head456",
  "changes": []
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `from_state`, `to_state` | string \| null | required | State identifiers resolved for the comparison. Worktree-mode diffs leave `to_state` null. |
| `changes` | object \| array<object> | required | Worktree mode: `{modified, added, deleted}` category arrays (each entry carries `path`, `kind`, and the other per-file diff fields; a `renamed` entry buckets under `modified`). State-to-state mode: a flat `array<object>` of file-level or semantic changes. Empty when there are no changes. |
| `semantic_changes` | array<object> \| null | optional | Semantic diff entries when semantic analysis is requested and available. |
| `context`, `broader_guidance` | array<object> \| null | optional | Context snippets and broader guidance when requested. |

---

## `heddle merge --preview --output json`

Preview a merge without changing the worktree.

### Sample

```json
{
  "status": "preview",
  "action": "merge",
  "message": "Would fast-forward main to hd-feature123",
  "fast_forward": true,
  "preview_only": true,
  "merge_state": null,
  "conflicts": [],
  "preview_summary": ["fast-forward feature/parser into main"],
  "thread_state": "ready",
  "freshness": "current",
  "changed_paths": ["src/parser.rs"],
  "changed_path_count": 1,
  "impact_categories": [],
  "promotion_suggested": false,
  "heavy_impact_paths": [],
  "semantic_result": "fast_forward",
  "conflict_count": 0,
  "thread_health": "ready",
  "blockers": [],
  "warnings": [],
  "next_action": "heddle land --thread feature/parser --push",
  "recommended_action": "heddle land --thread feature/parser --push",
  "diff": {}
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `status` | string \| null | required | Preview status. |
| `would_merge` | bool | required | Whether the preview believes the merge can proceed. |
| `blockers` | array<string> \| null | required | Reasons merge should not proceed. |
| `recommended_action`, `recommended_action_template` | string \| null, object \| null | required | Primary next command and its fillable template when one exists. |
| `diff` | object \| null | required | Preview diff payload. |
| `verification` | object \| null | required | Repository verification state after the preview. Preview mode does not mutate refs or the worktree, so this proves the decision surface was computed from a verified repository state. |

---

## `heddle start --output json`

Create an isolated or lightweight thread and report where work should
continue.

### Sample

```json
{
  "output_kind": "thread_start",
  "name": "parser-fast",
  "message": "started thread parser-fast",
  "thread": null,
  "path": "../parser-fast",
  "execution_path": "../parser-fast"
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `output_kind` | string | required | Stable output discriminator; `start` emits `thread_start`. |
| `name` | string | required | New thread name. |
| `message` | string | required | Human-readable result. |
| `thread` | object \| null | required | Thread summary when available. |
| `path`, `execution_path` | string \| null | required | Materialized checkout path and effective execution path. |
| `verification` | object \| null | required | Post-start verification proof. |

---

## `heddle thread create --output json`

Thread mutations report the action, the affected thread summary when
available, checkout paths, and post-command verification.

`heddle thread create|switch|rename --output json` emit:

```json
{
  "output_kind": "thread_create",
  "name": "feature/parser",
  "message": "Created thread 'feature/parser' at hd-sqr398dvx9ay",
  "thread": null,
  "path": null,
  "execution_path": null
}
```

---

## `heddle thread current --output json`

Print the attached thread name.

```json
{
  "thread": "main"
}
```

---

## `heddle thread captures --output json`

List recent saved states on a thread.

```json
[
  {
    "change_id": "hd-sqr398dvx9ay",
    "created_at": "2026-05-23T23:57:09Z",
    "intent": "capture parser fix",
    "confidence": 0.86,
    "agent": "codex-cli",
    "message": "capture parser fix",
    "summary": {
      "added": 1,
      "modified": 0,
      "deleted": 0,
      "total": 1
    }
  }
]
```

---

## `heddle thread drop --output json`

`heddle thread drop|refresh|promote --output json` emit:

```json
{
  "output_kind": "thread",
  "status": "completed",
  "action": "thread drop",
  "name": "feature/parser",
  "message": "Dropped thread 'feature/parser'",
  "next_action": null,
  "next_action_template": null,
  "recommended_action": null,
  "recommended_action_template": null,
  "thread": null,
  "path": null,
  "execution_path": null
}
```

---

## `heddle thread move --output json`

Move captured paths between isolated threads.

```json
{
  "from_thread": "feature/parser",
  "to_thread": "feature/tests",
  "moved_paths": ["src/parser.rs"],
  "source_change_id": "hd-src123",
  "target_change_id": "hd-tgt456",
  "message": "Moved selected paths between threads"
}
```

---

## `heddle thread absorb --output json`

Absorb a child thread into its parent, or preview the same operation.

```json
{
  "thread": "feature/parser",
  "into": "main",
  "preview_only": true,
  "conflicts": [],
  "merge_state": null,
  "message": "Merge preview completed"
}
```

---

## `heddle thread resolve --output json`

Report manual follow-up after a blocked or refreshed thread.

```json
{
  "output_kind": "resolve",
  "status": "completed",
  "action": "resolve",
  "message": "Thread requires a manual follow-up",
  "blockers": [],
  "warnings": [],
  "next_action": "heddle land --thread feature/parser --no-push",
  "recommended_action": "heddle land --thread feature/parser --no-push",
  "thread": "feature/parser"
}
```

---

## `heddle thread approve --output json`

Hosted approval records are pinned to a source thread state.
`heddle thread approve --output json` emits one approval; `thread approvals`
emits an array of the same object.

```json
{
  "id": "apr_123",
  "repo_path": "acme/project",
  "source_thread": "feature/parser",
  "target_thread": "main",
  "source_state": "hd-src123",
  "approver_user_id": "user_42",
  "note": "looks good",
  "approved_at": 1779580000,
  "expires_at": 1779666400
}
```

---

## `heddle thread approvals --output json`

```json
[
  {
    "id": "apr_123",
    "repo_path": "acme/project",
    "source_thread": "feature/parser",
    "target_thread": "main",
    "source_state": "hd-src123",
    "approver_user_id": "user_42",
    "note": "looks good",
    "approved_at": 1779580000,
    "expires_at": 1779666400
  }
]
```

---

## `heddle thread revoke-approval --output json`

```json
{
  "output_kind": "thread_revoke_approval",
  "deleted": true,
  "id": "apr_123"
}
```

---

## `heddle thread check-merge --output json`

```json
{
  "allowed": false,
  "unmet": [
    {
      "policy_id": "two-reviewers",
      "kind": "approval_count",
      "group_id": "maintainers",
      "reason": "needs two maintainer approvals",
      "needed": 2,
      "have": 1
    }
  ],
  "valid_approvals": []
}
```

---

## `heddle thread cleanup --output json`

```json
{
  "output_kind": "thread.cleanup",
  "status": "completed",
  "action": "thread.cleanup",
  "message": "would drop 1 merged thread(s) (would reclaim 12.0 KB)",
  "blockers": [],
  "warnings": [],
  "next_action": null,
  "recommended_action": null,
  "dry_run": true,
  "merged": [
    {
      "thread": "feature/parser",
      "id": "feature/parser",
      "reason": "merged",
      "age_seconds": 86400,
      "bytes": 12288,
      "execution_path": "/tmp/project-feature-parser"
    }
  ],
  "auto": [],
  "reclaimed_bytes": 0,
  "would_reclaim_bytes": 12288,
  "skipped": []
}
```

---

## `heddle thread show --output json`

Detailed thread summary plus the same verification proof used by status and
verification.

### Sample

```json
{
  "output_kind": "thread_show",
  "repository_label": "Git + Heddle",
  "name": "parser-fast",
  "operation": {},
  "remote_tracking": {},
  "base_state": "hd-base123",
  "base_root": "hd-base123",
  "current_state": "hd-head456",
  "path": "../parser-fast",
  "execution_path": "../parser-fast",
  "actor": {"provider": "codex-cli", "model": "oss-cold-flow"},
  "harness": "codex",
  "thinking_level": null,
  "usage_summary": {},
  "last_progress_at": null,
  "last_activity_at": null,
  "report_flush_state": null,
  "attach_reason": null,
  "thread_mode": "materialized",
  "thread_state": "active",
  "freshness": "current",
  "visibility": "local",
  "target_thread": null,
  "parent_thread": "main",
  "child_threads": [],
  "sibling_threads": [],
  "stack_depth": 1,
  "stale_from_parent": false,
  "task": null,
  "changed_paths": [],
  "promotion_suggested": false,
  "impact_categories": [],
  "heavy_impact_paths": [],
  "verification_summary": {},
  "confidence_summary": {},
  "integration_policy_result": {},
  "coordination_status": "clean",
  "is_current": true,
  "is_isolated": true,
  "thread_health": "clean",
  "blockers": [],
  "recommended_action": "",
  "recommended_action_template": null,
  "next_action": null,
  "next_action_template": null,
  "git_branch_tip": "abc123",
  "history_imported": true,
  "auto": false,
  "shared_target_dir": null,
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "git-overlay",
    "heddle_initialized": true,
    "git_branch": "parser-fast",
    "heddle_thread": "parser-fast",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Git overlay and Heddle agree",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  },
  "recovery_commands": []
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `output_kind` | string | required | Stable output discriminator; `thread show` emits `thread_show`. |
| `repository_label`, `repository_context` | string/object | required/optional | Presentation identity and optional managed-child parent/target context. |
| `name` | string | required | Thread name. |
| `operation`, `remote_tracking` | object | required | Operation and remote summaries; empty object if none. |
| `base_state`, `base_root`, `current_state` | string \| null | required | Thread anchors and tip. |
| `path`, `execution_path` | string \| null | required | Materialized checkout path and effective execution path. |
| `actor`, `harness`, `thinking_level` | object/string \| null | required | Attribution and execution context. |
| `thread_mode`, `thread_state`, `freshness` | enum \| null | required | Thread lifecycle and freshness. |
| `visibility`, `target_thread`, `parent_thread` | string \| null | required | Thread relationship metadata. |
| `child_threads`, `sibling_threads`, `changed_paths`, `blockers` | array<string> | required | Empty arrays when none. |
| `stack_depth`, `stale_from_parent`, `is_current`, `is_isolated`, `history_imported`, `auto` | number/bool | required | Coordination metadata. |
| `verification_summary`, `confidence_summary`, `integration_policy_result` | object | required | Structured readiness/coordination summaries. |
| `coordination_status`, `thread_health`, `recommended_action` | string | required | Current coordination state and next action. |
| `next_action`, `recommended_action_template`, `next_action_template` | mixed | required | Machine-readable action metadata; templates carry `argv_template`/`required_inputs`/`agent_may_fill` and are `null` when no action is needed. |
| `verification` | object | required | Full repository verification proof for this checkout. |
| `recovery_commands` | array<string> | required | Recovery commands from verification/advice. Empty when verified. |

---

## Clone, remotes, and transport schemas

`heddle clone --output json` emits:

```json
{
  "output_kind": "clone",
  "action": "clone",
  "status": "cloned",
  "success": true,
  "cloned": true,
  "transport": "git",
  "remote": "file:///tmp/source.git",
  "local": "work",
  "branch": "main",
  "repository_capability": "git-overlay",
  "commits_imported": 3,
  "states_created": 3
}
```

`heddle remote list --output json` emits:

```json
{
  "output_kind": "remote_list",
  "remotes": [
    {
      "name": "origin",
      "url": "file:///tmp/source.git",
      "source": "git",
      "is_default": true
    }
  ]
}
```

`heddle remote show --output json` emits:

```json
{
  "output_kind": "remote_show",
  "name": "origin",
  "url": "file:///tmp/source.git",
  "source": "git",
  "is_default": true
}
```

`heddle remote add|remove|set-default --output json` emit:

```json
{
  "output_kind": "remote_add",
  "status": "completed",
  "action": "remote_add",
  "name": "origin",
  "url": "file:///tmp/source.git",
  "default": null,
  "message": "Added remote"
}
```

## `heddle actor spawn --output json`

`heddle actor spawn|show --output json` emit an actor envelope with post-command
verification. Lists are also enveloped so agents never have to special-case a raw
array.

```json
{
  "actor": {
    "session_id": "agent-4dvta2dd6as3uzjrszmq",
    "thread": "actor/agent-4dvta2dd6as3uzjrszmq",
    "base_state": "hd-sqr398dvx9ay",
    "provider": "openai",
    "model": "gpt-5",
    "usage_summary": {},
    "attach_reason": "actor agent-4dvta2dd6as3uzjrszmq was spawned explicitly",
    "attach_precedence": ["explicit-actor-spawn"],
    "winning_attach_rule": "explicit-actor-spawn",
    "probe_source": "explicit_payload",
    "probe_confidence": 1.0,
    "status": "active",
    "started_at": "2026-05-24T00:00:00Z",
    "actor_chain": []
  }
}
```

---

## `heddle actor list --output json`

```json
{
  "actors": [],
  "active_only": false,
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "heddle-native",
    "heddle_initialized": true,
    "git_branch": null,
    "heddle_thread": "main",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Repository is not using the Git overlay",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  }
}
```

---

## `heddle actor done --output json`

```json
{
  "session_id": "agent-4dvta2dd6as3uzjrszmq",
  "status": "complete",
  "thread": "actor/agent-4dvta2dd6as3uzjrszmq",
  "coordination_status": "active"
}
```

---

## `heddle actor explain --output json`

```json
{
  "attached": false,
  "reason": "No active actor is registered for this checkout.",
  "repository": "/work/project",
  "detected": {
    "harness": "codex",
    "provider": "openai",
    "model": "gpt-5",
    "thinking_level": "high",
    "native_actor_key": "thread-123",
    "probe_source": "environment",
    "probe_confidence": 0.9
  },
  "environment": {
    "principal_name": "Cold Agent",
    "principal_email": "agent@example.com",
    "signals": ["CODEX_THREAD_ID"]
  },
  "recommended_action": "heddle actor spawn --no-thread --provider openai --model gpt-5",
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "heddle-native",
    "heddle_initialized": true,
    "git_branch": null,
    "heddle_thread": "main",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Repository is not using the Git overlay",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  }
}
```

---

## `heddle agent serve --output json`

Foreground daemon success emits one JSON value when the daemon exits cleanly.
Background startup refusals use the shared error envelope.

```json
{
  "output_kind": "agent_serve",
  "status": "stopped",
  "socket_path": "/work/project/.heddle/sockets/grpc.sock",
  "pid_path": "/work/project/.heddle/sockets/grpc.pid"
}
```

---

## `heddle agent status --output json`

```json
{
  "output_kind": "agent_status",
  "running": false,
  "pid": null,
  "socket_path": "/work/project/.heddle/sockets/grpc.sock",
  "pid_path": "/work/project/.heddle/sockets/grpc.pid",
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "heddle-native",
    "heddle_initialized": true,
    "git_branch": null,
    "heddle_thread": "main",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Repository is not using the Git overlay",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  }
}
```

---

## `heddle agent stop --output json`

```json
{
  "output_kind": "agent_stop",
  "stopped": false,
  "swept_stale": false,
  "pid": null,
  "reason": "no pidfile"
}
```

---

## `heddle agent reserve --output json`

`heddle agent reserve|heartbeat|release --output json` emit:

```json
{
  "reservation": {
    "session_id": "agent-kvd9yn2z5kk3ehm0x8be",
    "reservation_token": "agent-k3f2w58q7f8rmm3qj0v8",
    "thread": "main",
    "anchor_state": "hd-sqr398dvx9ay",
    "anchor_root": "32fc0aff",
    "status": "active",
    "path": null,
    "task": "implement parser",
    "provider": "openai",
    "model": "gpt-5",
    "harness": "codex",
    "thinking_level": "high",
    "probe_source": "app_protocol",
    "probe_confidence": 0.98
  }
}
```

---

## `heddle agent capture --output json`

`agent capture` is the session-validated form of `capture`; the success
shape is the same capture envelope.

```json
{
  "output_kind": "capture",
  "status": "captured",
  "action": "capture",
  "change_id": "hd-sqr398dvx9ay",
  "content_hash": "sha256:...",
  "intent": "agent capture",
  "confidence": 0.8,
  "promotion_suggested": false,
  "heavy_impact_paths": [],
  "message": "Captured hd-sqr398dvx9ay"
}
```

---

## `heddle agent ready --output json`

`agent ready` is the session-validated form of `ready`; the success shape is
the same ready envelope.

```json
{
  "output_kind": "ready",
  "status": "completed",
  "action": "ready",
  "message": "Thread is ready.",
  "blockers": [],
  "warnings": [],
  "next_action": null,
  "recommended_action": null,
  "captured": false,
  "captured_state": null,
  "thread_state": "ready",
  "report": {}
}
```

---

## `heddle agent list --output json`

```json
{
  "reservations": [],
  "alive_only": false,
  "thread": null,
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "heddle-native",
    "heddle_initialized": true,
    "git_branch": null,
    "heddle_thread": "main",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Repository is not using the Git overlay",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  }
}
```

---

## `heddle session start --output json`

`heddle session start|show|end --output json` emit:

```json
{
  "session": {
    "id": "sess-6ngly2zoky3ifhx2",
    "principal": "Ada <ada@example.com>",
    "created_at": "2026-05-24T00:00:00Z",
    "active": true,
    "segments": [
      {
        "id": "sess-6ngly2zoky3ifhx2-seg-1",
        "provider": "openai",
        "model": "gpt-5",
        "started_at": "2026-05-24T00:00:00Z"
      }
    ]
  }
}
```

---

## `heddle session segment --output json`

```json
{
  "segment": {
    "id": "sess-6ngly2zoky3ifhx2-seg-2",
    "provider": "openai",
    "model": "gpt-5.1",
    "started_at": "2026-05-24T00:05:00Z"
  }
}
```

---

## `heddle session list --output json`

```json
{
  "sessions": [],
  "active_only": false,
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "heddle-native",
    "heddle_initialized": true,
    "git_branch": null,
    "heddle_thread": "main",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Repository is not using the Git overlay",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  }
}
```

---

`heddle fetch --output json` emits:

```json
{
  "output_kind": "fetch",
  "remote": "origin",
  "refs_fetched": 1,
  "objects_fetched": 2
}
```

`heddle pull --output json` emits:

```json
{
  "output_kind": "pull",
  "action": "pull",
  "status": "updated",
  "pulled": true,
  "changed": true,
  "success": true,
  "transport": "git",
  "remote": "origin",
  "branch": "main",
  "old_git_head": "1111111111111111111111111111111111111111",
  "new_git_head": "2222222222222222222222222222222222222222",
  "old_state": "hd-old123",
  "new_state": "hd-head456",
  "states_created": 1,
  "commits_seen": 1,
  "commits_seen_scope": "branches_and_heddle_notes",
  "materialized_checkout": true,
  "changed_path_count": 1,
  "changed_paths": ["src/app.rs"]
}
```

`heddle push --output json` emits:

`heddle push <remote> <thread>` is accepted as a Git-shaped alias for
`heddle push <remote> --thread <thread>`; the JSON output contract is the same.

```json
{
  "output_kind": "push",
  "action": "push",
  "status": "pushed",
  "pushed": true,
  "changed": true,
  "success": true,
  "transport": "git",
  "remote": "origin",
  "push_scope": "current_thread",
  "ref_scope": "branch_and_heddle_notes",
  "git_notes_ref": "refs/notes/heddle",
  "refs_written": ["refs/heads/main", "refs/notes/heddle"],
  "git_notes_visibility_warning": "ordinary `git log --all` may show Heddle metadata commits from refs/notes/heddle",
  "git_tracking_remote": "origin",
  "git_remote_configured": {
    "name": "origin",
    "url": "file:///tmp/example.git"
  },
  "git_upstream_configured": {
    "branch": "main",
    "remote": "origin"
  },
  "tags_included": false,
  "force": false,
  "thread": "main",
  "next_action": null,
  "next_action_template": null,
  "recommended_action": null,
  "recommended_action_template": null
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `output_kind`, `action`, `status`, `success`, `cloned`, `transport`, `remote`, `local`, `branch`, `repository_capability` | mixed | required for successful `clone` | Stable clone envelope, transport, source, destination, checked-out branch, and initialized repository capability. |
| `commits_imported`, `states_created` | int \| null | required for Git-overlay `clone` | Import counts reported after clone verification. |
| `objects`, `state` | int/string \| null | native/hosted Heddle clone only | Transferred object count and resulting Heddle state when the transport is native Heddle rather than Git-overlay. |
| `verification` | object \| null | required for Git-overlay `clone` | Post-clone repository verification proof; clean clones report `clone_verification: "verified"`. |
| `remotes` | array<object> | required for `remote list` | Configured remotes. Empty if none. |
| `name`, `url`, `source`, `is_default` | string/string/string/bool | required for `remote show` and remote entries | Remote identity and default marker. |
| `refs_fetched`, `objects_fetched` | int | required for `fetch` | Fetch transfer counts. |
| `pulled`, `pushed`, `success` | bool \| null | required when present | Transport result booleans. Pull reports `pulled`; push reports `pushed`. |
| `action`, `status`, `transport` | string \| null | required for pull/push | Stable action name, outcome status, and transport kind. Git-overlay transfers report `transport: "git"`; native Heddle transfers report `transport: "heddle"`. |
| `branch`, `old_git_head`, `new_git_head`, `old_state`, `new_state`, `states_created`, `commits_seen`, `commits_seen_scope`, `materialized_checkout`, `changed_path_count`, `changed_paths` | mixed | Git-overlay pull only | Concrete Git/Heddle movement proof for a pull, including imported commit scope and materialized path changes. |
| `state`, `objects` | string/int \| null | native Heddle pull/push only | Resulting native Heddle state and transferred object count. Git-overlay transfers report Git ref publication details instead. |
| `push_scope`, `ref_scope`, `tags_included`, `thread` | string/bool \| null | Git-overlay push only | Whether the push published only the current thread or all threads, the concrete Git ref scope, whether tags were included, and the thread whose branch was pushed. |
| `force`, `force_discard_warning` | bool/string \| null | Git-overlay push only | Present for Git-overlay push. `force_discard_warning` is non-null when `--force` may move remote refs backward or discard remote-only commits. |
| `git_notes_ref`, `git_notes_visibility_warning` | string \| null | Git-overlay push only | Heddle metadata notes ref carried with the push and the human-visible Git disclosure for that ref. |
| `refs_written` | array<string> \| null | push | The fully-qualified Git refs this invocation actually wrote (e.g. `refs/heads/<thread>`, `refs/notes/heddle`); empty when the push was a no-op. Lets callers verify the round-trip with `git ls-remote`. |
| `git_tracking_remote`, `git_remote_configured`, `git_upstream_configured` | mixed | Git-overlay push only | Git config side effects when Heddle configures a remote or branch upstream during push. |
| `next_action`, `recommended_action`, `next_action_template`, `recommended_action_template` | mixed | required for push | Post-push action metadata promoted from verification; all are `null` when the push closes the remote loop. |
| `verification` | object | required for pull/push | Post-transfer verification proof. |

---

## `heddle adopt --output json`

One-command Git adoption. Initializes Heddle sidecar data when needed,
imports the requested Git refs, and returns the post-adoption verification proof.

### Sample

```json
{
  "output_kind": "adopt",
  "adopted": true,
  "initialized": true,
  "path": "/repo/.heddle",
  "refs": [],
  "commits_imported": 12,
  "states_created": 12,
  "branches_synced": 2,
  "tags_synced": 1,
  "skipped_non_commit_refs": 0,
  "partial_mirror_refs": 0,
  "already_in_sync": false
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `adopted`, `initialized`, `already_in_sync` | bool | required | Adoption outcome, whether `.heddle/` was created, and whether import found no new states. |
| `path` | string | required | Path to the Heddle sidecar data. |
| `refs` | array<string> | required | Refs explicitly requested with `--ref`; empty means all local refs were imported. |
| `commits_imported`, `states_created`, `branches_synced`, `tags_synced` | int | required | Git import counts. |
| `skipped_non_commit_refs`, `partial_mirror_refs` | int | required | Degraded import counts that may require inspection. |
| `verification` | object | required | Post-adoption repository verification proof. |

---

## `heddle bridge git status --output json`

Canonical surface for the Git-overlay bridge state. This is the
advanced Git-adapter surface, so its recovery actions intentionally
name `heddle bridge git import ...`. Native first-run flows should use
the `heddle adopt --ref <branch>` recommendation from `status`,
`init`, and `verification`. This is the only command whose JSON output
carries `git_overlay_import_hint`.

### Sample

```json
{
  "output_kind": "bridge_git_status",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "mirror_path": "/repo/.heddle/git",
  "mirror_initialized": true,
  "git_overlay_import_hint": {
    "current_branch": "main",
    "missing_branch_count": 1,
    "missing_branches": ["support/import-me"],
    "recommended_command": "heddle bridge git import --ref support/import-me"
  },
  "git_overlay_health": {
    "status": "needs_import",
    "clean": false,
    "summary": "1 Git branch tip(s) still need Heddle import",
    "recovery_commands": ["heddle bridge git import --ref support/import-me"],
    "checks": [
      {
        "name": "import",
        "status": "needs_import",
        "summary": "1 Git branch tip(s) still need Heddle import"
      }
    ]
  },
  "recommended_action": "heddle bridge git import --ref support/import-me",
  "recovery_commands": ["heddle bridge git import --ref support/import-me"]
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `repository_capability` | string | required | Same vocabulary as `heddle status`. |
| `storage_model` | string | required | Same. |
| `mirror_path` | string \| null | required | Path to the bridge mirror, when known. |
| `mirror_initialized` | bool | required | `true` when `.heddle/git` exists. |
| `git_overlay_import_hint` | object \| null | required | `null` when bridge is in sync. |
| `git_overlay_import_hint.current_branch` | string | required when hint is present | Active branch on the Git side. |
| `git_overlay_import_hint.missing_branch_count` | int | required when hint is present | Length of `missing_branches`. |
| `git_overlay_import_hint.missing_branches` | array<string> | required when hint is present | Branch names visible only on the Git side. |
| `git_overlay_import_hint.recommended_command` | string | required when hint is present | Suggested `heddle bridge git import …` invocation. |
| `git_overlay_health` | object | required | Health summary derived from the shared verification engine. |
| `recommended_action` | string | required | Top-level mirror of the verification engine's primary next command. |
| `recommended_action_template` | object \| null | required | Fillable template (`argv_template`/`required_inputs`/`agent_may_fill`) for the primary action; `null` when none. |
| `recovery_commands` | array<string> | required | Verification recovery commands. Empty when clean. |
| `verification` | object | required | Full `RepositoryVerificationState` proof payload shared with `heddle verify`. |

---

## `heddle log --output json`

State history walking from a given starting state.

### Sample

```json
{
  "output_kind": "log",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "states": [
    {
      "change_id": "hd-def456",
      "content_hash": "deadbeef",
      "intent": "Capture audit pipeline",
      "principal": "Ada <ada@example.com>",
      "agent": "anthropic/claude-opus-4-7",
      "confidence": 0.95,
      "created_at": "2026-05-09 12:00:00",
      "parents": ["hd-abc123"],
      "git_checkpoint": "abc123def456"
    }
  ]
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `repository_capability` | string | required | |
| `storage_model` | string | required | |
| `states` | array<object> | required | Empty array if no states match the query. |
| `states[].change_id` | string | required | Short change-id. |
| `states[].content_hash` | string | required | Short content hash. |
| `states[].intent` | string \| null | required | |
| `states[].principal` | string | required | `"Name <email>"` form. |
| `states[].agent` | string \| null | required | `"provider/model"` or `null`. |
| `states[].confidence` | float \| null | required | 0.0–1.0 or `null` if unset. |
| `states[].created_at` | string | required | `YYYY-MM-DD HH:MM:SS`. |
| `states[].parents` | array<string> | required | Short change-ids; empty for root. |
| `states[].git_checkpoint` | string \| null | required | Git commit OID, when checkpointed. |

`heddle log --reflog --output json` emits a different shape:

```json
{
  "repository_capability": "...",
  "storage_model": "...",
  "entries": [
    {"source": "logs", "reference": "HEAD", "old_oid": "...", "new_oid": "...",
     "actor": "Ada <ada@example.com>", "timestamp": "...", "message": "..."}
  ]
}
```

---

## `heddle show <state> --output json`

State detail view, pretty-printed.

### Sample

```json
{
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "change_id": "hd-def456",
  "change_id_full": "hd-def4561234567890abcdef",
  "content_hash": "deadbeef…",
  "tree": "…",
  "parents": ["hd-abc123"],
  "intent": "Capture audit pipeline",
  "confidence": 0.95,
  "principal": {"name": "Ada", "email": "ada@example.com"},
  "agent": {"provider": "anthropic", "model": "claude-opus-4-7"},
  "created_at": "2026-05-09T12:00:00Z",
  "status": "Captured",
  "verification": null,
  "git_checkpoint": "abc123def456"
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `change_id` | string | required | Short id. |
| `change_id_full` | string | required | Full 32-character id. |
| `content_hash` | string | required | Hex tree hash. |
| `tree` | string | required | Hex tree hash alias of `content_hash`. |
| `parents` | array<string> | required | Empty for root state. |
| `intent` | string \| null | required | |
| `confidence` | float \| null | required | |
| `principal` | object | required | `{name, email}`. |
| `agent` | object \| null | required | `{provider, model, session_id, policy_id}`. |
| `created_at` | string | required | RFC3339 timestamp. |
| `status` | string | required | Debug-printed `StateStatus`. |
| `verification` | object \| null | required | Test/coverage summary, when present. |
| `git_checkpoint` | string \| null | required | Git commit OID. |

---

## `heddle marker list --output json`

```json
{
  "markers": [
    {"name": "v1.0.0", "change_id": "hd-abc123"}
  ]
}
```

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `markers` | array<object> | required | Empty array when no markers exist. |
| `markers[].name` | string | required | Marker name. |
| `markers[].change_id` | string | required | Short change-id the marker points at. |

`heddle marker create|delete|show` emit:

```json
{"name": "v1.0.0", "change_id": "hd-abc123", "message": "Created marker 'v1.0.0' at hd-abc123"}
```

`marker delete --prefix` emits:

```json
{
  "deleted": [
    {"name": "tmp/audit", "change_id": "hd-abc123"}
  ],
  "count": 1,
  "message": "Deleted 1 marker with prefix 'tmp/'"
}
```

---

## `heddle thread list --output json`

```json
{
  "output_kind": "thread_list",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "hosted_enabled": false,
  "threads": [
    {
      "name": "feature/parser-fast",
      "current_state": "hd-def456",
      "is_current": true,
      "thread_state": "active",
      "freshness": "current",
      "blockers": [],
      "child_threads": [],
      "shared_target_dir": null
    }
  ],
  "available_git_refs": [
    {
      "name": "support/git-only",
      "git_commit": "9fceb02",
      "recommended_action": "heddle adopt --ref support/git-only"
    }
  ],
  "current": "feature/parser-fast",
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "git-overlay",
    "heddle_initialized": true,
    "git_branch": "main",
    "heddle_thread": "feature/parser-fast",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Git overlay and Heddle agree",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  },
  "recommended_action": "",
  "recovery_commands": []
}
```

The thread summary is large (40+ fields); the discipline rules apply
uniformly. See [`crates/cli/src/cli/commands/thread.rs`](../crates/cli/src/cli/commands/thread.rs)
for the field-level definition. Notable invariants:

- `current` is `null` when on detached HEAD.
- `threads` is empty when the repo has no threads.
- `available_git_refs` contains Git refs that Heddle can adopt but has
  not yet modeled as active/imported threads.
- `repository_label` is the human-facing identity; `repository_context`
  is present when the command is run inside a managed child checkout.
- All `Option<...>` fields serialize as explicit `null`.
- `child_threads`, `sibling_threads`, `blockers`, `changed_paths`, and
  `impact_categories` are empty arrays — never omitted.
- `shared_target_dir` is `null` when the thread uses cargo's default
  per-checkout `target/` (was previously omitted).

---

## `heddle workspace show --output json`

Control-tower view across every active thread.

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `repository`, `repository_capability`, `repository_label`, `storage_model`, `hosted_enabled` | scalars | required | `repository_label` is the human-facing identity. |
| `repository_context` | object | optional | Present for managed child checkouts; includes parent repository and recorded target/parent thread context. |
| `operation` | object \| null | required | |
| `remote_tracking` | object \| null | required | |
| `verification` | object | required | Full `RepositoryVerificationState`; top-level workspace recommendations defer to this when verification is blocked. |
| `recommended_action` | string | required | Empty string when no action. |
| `current_thread` | string \| null | required | |
| `groups` | array<object> | required | One per non-empty bucket; can be empty. |
| `groups[].id` | enum string | required | `current` / `stacked` / `parallel` / `ready` / `blocked` / `recent`. |
| `groups[].label` | string | required | Human label. |
| `groups[].threads` | array<ThreadSummary> | required | At least one element per emitted group. |
| `available_git_refs` | array<object> | required | Git refs available for optional adoption/import; not counted as active threads and not nested in `groups`. |
| `available_git_refs[].name`, `git_commit`, `recommended_action`, `recommended_action_template` | scalars/object | required except `recommended_action_template` may be null | Typed import guidance for a Git ref not yet modeled as a Heddle thread. |
| `thread_count` | int | required | |

```json
{
  "output_kind": "workspace_summary",
  "repository": "/work/project",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "hosted_enabled": false,
  "operation": null,
  "remote_tracking": null,
  "verification": {
    "verified": true,
    "status": "clean",
    "repository_mode": "git-overlay",
    "heddle_initialized": true,
    "git_branch": "main",
    "heddle_thread": "feature/parser-fast",
    "worktree_dirty": false,
    "import_state": "clean",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "Git overlay and Heddle agree",
    "recommended_action": "",
    "recovery_commands": [],
    "checks": []
  },
  "recommended_action": "heddle commit -m \"...\"",
  "current_thread": "feature/parser-fast",
  "groups": [
    {
      "id": "current",
      "label": "Current",
      "threads": []
    }
  ],
  "available_git_refs": [
    {
      "name": "support/git-only",
      "git_commit": "9fceb02",
      "recommended_action": "heddle adopt --ref support/git-only"
    }
  ],
  "thread_count": 1
}
```

---

## `heddle commands --output json`

Public command catalog for agents, shell integrations, and generated docs.
Use `heddle commands --output json` in automation. The catalog includes
native commands first and lower-level Git-adapter actions only where a
command explicitly belongs to that surface.

Agents can bound the response before parsing it:

```bash
heddle commands --output json --command commit
heddle commands --output json --command thread
heddle commands --output json --tier everyday
heddle commands --output json --mutating --supports-op-id
```

`--command <COMMAND>` matches an exact display path or a command-family
prefix, so `--command thread` returns `thread` and its public
subcommands. Repeat `--command` or `--tier` to include multiple slices.
`--mutating` keeps commands with `mutates: true`; `--supports-op-id`
keeps commands that accept caller-supplied replay ids.

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `executable_path` | string | required | Absolute path to the Heddle binary that produced this catalog when discoverable. Agent-facing `argv` values use this path so replay does not depend on `PATH` resolving the same binary. Falls back to `heddle` only when the executable cannot be resolved. |
| `commands` | array<object> | required | One entry per public command path. |
| `commands[].path` | array<string> | required | Command path tokens. |
| `commands[].display` | string | required | Joined command path. |
| `commands[].aliases` | array<string> | required | Alternate command spellings advertised by the command contract table. |
| `commands[].tier` | string | required | Derived discovery tier for broad filtering (`everyday`, `advanced`, or `hidden`). |
| `commands[].surface` | string | required | Product surface from the command contract table (`native`, `git_adapter`, `automation`, `admin`, or `internal`). |
| `commands[].help_visibility` | string | required | Human discovery placement from the command contract table (`everyday`, `advanced`, `git_adapter`, or `hidden`). |
| `commands[].help_rank` | int | required | Stable ordering key for human command discovery. Lower ranks appear earlier. |
| `commands[].canonical_command` | string \| null | required | Canonical Heddle command for Git-shaped aliases; `null` for native commands. |
| `commands[].canonical_action` | object \| null | required | Structured canonical mapping for Git-shaped aliases. Contains `command`, `kind`, `executable`, `note`, `argv`, and `template`; `null` for native commands. `kind` is `direct_command`, `command_family`, `workflow`, or `conceptual_home`. |
| `commands[].command_action` | object \| null | required | Agent-facing invocation advertised by the command contract table. Executable commands carry `argv`; fillable placeholders carry `template`. Group-only commands use `null`. |
| `commands[].summary` | string | required | First help line. |
| `commands[].has_subcommands` | bool | required | Whether the command has public children. |
| `commands[].supports_json` | bool | required | Whether the command supports JSON output. |
| `commands[].mutates` | bool | required | Whether the command can change repository or process state. |
| `commands[].supports_op_id` | bool | required | Whether the command accepts caller-supplied idempotent `--op-id` / `HEDDLE_OPERATION_ID`. |
| `commands[].persists_op_id` | bool | required | Whether the command contract may preserve a generated op-id across an interrupted retry loop. This is currently false for all commands; agents should supply explicit ids when they need replay. |
| `commands[].op_id_behavior` | string | required | Precise op-id contract (`none`, `explicit_replay`, or `generated_resume`). |
| `commands[].observe_only` | bool | required | Whether the command is contractually observe-only. |
| `commands[].may_initialize` | bool | required | Whether the command may create `.heddle`/repository metadata. |
| `commands[].may_import_git` | bool | required | Whether the command may import Git history or mappings. |
| `commands[].may_write_worktree` | bool | required | Whether the command may materialize or rewrite worktree files. |
| `commands[].may_move_ref` | bool | required | Whether the command may move Heddle or Git refs. |
| `commands[].destructive_requires_force` | bool | required | Whether destructive execution requires explicit force. |
| `commands[].writes_heddle_refs` | bool | required | Whether the command may write Heddle refs. |
| `commands[].writes_git_refs` | bool | required | Whether the command may write Git refs. |
| `commands[].writes_worktree` | bool | required | Whether the command may write worktree files. |
| `commands[].writes_config` | bool | required | Whether the command may write repository or user configuration. |
| `commands[].writes_hooks` | bool | required | Whether the command may install, remove, or rewrite hook files. |
| `commands[].network_io` | bool | required | Whether the command may contact a remote service or repository. |
| `commands[].daemon_process` | bool | required | Whether the command may start, stop, or otherwise control a daemon process. |
| `commands[].object_gc` | bool | required | Whether the command may compact, prune, or garbage-collect object storage. |
| `commands[].external_command` | bool | required | Whether the command may execute a caller-provided external command. |
| `commands[].requires_git_executable` | bool | required | Whether Heddle itself requires a `git` executable on `PATH` to run this command. Supported runtime commands report `false`; caller-provided external commands may still invoke whatever the caller chooses. |
| `commands[].destructive_data` | bool | required | Whether the command may delete or irreversibly rewrite data. |
| `commands[].side_effects` | array<string> | required | Derived side-effect summary. Use the boolean dimensions above for the exact machine contract; this list preserves `observe_only`, `initialize`, `import_git`, concrete dimension names, `destructive_requires_force`, `destructive_data`, or `mutation`. |
| `commands[].side_effect_class` | string | required | Derived side-effect class from the command contract table. |
| `commands[].first_run_behavior` | string | required | Derived first-run policy from the command contract table. |
| `commands[].json_kind` | string | required | JSON output class (`json`, `jsonl`, `json_or_jsonl`, or `none`). |
| `commands[].schema_verbs` | array<string> | required | Runtime schema verb(s) registered for this command. |
| `commands[].documented_schema_verbs` | array<string> | required | Schema verb(s) checked against samples in this document. |
| `commands[].options` | array<object> | required | Public flags/options local to that command. |
| `commands[].arguments` | array<object> | required | Public positional arguments local to that command. |
| `global_options` | array<object> | required | Public global flags accepted across commands. Hidden conditional flags such as `--op-id` are described by per-command fields instead of this broad list. |
| `recommended_action_placeholders` | array<string> | required | Explicit display-only placeholders that cannot parse directly through Clap until the caller supplies the missing value. |
| `recommended_action_templates` | array<object> | required | Structured fillable forms for display-only recommended actions. Agents may fill templates only when `agent_may_fill` is true. |

`command_action` is the per-command action contract. For example, `push`
advertises executable argv `["/path/to/heddle", "push"]`, while `adopt`
advertises the fillable template `["/path/to/heddle", "adopt", "--ref",
"<branch>"]` and `merge` advertises `["/path/to/heddle", "merge",
"<thread>", "--preview"]`.
Agents should execute `argv` directly and fill `template.argv_template`
only when they can supply every `required_inputs` value.

Op-id behavior is deliberately split so agents can avoid assuming more
than the command provides:

* `none` rejects `--op-id`; retry without an operation id.
* `explicit_replay` reserves the caller-supplied id before execution.
  Repeating the same command path and arguments with the same id replays
  the recorded stdout/stderr/exit status. Reusing the id with different
  arguments returns a typed `op_id_conflict`; an active reservation
  returns `op_id_in_flight`.
* `generated_resume` is reserved for commands that can generate and save
  an id for interrupted retry loops. No current command advertises this
  behavior, so agents must provide an id explicitly for replay safety.

`--op-id` / `HEDDLE_OPERATION_ID` is accepted only when the target command
advertises `supports_op_id: true`; inspect each command's `op_id_behavior`
instead of treating it as a global catalog option.

`heddle commands --output json` emits:

```json
{
  "executable_path": "/path/to/heddle",
  "commands": [
    {
      "path": ["status"],
      "display": "status",
      "tier": "everyday",
      "surface": "native",
      "help_visibility": "everyday",
      "help_rank": 10,
      "canonical_command": null,
      "canonical_action": null,
      "command_action": {
        "action": "heddle status",
        "executable": true,
        "argv": ["/path/to/heddle", "status"],
        "template": null
      },
      "summary": "Show repository status",
      "has_subcommands": false,
      "supports_json": true,
      "mutates": false,
      "supports_op_id": false,
      "persists_op_id": false,
      "op_id_behavior": "none",
      "observe_only": true,
      "may_initialize": false,
      "may_import_git": false,
      "may_write_worktree": false,
      "may_move_ref": false,
      "destructive_requires_force": false,
      "writes_heddle_refs": false,
      "writes_git_refs": false,
      "writes_worktree": false,
      "writes_config": false,
      "writes_hooks": false,
      "network_io": false,
      "daemon_process": false,
      "object_gc": false,
      "external_command": false,
      "requires_git_executable": false,
      "destructive_data": false,
      "side_effects": ["observe_only"],
      "side_effect_class": "observe_only",
      "first_run_behavior": "observe_only_no_init",
      "json_kind": "json_or_jsonl",
      "schema_verbs": ["status"],
      "documented_schema_verbs": ["status"],
      "options": [
        {
          "id": "short",
          "long": "short",
          "short": "s",
          "value_names": [],
          "help": "Short format",
          "required": false,
          "global": false
        }
      ],
      "arguments": []
    }
  ],
  "global_options": [
    {
      "id": "output",
      "long": "output",
      "short": null,
      "value_names": ["OUTPUT"],
      "help": "Output format. `auto` (default) renders text on a TTY and JSON when piped; `json` and `text` override regardless of stream",
      "required": false,
      "global": true
    }
  ],
  "recommended_action_placeholders": [
    "heddle commit -m \"...\"",
    "heddle commit -m \"...\"",
    "heddle commit -m \"...\"",
    "heddle ready -m \"...\"",
    "heddle stash push -m \"...\"",
    "heddle remote add <name> <url>",
    "heddle clone <remote> <path>",
    "heddle clone <remote> <new-path>",
    "heddle clone <remote> <fresh-path>",
    "heddle switch <branch>",
    "heddle merge <thread> --git-commit"
  ],
  "recommended_action_templates": [
    {
      "action": "heddle commit -m \"...\"",
      "argv_template": ["/path/to/heddle", "commit", "-m", "<message>"],
      "required_inputs": ["message"],
      "agent_may_fill": true
    }
  ]
}
```

---

## `heddle review show --output json`

Hosted-review payload for a single state.

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `change_id` | string | required | Renamed from `state_id`. |
| `headline` | string | required | |
| `agent_narrative` | string \| null | required | |
| `files_changed` | int | required | |
| `in_budget_signals` | array<SignalView> | required | Empty array if none. |
| `all_signals` | array<SignalView> | required | Empty unless `--all-signals`. |
| `discussions` | array<DiscussionView> | required | Empty array if none. |
| `signing_kinds` | array<string> | required | |
| `signatures` | array<SignatureView> | required | |

```json
{
  "output_kind": "review_show",
  "change_id": "hd-def456",
  "headline": "Tighten parser recovery",
  "agent_narrative": null,
  "files_changed": 3,
  "in_budget_signals": [],
  "all_signals": [],
  "discussions": [],
  "signing_kinds": ["human"],
  "signatures": []
}
```

`heddle review sign --output json` emits:

```json
{"output_kind": "review_sign", "signature_id": "...", "change_id": "..."}
```

`heddle review next --output json` emits a stable envelope keyed by
`output_kind: "review_next"`. When the scan window holds a pending
review, the pending state's view is flattened alongside `output_kind`
(`change_id`, `headline`, `existing_signatures`) and the same view is
echoed under `next`. When the scan window holds no pending review, the
payload carries only `output_kind` and `next: null` — never a
top-level `null`.

```json
{"output_kind": "review_next", "change_id": "hd-def456", "headline": "Tighten parser recovery", "existing_signatures": 0, "next": {"change_id": "hd-def456", "headline": "Tighten parser recovery", "existing_signatures": 0}}
```

`heddle review health --output json` emits:

```json
{"output_kind": "review_health", "entries": [{"module_id": "...", "fire_rate": 0.42, "warn": false}], "window_states": 12}
```

---

## `heddle transaction commit`

```json
{"change_id": "hd-def456", "op_count": 7}
```

`change_id` was previously named `state_id`; the rename matches the
canonical naming for state identifiers across the CLI.

---

## `heddle transaction begin|abort|status --output json`

Hidden transaction-management commands are schema-backed so agents can
discover and validate internal recovery flows.

```json
{"status": "ok"}
```

---

## `heddle integration relay --output json`

Hidden integration relay output is registered as a generic object payload.

```json
{"status": "ok"}
```

---

## `heddle maintenance index --output json`

Maintenance index inspection emits one concrete JSON value. `--dump` places the
human-readable dump text in `dump` instead of writing a second stdout payload.

```json
{
  "output_kind": "index",
  "present": true,
  "path": "/repo/.heddle/state/index.bin",
  "file_entries": 12,
  "directory_entries": 4,
  "untracked_directory_entries": 1,
  "snapshot_bytes": 1024,
  "journal_bytes": 128,
  "journal_ops": 3,
  "journal_replay_ms": 0,
  "dump": null
}
```

---

## `heddle harness-bridge --output json`

Hidden harness bridge output is JSONL-capable and registered for automation
contract coverage.

```json
{"event": "ready"}
```

---

## `heddle bridge git init|export|import|sync|push|pull --output json`

All bridge ops emit JSON via `serde_json::json!{}` with consistent
key naming:

| Verb | Shape |
|------|-------|
| `init` | `{"initialized": true, "path": "..."}` |
| `export` | `{"states_exported": N, "threads_synced": N, "markers_synced": N, "destination": "..."}` |
| `import` | `{"output_kind": "bridge_git_import", "commits_imported": N, "states_created": N, "branches_synced": N, "tags_synced": N, "skipped_non_commit_refs": N, "partial_mirror_refs": N, "lossy_entries": [], "already_in_sync": false}` |
| `sync` | `{"output_kind": "bridge_git_sync", "states_exported": N, "commits_imported": N, "threads_synced": N, "markers_synced": N}` |
| `push` | `{"output_kind": "bridge_git_push", "action": "bridge git push", "status": "pushed", "success": true, "pushed": true, "changed": true, "transport": "git", "remote": "origin"}` |
| `pull` | `{"output_kind": "bridge_git_pull", "action": "bridge git pull", "status": "updated", "success": true, "pulled": true, "changed": true, "transport": "git", "remote": "origin"}` |

`heddle bridge git init --output json` emits:

```json
{"initialized": true, "path": "/work/project/.heddle/git"}
```

`heddle bridge git export --output json` emits:

```json
{"states_exported": 3, "threads_synced": 1, "markers_synced": 2, "destination": "/work/project/.heddle/git"}
```

`heddle bridge git import --output json` emits:

```json
{"output_kind": "bridge_git_import", "commits_imported": 4, "states_created": 4, "branches_synced": 2, "tags_synced": 1, "skipped_non_commit_refs": 0, "partial_mirror_refs": 0, "lossy_entries": [], "already_in_sync": false}
```

`heddle bridge git sync --output json` emits:

```json
{"output_kind": "bridge_git_sync", "states_exported": 3, "commits_imported": 4, "threads_synced": 1, "markers_synced": 2}
```

`heddle bridge git push --output json` emits:

```json
{"output_kind": "bridge_git_push", "action": "bridge git push", "status": "pushed", "success": true, "pushed": true, "changed": true, "transport": "git", "remote": "origin"}
```

`heddle bridge git pull --output json` emits:

```json
{"output_kind": "bridge_git_pull", "action": "bridge git pull", "status": "updated", "success": true, "pulled": true, "changed": true, "transport": "git", "remote": "origin"}
```

### Bridge Git Import Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `lossy_entries` | array<object> | required | Entries dropped or converted only when `--lossy` was explicitly passed; empty for lossless imports. |

---

## `heddle schemas --output json`

List every runtime schema verb and the subset enforced by
`heddle doctor schemas`.

```json
{
  "output_kind": "schemas",
  "schema_verbs": ["status", "verify", "try"],
  "documented_schema_verbs": ["status", "verify", "try"]
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `schema_verbs` | array<string> | required | Every verb with a runtime JSON Schema mirror. |
| `documented_schema_verbs` | array<string> | required | Schema verbs whose samples are checked against this document. |

---

## `heddle doctor --output json`

Doctor is the comprehensive health report; it includes the shared
verification report and the primary recovery command. This is the one
place outside `bridge git status` where `git_overlay_import_hint` is part
of the JSON contract — doctor is the catch-all health surface and its job
is to surface every relevant signal for the operator.

```json
{
  "output_kind": "diagnose",
  "repository": "/work/project",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "hosted_enabled": false,
  "git_overlay_import_hint": null,
  "git_overlay_health": {"status": "clean", "clean": true, "summary": "Git overlay and Heddle agree", "recovery_commands": [], "checks": []},
  "verification": {"verified": true, "status": "clean", "checks": [], "recommended_action": "", "recovery_commands": []},
  "operation": null,
  "remote_tracking": null,
  "thread": null,
  "state": null,
  "changes": {"modified": [], "added": [], "deleted": []},
  "workspace": {"thread_count": 0},
  "health": {"status": "clean"},
  "recommended_action": "",
  "recovery_commands": [],
  "profile": null
}
```

---

## `heddle doctor docs --output json`

Validate markdown command examples against the live Clap command
surface.

```json
{
  "output_kind": "doctor_docs",
  "status": "clean",
  "verified": true,
  "recommended_action": null,
  "files_scanned": 42,
  "issues": []
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `output_kind` | string | required | Always `doctor_docs`. |
| `status` | string | required | `clean` when no drift is found, otherwise `drift`. |
| `verified` | bool | required | True when docs examples match the live CLI surface. |
| `recommended_action`, `recommended_action_template` | string \| null, object \| null | required | Re-run command for CI/debugging when drift exists, plus its fillable template. |
| `files_scanned` | number | required | Markdown files checked. |
| `issues` | array<object> | required | Drift findings with `file`, `line`, `invocation`, `kind`, `detail`, and optional `suggestion`. |

---

## `heddle doctor schemas --output json`

Validate this document against the runtime schema registry and report
catalog-wide schema coverage.

```json
{
  "output_kind": "doctor_schemas",
  "status": "available",
  "verified": true,
  "summary": "196 command(s), 168 JSON command(s), 100 mutating command(s), 99 mutating JSON command(s); verified everyday/agent machine surface has 38 concrete schema-backed JSON command(s); advanced/internal/admin surfaces carry 51 accepted opaque schema(s) outside clean verification",
  "recommended_action": null,
  "recovery_commands": [],
  "registered_verbs": ["status", "verify", "try"],
  "documented_verbs": ["status", "verify", "try"],
  "undocumented_verbs": [],
  "unmatched_verbs": [],
  "passing_verbs": ["status", "verify", "try"],
  "issues": [],
  "command_contract_schema_coverage": {
    "status": "available",
    "verified_scope": "everyday_and_agent",
    "advanced_scope": "advanced_internal_admin",
    "summary": "203 command(s), 174 JSON command(s), 104 mutating command(s), 103 mutating JSON command(s); verified everyday/agent machine surface has 38 concrete schema-backed JSON command(s); advanced/internal/admin surfaces carry 56 accepted opaque schema(s) outside clean verification",
    "catalog_commands_total": 203,
    "catalog_mutating_commands_total": 104,
    "json_commands_total": 174,
    "json_mutating_commands_total": 103,
    "json_commands_with_schema": 118,
    "json_commands_with_accepted_opaque_schema": 56,
    "json_commands_without_schema": 0,
    "verified_scope_json_commands_total": 38,
    "verified_scope_json_commands_with_schema": 38,
    "verified_scope_json_commands_with_accepted_opaque_schema": 0,
    "verified_scope_json_commands_without_schema": 0,
    "advanced_scope_json_commands_total": 136,
    "advanced_scope_json_commands_with_accepted_opaque_schema": 56,
    "mutating_commands_total": 103,
    "mutating_commands_with_schema": 74,
    "mutating_commands_with_accepted_opaque_schema": 29,
    "mutating_commands_without_schema": 0,
    "verified_scope_mutating_commands_total": 23,
    "verified_scope_mutating_commands_with_schema": 23,
    "verified_scope_mutating_commands_with_accepted_opaque_schema": 0,
    "verified_scope_mutating_commands_without_schema": 0,
    "advanced_scope_mutating_commands_total": 80,
    "advanced_scope_mutating_commands_with_accepted_opaque_schema": 29,
    "undocumented_schema_verbs_total": 0,
    "opaque_schema_verbs_total": 56,
    "accepted_opaque_schema_verbs_total": 56,
    "unaccepted_opaque_schema_verbs_total": 0,
    "missing_schema_examples": [],
    "missing_mutating_schema_examples": [],
    "verified_scope_missing_schema_examples": [],
    "verified_scope_accepted_opaque_schema_examples": [],
    "advanced_scope_accepted_opaque_schema_examples": [
      "transaction begin",
      "transaction abort",
      "transaction status",
      "conflict list",
      "conflict show",
      "redact apply",
      "redact list",
      "redact show"
    ],
    "accepted_opaque_schema_examples": [
      "transaction begin",
      "transaction abort",
      "transaction status",
      "conflict list",
      "conflict show",
      "redact apply",
      "redact list",
      "redact show"
    ],
    "unaccepted_opaque_schema_examples": [],
    "undocumented_schema_examples": []
  },
  "doc_path": "/repo/docs/json-schemas.md"
}
```

---

## `heddle git-overlay --output json`

The built-in Git-overlay guide as structured steps. This is a guide
surface, not repository state.

```json
{
  "topic": "git-overlay",
  "summary": "Use Heddle as the daily Git-overlay loop: status, diff, commit, start --path, ready, land, push, undo, verification.",
  "steps": [
    "heddle status",
    "heddle adopt --ref <branch>",
    "heddle diff",
    "heddle commit -m <message>",
    "heddle start <name> --path ../<name>",
    "heddle ready",
    "heddle land --thread <name> --no-push",
    "heddle push",
    "heddle undo",
    "heddle verify"
  ]
}
```

---

## `heddle watch --output json`

`watch` emits JSONL: one object per observed oplog event.

```json
{
  "ts": "2026-05-23T00:00:00Z",
  "thread": "main",
  "kind": "snapshot",
  "change_id": "hd-sqr398dvx9ay",
  "intent": "capture parser fix",
  "confidence": 0.91,
  "actor": {"provider": "openai", "model": "gpt-5"},
  "id": 12
}
```

---

## `heddle try --output json`

Run a command in an ephemeral isolated thread. A successful run leaves
the thread ready for merge unless `--auto-merge` lands and drops it.
Every action field is a single parseable command; discard guidance
lives in `recovery_commands`.

```json
{
  "status": "completed",
  "action": "try",
  "message": "`cargo test` succeeded; thread 'try-1234abcd' ready (state hd-sqr398dvx9ay). Run `heddle ready --thread try-1234abcd` to land.",
  "thread": "try-1234abcd",
  "thread_dropped": false,
  "exit_code": 0,
  "duration_ms": 1420,
  "captured_state": "hd-sqr398dvx9ay",
  "merge_state": null,
  "next_action": "heddle ready --thread try-1234abcd",
  "recommended_action": "heddle ready --thread try-1234abcd",
  "recovery_commands": ["heddle thread drop try-1234abcd"]
}
```

---

## `heddle attempt --output json`

Run N isolated candidates and recommend the best thread to inspect or merge.

```json
{
  "status": "completed",
  "action": "attempt",
  "message": "2/3 attempt(s) succeeded; recommended: attempt-1",
  "command": "cargo test",
  "evaluate": "cargo test",
  "attempts_total": 3,
  "attempts_succeeded": 2,
  "attempts_dropped": 1,
  "attempts": [
    {
      "index": 1,
      "thread": "attempt-1",
      "status": "succeeded",
      "primary_exit_code": 0,
      "primary_duration_secs": 1.24,
      "evaluate_exit_code": 0,
      "evaluate_duration_secs": 0.81,
      "captured_state": "hd-sqr398dvx9ay",
      "diff_files": 2,
      "thread_dropped": false,
      "note": null
    }
  ],
  "recommended": "attempt-1",
  "next_action": "heddle ready --thread attempt-1"
}
```

---

## `heddle continue|abort --output json`

Operator recovery commands share one command-result envelope.

```json
{
  "output_kind": "continue",
  "status": "continued",
  "action": "continue",
  "message": "Operation continued",
  "blockers": [],
  "warnings": [],
  "next_action": null,
  "recommended_action": null
}
```

---

## `heddle sync --output json`

Refresh the active or named thread, or report the verification/action blocker.

```json
{
  "output_kind": "sync",
  "status": "refreshed",
  "action": "sync",
  "message": "Refreshed thread 'feature/parser'",
  "blockers": [],
  "warnings": [],
  "next_action": "heddle land",
  "recommended_action": "heddle land",
  "thread": "feature/parser",
  "current_state": "hd-sqr398dvx9ay",
  "chosen_path": "refresh"
}
```

---

## `heddle delegate --output json`

Create isolated delegated threads from the current thread.

```json
{
  "parent_thread": "main",
  "delegated": [
    {
      "name": "delegate-1",
      "task": "fix parser",
      "path": "../delegate-1",
      "execution_path": "../delegate-1"
    }
  ],
  "message": "Delegated 1 thread(s)"
}
```

---

## `heddle goto --output json`

Move the active checkout to a resolved state.

```json
{
  "output_kind": "goto",
  "target": "hd-sqr398dvx9ay",
  "intent": "capture parser fix",
  "message": "Now at: hd-sqr398dvx9ay"
}
```

---

## `heddle clean --output json`

List or remove untracked worktree paths.

```json
{
  "output_kind": "clean",
  "removed": ["tmp/output.txt"],
  "dry_run": true
}
```

---

## `heddle branch --output json`

Git adapter thread listing and mutation. With no branch name it
emits the same top-level verification/list contract as `thread list`; with a
name, delete, or rename it emits a thread operation result.

```json
{
  "output_kind": "thread_create",
  "name": "feature/parser",
  "message": "Created thread 'feature/parser' at hd-sqr398dvx9ay",
  "thread": {
    "name": "feature/parser",
    "operation": null,
    "remote_tracking": null,
    "base_state": "hd-sqr398dvx9ay",
    "base_root": "c5b5ee6e",
    "current_state": "hd-sqr398dvx9ay",
    "path": null,
    "execution_path": null,
    "actor": null,
    "harness": null,
    "thinking_level": null,
    "usage_summary": null,
    "last_progress_at": null,
    "last_activity_at": "2026-05-23T23:32:39Z",
    "report_flush_state": null,
    "attach_reason": null,
    "thread_mode": "materialized",
    "thread_state": "active",
    "freshness": "current",
    "visibility": "materialized",
    "target_thread": "main",
    "parent_thread": null,
    "child_threads": [],
    "sibling_threads": [],
    "stack_depth": 0,
    "stale_from_parent": false,
    "task": null,
    "changed_paths": [],
    "promotion_suggested": false,
    "impact_categories": [],
    "heavy_impact_paths": [],
    "verification_summary": {},
    "confidence_summary": {},
    "integration_policy_result": {},
    "coordination_status": "clean",
    "is_current": false,
    "is_isolated": true,
    "thread_health": "clean",
    "blockers": [],
    "recommended_action": "",
    "git_branch_tip": null,
    "history_imported": true,
    "auto": false,
    "shared_target_dir": null
  },
  "path": null,
  "execution_path": null
}
```

---

## `heddle switch --output json`

Switch to an existing thread, or fall through to the state-checkout
shape when the target resolves as a state rather than a thread.

```json
{
  "output_kind": "thread_switch",
  "name": "feature/parser",
  "message": "Switched to thread 'feature/parser'",
  "thread": null,
  "path": null,
  "execution_path": null
}
```

---

## `heddle bridge git reconcile --output json`

Preview or apply a ref reconciliation between Git and Heddle.

```json
{
  "output_kind": "bridge_git_reconcile",
  "status": "preview",
  "prefer": null,
  "ref_name": "main",
  "preview": true,
  "summary": "Preview: local Git/Heddle repair choices for 'main'. This does not push, pull, rewrite remotes, move refs, update the index, or change worktree files",
  "recovery_commands": [
    "heddle bridge git reconcile --prefer heddle --ref main --preview",
    "heddle bridge git reconcile --prefer git --ref main --preview"
  ]
}
```

---

## `heddle stack --output json`

`heddle stack` emits:

```json
{"output_kind": "stack", "thread": "main", "stack": null, "stacks": []}
```

`heddle stack ready` emits:

```json
{"output_kind": "stack_ready", "thread": "main", "next_action": {"kind": "unknown"}}
```

`heddle stack snapshot` emits a `RepositorySnapshot` flattened beneath the discriminator, so the root carries `output_kind` alongside `version`, `captured_at`, `stacks`, and `threads` — there is no `thread`/`snapshot` wrapper:

```json
{"output_kind": "stack_snapshot", "version": 1, "captured_at": "2026-05-28T15:43:36Z", "stacks": [{"root": {"name": "feature-x", "children": []}}], "threads": [{"thread": "feature-x", "parent_thread": null, "base_state": "hd-sqr398dvx9ay", "current_state": "hd-sqr398dvx9ay", "state": "active", "freshness": "current"}]}
```

---

## `heddle stash push --output json`

`heddle stash push|pop|apply|drop|clear --output json` emit:

```json
{
  "message": "Saved stash@{0}",
  "stash_index": 0
}
```

---

## `heddle stash list --output json`

List saved stash entries.

```json
{
  "output_kind": "stash_list",
  "stashes": [
    {
      "index": 0,
      "message": "save parser work",
      "created_at": "2026-05-23T23:32:39Z"
    }
  ]
}
```

---

## `heddle stash show --output json`

Show the top stash as path buckets.

```json
{
  "output_kind": "stash_show",
  "modified": ["src/parser.rs"],
  "added": ["tests/parser.rs"],
  "deleted": []
}
```

---

## `heddle revert --output json`

Apply the inverse of a state. With `--no-commit`, `change_id` is
`null`; otherwise it is the new revert state.

```json
{
  "output_kind": "revert",
  "change_id": null,
  "reverted_state": "hd-sqr398dvx9ay",
  "files_affected": ["M src/parser.rs"],
  "message": "Changes applied to worktree (not committed)"
}
```

---

## Additional runtime schema samples

Every verified everyday/agent runtime schema is a concrete machine-contract
mirror. Advanced/internal/admin opaque entries are counted separately
outside clean verification coverage.

`heddle blame --output json` emits structured attribution that mirrors
`log` / `show`: each line (and each entry in `origins`) carries a
`principal` object (`name`, `email`) and an `agent` field that is either
a structured object (`provider`, `model`, optional `session_id` /
`policy_id`) or `null` for human-only changes — no string-parsing
required:

```json
{"output_kind": "blame", "file": "src/lib.rs", "context": [], "lines": [{"line_number": 1, "content": "pub fn run() {}", "change_id": "hd-sqr398dvx9ay", "principal": {"name": "A. Engineer", "email": "a@example.com"}, "agent": {"provider": "anthropic", "model": "claude-opus-4-7"}, "timestamp": "2026-01-01T00:00:00Z", "origins": [{"change_id": "hd-sqr398dvx9ay", "principal": {"name": "A. Engineer", "email": "a@example.com"}, "agent": {"provider": "anthropic", "model": "claude-opus-4-7"}, "timestamp": "2026-01-01T00:00:00Z"}]}]}
```

`heddle bridge git ingest|reason --output json` emit:

```json
{"ingested": true, "commits_imported": 2, "states_created": 2, "reason": "mirror update", "remote": "origin"}
```

`heddle bridge backfill-fidelity --output json` emits the scanned/backfilled/skipped counts for the one-time #565 git-fidelity migration. `states_resigned` counts backfilled states whose own signature was re-signed over the new hash; `states_signature_unreproducible` counts states left untouched because they carry a signature this migration cannot reproduce (a foreign key or no local signer), so it never ships an invalid signature. `missing_mirror_commits` lists any mapping entries whose git object is absent from the mirror — those states could not be backfilled and are reported (as `{change_id, git_oid}`) rather than silently skipped:

```json
{"output_kind": "bridge_backfill_fidelity", "action": "bridge backfill-fidelity", "states_scanned": 2, "states_backfilled": 2, "states_skipped": 0, "states_resigned": 0, "states_signature_unreproducible": 0, "missing_mirror_commits": []}
```

`heddle cherry-pick --output json` emits the committed shape by default;
with `--no-commit` the `new_commit` field is replaced by `"no_commit":
true` and `status` is `"applied"`:

```json
{"output_kind": "cherry_pick", "status": "committed", "commit": "hd-source123", "new_commit": "hd-result456"}
```

`heddle collapse --output json` emits:

```json
{"change_id": "hd-collapsed123", "collapsed": 3, "message": "collapse feature checkpoints", "parents": ["hd-base123"]}
```

`heddle conflict list|show --output json` emit:

```json
{"conflicts": [{"id": "conflict-1", "kind": "content", "path": "src/lib.rs", "candidate_resolutions": []}]}
```

`heddle context set|get|list|history|edit|supersede|rm|check|suggest|audit --output json` emit per-subcommand shapes (each carries `output_kind` set to the snake-cased subcommand, e.g. `context_set`, `context_get`) — there is no single shared shape. For example, `context set` (and `edit`/`supersede`/`rm`) reports the mutated target and the new state:

```json
{"output_kind": "context_set", "target": "src/lib.rs", "annotations": 1, "state": "hd-k6a0wfrbgcg7"}
```

`heddle context list --output json` wraps its rows in an `items` envelope; the rows themselves carry no per-row discriminator (the envelope owns it). Each row is `{"target_kind": ..., "target": ..., "annotations": [...]}` — it emits:

```json
{"output_kind": "context_list", "items": [{"target_kind": "file", "target": "src/lib.rs", "annotations": [{"annotation_id": "hd-hy06md66hab4qb5ctkwphyc22r", "attribution": "A. Engineer <a@example.com>", "content": "returns false on timing mismatch", "created_at": 1767225600, "kind": "rationale", "revision_count": 1, "scope": "file", "status": "active", "supersedes_annotation_id": null, "supersedes_rewrite_pct": null, "tags": []}]}]}
```

`heddle daemon serve|status --output json` emit:

```json
{"running": true, "pid": 4242, "endpoint": "/work/project/.heddle/daemon.sock", "mounts": 1, "stopped": false}
```

`heddle daemon stop --output json` emits its own envelope (`status` is
`"stopped"` after a live daemon shuts down, `"not_running"` when there was
nothing to stop — both exit 0):

```json
{"output_kind": "daemon_stop", "action": "daemon stop", "status": "not_running"}
```

`heddle discuss open|append|resolve|show --output json` emit (each carries
`output_kind` set to the snake-cased subcommand, e.g. `discuss_open`,
`discuss_append`):

```json
{"output_kind": "discuss_open", "id": "disc-123", "file": "src/lib.rs", "symbol": "verify", "opened_against_state": "hd-sqr398dvx9ay", "opened_at_secs": 1767225600, "visibility": "team", "body_changed_since_open": false, "orphaned": false, "resolution": {"kind": "open", "annotation_id": null, "state_id": null, "reason": null}, "turns": [{"author_name": "A. Engineer", "author_email": "a@example.com", "body": "Please check this edge case.", "posted_at_secs": 1767225600}], "resolved_annotation_id": null}
```

`heddle discuss list --output json` emits:

```json
{"output_kind": "discuss_list", "discussions": [{"id": "disc-123", "file": "src/lib.rs", "symbol": "verify", "opened_against_state": "hd-sqr398dvx9ay", "opened_at_secs": 1767225600, "visibility": "team", "body_changed_since_open": false, "orphaned": false, "resolution": {"kind": "open", "annotation_id": null, "state_id": null, "reason": null}, "turns": [{"author_name": "A. Engineer", "author_email": "a@example.com", "body": "Please check this edge case.", "posted_at_secs": 1767225600}], "resolved_annotation_id": null}]}
```

`heddle fork --output json` emits (`thread` is `null` unless `--name`
names a new thread for the fork):

```json
{"output_kind": "fork", "change_id": "hd-result456", "content_hash": "b9c34842", "thread": "review/fix-parser", "from_state": "hd-sqr398dvx9ay", "message": "Created fork hd-result456 from hd-sqr398dvx9ay"}
```

`heddle fsck --output json` emits:

```json
{"valid": true, "errors": [], "warnings": [], "objects_checked": 42, "bridge_checked": false}
```

`heddle hook list|install|uninstall|events --output json` emit:

```json
{"hooks": [{"event": "pre-capture", "command": "cargo test"}], "installed": true, "uninstalled": false, "events": [{"name": "pre-capture", "description": "before capture"}]}
```

`heddle inspect --output json` emits:

```json
{"repository_capability": "native", "storage_model": "native", "change_id": "hd-sqr398d", "change_id_full": "hd-sqr398dvx9ay", "content_hash": "sha256:abc123", "tree": "sha256:def456", "parents": ["hd-base123"], "intent": "capture parser fix", "confidence": 0.91, "principal": {"name": "A. Engineer", "email": "a@example.com"}, "agent": {"provider": "codex", "model": "gpt-5", "session_id": "session-123"}, "created_at": "2026-01-01T00:00:00Z", "status": "Complete", "verification": {"tests_passed": true}, "git_checkpoint": "abc123"}
```

`heddle integration list|install|doctor|uninstall|upgrade --output json` emit:

```json
{"integrations": [{"name": "github", "installed": true, "version": "1"}], "installed": true, "uninstalled": false, "upgraded": false, "issues": []}
```

`heddle maintenance inspect|run|monitor --output json` emit:

```json
{"ok": true, "tasks": [{"name": "gc", "status": "skipped"}], "objects_removed": 0, "index_updated": true, "monitoring": false}
```

`heddle maintenance gc --output json` emits the pack/prune report (counts
are zero on a fresh repository; `pinned_redactions` / `preserved_redactions`
report redacted blobs the collector refused to touch):

```json
{"output_kind": "gc", "action": "gc", "status": "ok", "dry_run": false, "prune": false, "packed_count": 1, "bytes_saved": 0, "pruned_loose": 0, "bytes_freed": 0, "pinned_redactions": 0, "preserved_redactions": 0, "pruned_git_mapping_entries": 0}
```

`heddle purge apply|list --output json` emit (each carries `output_kind`
set to the snake-cased subcommand, e.g. `purge_apply`, `purge_list`).
`ignore_hint` is present only when the purged path is not yet covered by a
`.heddleignore` / `.gitignore` glob:

```json
{"output_kind": "purge_apply", "redaction_id": "redact-123", "blob": "sha256:abc123", "state": "hd-sqr398dvx9ay", "path": "secrets.env", "redactions_marked": 1, "blob_bytes_removed": false, "blob_remains_in_pack": false, "purger": "A. Engineer <a@example.com>", "message": "purged blob sha256:abc123 at secrets.env in hd-sqr398dvx9ay (1 redaction(s) marked)", "ignore_hint": {"ignore_file": ".heddleignore", "already_exists": false, "suggested_pattern": "secrets.env", "message": "hint: create .heddleignore with `secrets.env` so the next `heddle capture` doesn't re-import the leaked bytes"}}
```

`heddle query --output json` emits:

```json
{"output_kind": "query", "hits": [{"seq": 1, "timestamp_secs": 1767225600, "verb": "capture", "actor_email": "a@example.com", "operation_id": "op-123", "thread": "main", "symbols": ["verify"], "signal_kinds": ["test_passed"], "change_id": "hd-sqr398dvx9ay"}]}
```

`heddle rebase --output json` emits:

```json
{"rebased": true, "old_base": "hd-old123", "new_base": "hd-new456", "change_id": "hd-result789", "conflicts": []}
```

`heddle redact apply|list|show --output json` emit (each carries
`output_kind` set to the snake-cased subcommand, e.g. `redact_apply`,
`redact_list`). `signature_algorithm` is present only when the redaction
is signed (`--sign-with`); `ignore_hint` only when the path is not yet
covered by a `.heddleignore` / `.gitignore` glob:

```json
{"output_kind": "redact_apply", "redaction_id": "redact-123", "blob": "sha256:abc123", "state": "hd-sqr398dvx9ay", "path": "secrets.env", "reason": "credential", "redactor": "A. Engineer <a@example.com>", "redacted_at": "2026-01-01T00:00:00Z", "all_states": false, "states_redacted": 1, "signed": false, "ignore_hint": {"ignore_file": ".heddleignore", "already_exists": false, "suggested_pattern": "secrets.env", "message": "hint: create .heddleignore with `secrets.env` so the next `heddle capture` doesn't re-import the leaked bytes"}}
```

`heddle redact trust add|list|remove --output json` emit (each carries
`output_kind` set to the snake-cased subcommand, e.g. `redact_trust_add`,
`redact_trust_list`). The `add` payload flattens the added entry alongside
`output_kind` (`label` present only when `--label` is supplied); `list`
returns a `trusted_keys` array plus `count`; `remove` returns a `removed`
count:

```json
{"output_kind": "redact_trust_add", "algorithm": "ed25519", "public_key": "abc123def456", "label": "security"}
```

`heddle visibility set --output json` emits (each carries `output_kind`
set to the snake-cased subcommand). `label` is present only for the
`team-scoped` / `restricted` tiers; `supersedes` is omitted on a first
declaration:

```json
{"output_kind": "visibility_set", "state": "hd-sqr398dvx9ay", "tier": "internal", "record_id": "hd-vis123", "declarer": "A. Engineer <a@example.com>", "declared_at": "2026-01-01T00:00:00Z"}
```

`heddle visibility promote --output json` emits the same shape as `set`,
with `supersedes` carrying the prior record it replaces:

```json
{"output_kind": "visibility_promote", "state": "hd-sqr398dvx9ay", "tier": "internal", "record_id": "hd-vis456", "declarer": "A. Engineer <a@example.com>", "declared_at": "2026-01-01T00:00:00Z", "supersedes": "hd-vis123"}
```

`heddle visibility show --output json` emits the effective tier for one
state (`effective_public` is true and the declarer fields are omitted when
no record exists — public-by-absence):

```json
{"output_kind": "visibility_show", "state": "hd-sqr398dvx9ay", "tier": "internal", "effective_public": false, "declarer": "A. Engineer <a@example.com>", "declared_at": "2026-01-01T00:00:00Z", "record_count": 1}
```

`heddle visibility list --output json` emits every non-public state:

```json
{"output_kind": "visibility_list", "states": [{"state": "hd-sqr398dvx9ay", "tier": "internal", "declarer": "A. Engineer <a@example.com>", "declared_at": "2026-01-01T00:00:00Z"}], "count": 1}
```

`heddle resolve --output json` emits:

```json
{"message": "Resolved src/lib.rs", "resolved": ["src/lib.rs"], "remaining": []}
```

`heddle retro --output json` emits:

```json
{"since": "hd-base123", "until": "hd-head456", "duration_secs": 3600, "states_captured": [{"change_id": "hd-head456", "intent": "capture parser fix", "confidence": 0.91, "agent": "codex/gpt-5", "principal": "A. Engineer <a@example.com>", "timestamp": "2026-01-01T00:00:00Z"}], "agents_active": [{"session_id": "session-123", "provider": "codex", "model": "gpt-5", "status": "active", "started_at": "2026-01-01T00:00:00Z", "completed_at": null, "tokens": {"input": 1200, "output": 800, "reasoning": 300, "tool_calls": 12}}], "markers_created": [{"name": "verified-parser", "state": "hd-head456", "timestamp": "2026-01-01T00:00:00Z"}], "context_annotations": [{"path": "src/lib.rs", "scope": "file", "kind": "rationale", "content_excerpt": "Parser accepts the new token form.", "attribution": "A. Engineer <a@example.com>", "created_at": "2026-01-01T00:00:00Z"}], "verify_signals": [{"kind": "test_passed", "label": "verified: cargo test", "timestamp": "2026-01-01T00:00:00Z"}], "merges": [{"description": "Collapsed feature/parser", "timestamp": "2026-01-01T00:00:00Z"}], "undos": [{"description": "Undo capture", "timestamp": "2026-01-01T00:00:00Z"}]}
```

`heddle semantic hot --output json` emits:

```json
{"hotspots": [{"path": "src/lib.rs", "score": 0.87, "reasons": ["changed often"]}]}
```

## Other verbs

The following verbs also emit `--output json`. Their shapes follow the same
discipline; see the corresponding handler in `crates/cli/src/cli/commands/`:

`heddle blame`, `heddle checkpoint`, `heddle cherry-pick`,
`heddle clean`, `heddle clone`, `heddle collapse`,
`heddle conflict show`, `heddle context get/set`, `heddle diff`,
`heddle discuss`, `heddle doctor docs`, `heddle fetch`, `heddle fork`,
`heddle fsck`, `heddle goto`, `heddle init`, `heddle integration`,
`heddle maintenance`, `heddle merge`, `heddle ready`,
`heddle rebase`, `heddle remote`, `heddle resolve`, `heddle retro`,
`heddle session`, `heddle capture`,
`heddle support`, `heddle thread show/start`,
`heddle try`, `heddle attempt`, `heddle undo`, `heddle watch`.

Each of these:

- Emits a single JSON document on `--output json` (or one document per line for streaming verbs like `watch`).
- Uses `change_id` (not `state_id` or `id`) for state identifiers.
- Uses `created_at` (not `timestamp` or `recorded_at`) for state-creation timestamps.
- Serializes `Option<...>` semantic fields as explicit `null`.
- Serializes empty collections as `[]` / `{}`.
- Does not carry `git_overlay_import_hint` or `missing_branches`
  payloads; those live only in `heddle bridge git status` and
  `heddle doctor`.

---

## Error envelope (cross-cutting)

`error` emits the following stderr envelope when JSON output is selected
and the command fails. Stdout schemas above describe the success shape;
this schema describes the failure shape so scripts can parse failures
without scraping freeform text.

```json
{
  "error": "repository not found at /tmp/scratch",
  "exit_code": 1,
  "hint": "Run `heddle init` to initialize a repository here.",
  "kind": "repository_not_found",
  "unsafe_condition": "no Heddle repository was found at the requested path",
  "would_change": "the command cannot inspect or change repository state until initialization",
  "preserved": "no repository objects, refs, metadata, or worktree files were changed",
  "primary_command": "heddle init",
  "primary_command_template": null,
  "recovery_commands": ["heddle init"],
  "recovery_action_templates": []
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `kind` | string | required | Stable predicate name keying the hint class. The envelope's single discriminator — the redundant `code` mirror was dropped pre-1.0 (HeddleCo/heddle#647). |
| `error` | string | required | Human-readable failure message (the anyhow chain rendered via `{:#}`). Never empty. |
| `exit_code` | integer | required | Process exit code for this failure; currently `1`. |
| `hint` | string | required | One-line actionable next step. JSON-mode runtime errors use a non-empty fallback hint. |
| `unsafe_condition` | string | required | Why Heddle refused or could not safely continue. |
| `would_change` | string | required | What could be lost, duplicated, or changed by proceeding blindly. |
| `preserved` | string | required | What Heddle preserved or left untouched before failing. |
| `primary_command` | string | required | Main recovery/inspection command. |
| `primary_command_template` | object \| null | required | Fillable template (`argv_template`/`required_inputs`/`agent_may_fill`) for `primary_command`. The canonical machine-readable shape; the always-null `_argv` sidecar was dropped (HeddleCo/heddle#254). |
| `recovery_commands` | array<string> | required | Recovery commands in priority order. |
| `recovery_action_templates` | array<object> | required | Fillable templates mirroring `recovery_commands`. |
| `verification` | object | present for `kind: "verify_failed"` | Nested `RepositoryVerificationState` for the blocked `heddle verify` invocation. JSON callers should read this from stderr; blocked `verify` never writes the verification object to stdout. |

### Current `kind` values

These names are stable across releases. New values may be added; existing
ones do not change meaning.

| `kind`                  | Triggered by                                                                                         |
|-------------------------|------------------------------------------------------------------------------------------------------|
| `repository_not_found`  | A `HeddleError::RepositoryNotFound` surfaced in the chain — e.g. running `heddle status` outside a repo. |
| `repository_exists`     | `HeddleError::RepositoryExists` — e.g. running `heddle init` on an already-initialized directory.    |
| `state_not_found`       | `HeddleError::StateNotFound` or an anyhow message starting with `State not found:` from history lookups. |
| `thread_not_found`      | An anyhow message starting with `Thread not found:`.                                                 |
| `out_of_space`          | An underlying `io::Error` matching `objects::fs_atomic::is_out_of_space` (ENOSPC).                   |
| `permission_denied`     | An underlying `io::Error` matching `objects::fs_atomic::is_permission_denied`.                       |
| `read_only_filesystem`  | An underlying `io::Error` matching `objects::fs_atomic::is_read_only_filesystem`.                    |
| `path_not_found`       | A missing explicit filesystem path, such as `--repo /tmp/missing`.                                  |
| `operation_not_in_progress` | A continue/resolve/abort-style command was requested when no matching operation is active.    |
| `no_conflicts_to_resolve` | `heddle resolve --all` found no unresolved conflicts.                                             |
| `verify_failed`         | `heddle verify` found a blocked repository verification state. The envelope includes nested `verification`. |

### Stream contract

- Envelope is always on **stderr**, never stdout. Stdout stays available
  for partial output (an interrupted streaming verb may still flush
  bytes before the envelope appears on stderr).
- One envelope per process invocation. Polling scripts that retry on
  failure won't get a second envelope unless they re-invoke `heddle`.
- The default text mode equivalent is `Error: <message>\nNext: <command>`
  on stderr. Verbose text mode also prints the safety details carried
  by the structured JSON envelope.
