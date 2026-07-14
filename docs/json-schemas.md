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
    heddle schemas agent ready --output text
    heddle schemas status

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
3. **No leakage of unrelated context.** Git import guidance
   lives only in `heddle status --output json` (and the
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
* **`HEAD`, `@`, `HEAD~N`, `@~N`** — relative walks from the active
  thread's tip.
* **Thread name** — resolves to that thread's tip.

Verbs covered: `show`, `diff`, `revert`, `query --attribution --state`,
`log --since`, `review show`,
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
  "next_action": "heddle capture -m \"...\"",
  "recommended_action": "heddle capture -m \"...\""
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
| `next_action`, `recommended_action` | string \| null | required | Primary verification-guided next command. Git Overlay init proceeds directly to the normal save loop. |

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
    "recommended_action": null,
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
  "thread_mode": "lightweight",
  "thread_state": "active",
  "freshness": "current",
  "child_threads": [],
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
| `verification` | object | required | Full `RepositoryVerificationState`; status next actions defer to this when verification is blocked. |
| `thread` | string \| null | required | Current thread name; `null` for detached HEAD. |
| `base_state`, `base_root` | string \| null | required | Thread base anchor change-ids. |
| `current_state` | string \| null | required | Thread tip change-id. |
| `path` | string | optional | Materialized worktree path; omitted when no materialized/agent checkout context is recorded. |
| `execution_path` | string | optional | Effective execution root; omitted when no materialized/agent checkout context is recorded. |
| `session_id`, `heddle_session_id` | string | optional | Agent/session identifiers; omitted when no agent context is recorded. |
| `actor` | object | optional | `{provider, model}`; omitted when no agent context is recorded. |
| `harness`, `thinking_level`, `last_progress_at`, `report_flush_state`, `attach_reason` | string | optional | Agent execution metadata; omitted when no agent context is recorded. |
| `usage_summary` | object | optional | Agent token/tool/cost summary; omitted when no agent context is recorded. |
| `thread_mode` | enum \| null | required | `lightweight` / `materialized` / `virtualized`. |
| `thread_state` | enum \| null | required | Thread lifecycle: `active` / `ready` / `blocked` / `merged` / `abandoned` / `promoted`. Same values and meaning as `thread list`; repository-health/verification blockers surface via `coordination_status`, not here. |
| `freshness` | enum \| null | required | `current` / `stale` / `unknown`. |
| `target_thread`, `parent_thread`, `task` | string | optional | Agent/thread relationship and task metadata; omitted when no agent context is recorded. |
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

**Note:** Git import guidance is not part of this output.
Use `heddle status --output json`.

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
  "verification": {
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
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `output_kind` | string | required | Always `verify`; lets agents identify the proof payload. |
| `repository_label` | string | required | Human-facing repository identity; managed Git-overlay child checkouts use `"Git + Heddle isolated checkout"`. |
| `repository_context` | object | optional | Present for managed child checkouts; includes `kind`, `parent_repository`, and any recorded `target_thread` / `parent_thread`. |
| `clean` | bool | required | Alias of `verification.verified` for agents that sort command results into clean/blocked buckets. |
| `verification` | object | required | Full `RepositoryVerificationState`; this is the canonical verification proof shared with status, doctor, and post-operation reports. |
| `verification.verified` | bool | required | `true` only when all verification checks are clean or not applicable. |
| `verification.status` | string | required | Overall verification status, e.g. `clean`, `needs_import`, or `dirty_worktree`. |
| `verification.repository_mode`, `verification.heddle_initialized`, `verification.git_branch`, `verification.heddle_thread`, `verification.worktree_dirty`, `verification.worktree_state`, `verification.import_state`, `verification.mapping_state`, `verification.remote_drift`, `verification.active_operation`, `verification.default_remote`, `verification.clone_verification`, `verification.machine_contract`, `verification.machine_contract_coverage`, `verification.workflow_status`, `verification.workflow_summary` | mixed | required except nullable fields | Repository verification dimensions. |
| `verification.summary` | string | required | Human-sized explanation of the top verification state. |
| `verification.checks` | array<object> | required | Public checklist rows for Git, Heddle, Mapping, Worktree, Remote, Operation, Machine contract, and Clone. |
| `verification.recommended_action` | string \| null | required | Display command for the primary next step. `null` when no action is needed. |
| `verification.recommended_action_template` | object \| null | required | Fillable template for `recommended_action` — `argv_template` (executable argv, current Heddle executable path as argv[0]), `required_inputs`, `agent_may_fill`. Present for every valid action; `null` only when the display command is null. When `agent_may_fill` is false, treat `action`/`argv_template` as display-only: do not substitute `<name>`/`<url>` placeholders; surface the template to a human or discard it. Substituting and running it will pass literal `<name>` to Heddle and fail. The canonical machine-readable action shape — the always-null `_argv` sidecar was dropped (HeddleCo/heddle#254). |
| `verification.recovery_commands` | array<string> | required | Display commands for recovery, in priority order. Empty when verified. |
| `verification.recovery_action_templates` | array<object> | required | Fillable templates mirroring `recovery_commands`. |
| `verification.checks[].recommended_action_template`, `verification.checks[].recovery_action_templates` | object/array/null | required | Structured fillable action metadata scoped to the check row. |

### Blocked JSON verify

When verification is blocked, stdout is empty. The stderr envelope carries the
standard recovery fields plus nested verification proof:

```text
{
  "error": "Repository is not verified: dirty_worktree",
  "exit_code": 1,
  "hint": "Run `heddle capture -m <message>` to clear the primary verification blocker.",
  "kind": "verify_failed",
  "unsafe_condition": "worktree has unsaved changes",
  "would_change": "`heddle verify` is a strict proof gate and returns nonzero until every verification check is clean",
  "preserved": "verify is observe-only; repository objects, refs, index, and worktree files were left unchanged",
  "primary_command": "heddle capture -m <message>",
  "primary_command_template": {
    "action": "heddle capture -m <message>",
    "argv_template": ["heddle", "commit", "-m", "<message>"],
    "required_inputs": ["message"],
    "agent_may_fill": true
  },
  "recovery_commands": ["heddle capture -m <message>", "heddle verify"],
  "recovery_action_templates": [
    {
      "action": "heddle capture -m <message>",
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
`heddle help --output json`: capture state, undo/redo the last logical
operation, and ask whether a thread is ready. In Git Overlay repositories,
`heddle commit` writes the captured state to the authoritative `.git` store
through Sley; it does not require a Git executable.

`heddle capture --output json` emits:

```json
{
  "output_kind": "capture",
  "status": "captured",
  "action": "capture",
  "state_id": "hd-capture123",
  "content_hash": "deadbeef",
  "intent": "tighten parser validation",
  "confidence": 0.86,
  "task_assignment_id": "task-parser-validation",
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

`heddle commit --output json` emits after writing the current captured state
to Git Overlay source history. `-m/--message` is optional and defaults to the
capture intent. The command commits the complete captured tree, replaces the
Git index with that tree, and does not run Git `pre-commit` or `commit-msg`
hooks.

```json
{
  "output_kind": "commit",
  "status": "committed",
  "state_id": "hd-head456",
  "git_commit": "e97f61a",
  "summary": "committed",
  "recommended_action": null,
  "verification": {"verified":true,"status":"clean","repository_mode":"git-overlay","heddle_initialized":true,"git_branch":"main","heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"clean","mapping_state":"clean","remote_drift":"clean","active_operation":null,"default_remote":"origin","clone_verification":"verified","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Git overlay and Heddle agree","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}
}
```

`heddle undo` emits JSON when invoked with `--output json`:

```json
{
  "output_kind": "undo",
  "status": "completed",
  "action": "undo",
  "message": "restored previous logical operation",
  "batches": [],
  "next_action": "heddle undo --recover",
  "next_action_template": {"program": "heddle", "args": ["undo", "--recover"], "placeholders": []},
  "recommended_action": null,
  "recommended_action_template": null,
  "recovery_state": "hd-before123",
  "recovery_marker": ".undo-recovery"
}
```

`heddle undo --recover` emits JSON when invoked with `--output json`. It
materializes the checkout-local state preserved by the most recent undo as
dirty worktree changes. `HEAD` and the attached thread remain unchanged, so
the recovered work can be captured as new history:

```json
{
  "output_kind": "undo_recover",
  "status": "completed",
  "action": "recover",
  "message": "restored the state preserved by the most recent undo as worktree changes",
  "batches": [],
  "next_action": "heddle capture -m \"...\"",
  "next_action_template": {"program": "heddle", "args": ["capture", "-m", "<message>"], "placeholders": ["message"]},
  "recommended_action": "heddle capture -m \"...\"",
  "recommended_action_template": {"program": "heddle", "args": ["capture", "-m", "<message>"], "placeholders": ["message"]},
  "recovery_state": "hd-before123",
  "recovery_marker": ".undo-recovery"
}
```

`heddle undo --redo` emits JSON when invoked with `--output json`:

```json
{
  "output_kind": "redo",
  "status": "completed",
  "action": "redo",
  "message": "re-applied previously undone logical operation",
  "batches": [],
  "next_action": null,
  "next_action_template": null,
  "recommended_action": null,
  "recommended_action_template": null
}
```

`heddle undo --list --output json` emits the history view (its own
`output_kind: "undo_list"` discriminator — distinct from the `undo` / `undo --redo`
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
  "next_action": "heddle land --thread feature/parser",
  "recommended_action": "heddle land --thread feature/parser",
  "captured": true,
  "captured_state": "hd-sqr398dvx9ay",
  "thread_state": "ready",
  "readiness": {
    "status": "ready",
    "captured": true,
    "captured_state": "hd-sqr398dvx9ay",
    "checks": {
      "status": "completed",
      "reason": "readiness preview ran"
    },
    "integration": "configured",
    "freshness": "current",
    "merge_type": "fast-forward",
    "changed_path_count": 1,
    "changed_paths": ["src/parser.rs"],
    "conflict_count": 0,
    "conflicts": [],
    "impact": "none",
    "impact_categories": [],
    "blockers": []
  },
  "report": {},
  "verification": {"verified":true,"status":"clean","repository_mode":"native-heddle","heddle_initialized":true,"git_branch":null,"heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"not_applicable","mapping_state":"not_applicable","remote_drift":"clean","active_operation":null,"default_remote":"origin","clone_verification":"verified","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Repository is healthy","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}
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
  "performed_steps": ["merge", "checkpoint"],
  "skipped_steps": ["capture(no changes)", "sync(current)"],
  "merge_state": "hd-land123",
  "chosen_path": "capture_sync_merge_checkpoint"
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `change_id` | string | required when present | Stable Heddle state ID for the captured state. |
| `state_id`, `git_commit` | string | required for `commit` | Captured Heddle state and the Git commit written to the authoritative `.git` store. |
| `content_hash` | string | required for `capture` | Short content hash for the captured state. |
| `intent` | string \| null | required for `capture` | User-provided intent/message, when supplied. |
| `confidence` | number \| null | required for `capture` | Agent or human confidence score, when supplied. |
| `principal`, `agent` | object / object \| null | required for `capture` | Accountable principal and optional agent/model provenance recorded on the captured state. |
| `promotion_suggested`, `heavy_impact_paths` | bool / array<string> | required for `capture` | Thread-promotion signal. Empty array if none. |
| `output_kind`, `status` | string \| null | required when present | Stable output discriminator and machine status; `undo`, `undo --redo`, and `undo --recover` report `completed`; undo/redo previews report `preview`. |
| `message`, `summary` | string \| null | required when present | Human-readable result. |
| `next_action`, `recommended_action` | string \| null | required | Primary next command, if one is known. |
| `next_action_template`, `recommended_action_template` | object \| null | required | Fillable template metadata (`argv_template`, `required_inputs`, `agent_may_fill`) for the next/recommended command; present for every valid action, `null` when none. |
| `status` | string | required for `capture`/`ready`/`land` | Machine-stable success status for the operation. |
| `action` | string | required for `capture`/`undo`/`undo --redo`/`undo --recover`/`land` | Logical operation name. Recovery reports `recover`. |
| `batches` | array<object> | required for `undo`/`undo --redo`/`undo --recover` | Oplog batches affected by the operation. Recovery reports an empty array. |
| `thread_state`, `readiness`, `report` | string \| null / object / object | required for `ready` | Readiness result, stable human/machine summary, and structured preview report. `readiness` always carries the same fields; non-applicable checks/integration/freshness/merge details are represented with explicit `not_run`, `not checked`, or `n/a` values and reasons rather than omitted. |
| `thread`, `captured`, `checkpointed`, `synced`, `integrated` | string / bool | required for `land` | Thread landed and which local integration steps completed. |
| `performed_steps`, `skipped_steps`, `merge_state`, `chosen_path` | array<string> / string \| null / string | required for `land` | Machine-readable path through the land loop and the merge state landed, when one exists. |
| `verification` | object \| null | required | Post-operation verification proof. `null` only for undo / undo --redo paths that cannot compute it. |

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
flat `array<object>`:

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
| `fskit_readiness` | object \| null | optional | macOS FSKit enable state for virtualized starts when the CLI made an FSKit-specific decision; includes `state`, `backend`, `action`, and optional `settings_url`. Disabled FSKit fails without opening System Settings; an interactive terminal may opt into the bounded approval flow with `--interactive-setup`. |
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
  "output_kind": "thread_drop",
  "status": "completed",
  "action": "thread_drop",
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
  "source_state_id": "hd-src123",
  "target_state_id": "hd-tgt456",
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
  "output_kind": "thread_resolve",
  "status": "completed",
  "action": "thread_resolve",
  "message": "Thread requires a manual follow-up",
  "blockers": [],
  "warnings": [],
  "next_action": "heddle land --thread feature/parser",
  "recommended_action": "heddle land --thread feature/parser",
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
  "output_kind": "thread_cleanup",
  "status": "completed",
  "action": "thread_cleanup",
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
  "task_assignment_id": "task-parser-fast",
  "task_summary": {
    "task_id": "task-parser-fast",
    "title": "Tighten parser validation",
    "status": "in_progress",
    "target_thread": "parser-fast",
    "updated_at": "2026-01-01T00:00:00Z",
    "completed_at": null,
    "coordination_discussion_id": null
  },
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
    "recommended_action": null,
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
| `task_assignment_id`, `task_summary` | string/object \| null | required | Local agent task provenance for the active reservation, when present. `task_summary` carries title/status/thread metadata only. |
| `child_threads`, `sibling_threads`, `changed_paths`, `blockers` | array<string> | required | Empty arrays when none. |
| `stack_depth`, `stale_from_parent`, `is_current`, `is_isolated`, `history_imported`, `auto` | number/bool | required | Coordination metadata. |
| `verification_summary`, `confidence_summary`, `integration_policy_result` | object | required | Structured readiness/coordination summaries. |
| `coordination_status`, `thread_health`, `recommended_action` | string | required | Current coordination state and next action. |
| `next_action`, `recommended_action_template`, `next_action_template` | mixed | required | Machine-readable action metadata; templates carry `argv_template`/`required_inputs`/`agent_may_fill` and are `null` when no action is needed. |
| `verification` | object | required | Full repository verification proof for this checkout. |
| `recovery_commands` | array<string> | required | Recovery commands from verification/advice. Empty when verified. |

---

## `heddle thread marker list --output json`

```json
{
  "output_kind": "thread_marker_list",
  "markers": [
    {
      "name": "verified-parser",
      "state_id": "hd-def456"
    }
  ]
}
```

## `heddle thread marker create --output json`

```json
{
  "output_kind": "thread_marker_create",
  "name": "verified-parser",
  "state_id": "hd-def456",
  "message": "Created marker 'verified-parser' at hd-def456"
}
```

## `heddle thread marker delete --output json`

```json
{
  "output_kind": "thread_marker_delete",
  "name": "verified-parser",
  "state_id": null,
  "message": "Deleted marker 'verified-parser'"
}
```

## `heddle thread marker show --output json`

```json
{
  "output_kind": "thread_marker_show",
  "name": "verified-parser",
  "state_id": "hd-def456",
  "message": "Marker 'verified-parser' -> hd-def456"
}
```

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
  "transport": "heddle",
  "remote": "heddle://example.com/team/repo",
  "local": "work",
  "branch": "main",
  "repository_capability": "native-heddle",
  "objects": 42,
  "state": "hs-head456",
  "verification": {"verified":true,"status":"clean","repository_mode":"native-heddle","heddle_initialized":true,"git_branch":null,"heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"not_applicable","mapping_state":"not_applicable","remote_drift":"clean","active_operation":null,"default_remote":"origin","clone_verification":"not_applicable","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Repository is healthy","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}
}
```

`heddle remote list --output json` emits:

```json
{
  "output_kind": "remote_list",
  "remotes": [
    {
      "name": "origin",
      "url": "heddle://example.com/team/repo",
      "source": "heddle",
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
  "url": "heddle://example.com/team/repo",
  "source": "heddle",
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
  "url": "heddle://example.com/team/repo",
  "default": null,
  "message": "Added remote",
  "verification": {"verified":true,"status":"clean","repository_mode":"native-heddle","heddle_initialized":true,"git_branch":null,"heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"not_applicable","mapping_state":"not_applicable","remote_drift":"clean","active_operation":null,"default_remote":"origin","clone_verification":"not_applicable","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Repository is healthy","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}
}
```

## `heddle agent presence show --output json`

Presence inspection emits an envelope with post-command verification. Lists are
also enveloped so agents never have to special-case a raw array.

```json
{
  "output_kind": "agent_presence_show",
  "presence": {
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

## `heddle agent presence list --output json`

```json
{
  "output_kind": "agent_presence_list",
  "presence": [],
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
    "recommended_action": null,
    "recovery_commands": [],
    "checks": []
  }
}
```

---

## `heddle agent presence complete --output json`

```json
{
  "output_kind": "agent_presence_complete",
  "session_id": "agent-4dvta2dd6as3uzjrszmq",
  "status": "complete",
  "thread": "actor/agent-4dvta2dd6as3uzjrszmq",
  "coordination_status": "active"
}
```

---

## `heddle agent presence explain --output json`

```json
{
  "output_kind": "agent_presence_explain",
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
  "recommended_action": "heddle agent reserve --thread main",
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
    "recommended_action": null,
    "recovery_commands": [],
    "checks": []
  }
}
```

---

## `heddle agent serve --output json`

The foreground daemon emits one JSON value when it exits cleanly.

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

`heddle agent reserve --output json` emits the bearer token once. Heartbeat
and release emit the same reservation shape without `token`:

```json
{
  "reservation": {
    "lease_id": "lease-kvd9yn2z5kk3ehm0x8be",
    "actor_session_id": "agent-k3f2w58q7f8rmm3qj0v8",
    "thread": "main",
    "anchor_state": "hd-sqr398dvx9ay",
    "anchor_root": "32fc0aff",
    "task_assignment_id": null,
    "status": "active",
    "path": null,
    "heartbeat_at": "2026-07-12T23:15:00Z",
    "lease_expires_at": "2026-07-12T23:20:00Z",
    "liveness": "alive"
  },
  "token": "hwl_secret-token-material",
  "verification": {"verified":true,"status":"clean","repository_mode":"native-heddle","heddle_initialized":true,"git_branch":null,"heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"not_applicable","mapping_state":"not_applicable","remote_drift":"clean","active_operation":null,"default_remote":"origin","clone_verification":"not_applicable","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Repository is healthy","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}
}
```

---

## `heddle agent heartbeat --output json`

```json
{"reservation":{"lease_id":"lease-kvd9yn2z5kk3ehm0x8be","actor_session_id":"agent-k3f2w58q7f8rmm3qj0v8","thread":"main","anchor_state":"hd-sqr398dvx9ay","anchor_root":"32fc0aff","task_assignment_id":null,"status":"active","path":null,"heartbeat_at":"2026-07-12T23:16:00Z","lease_expires_at":"2026-07-12T23:21:00Z","liveness":"alive"},"token":null,"verification":{"verified":true,"status":"clean","repository_mode":"native-heddle","heddle_initialized":true,"git_branch":null,"heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"not_applicable","mapping_state":"not_applicable","remote_drift":"clean","active_operation":null,"default_remote":null,"clone_verification":"not_applicable","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Repository is healthy","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}}
```

## `heddle agent release --output json`

```json
{"reservation":{"lease_id":"lease-kvd9yn2z5kk3ehm0x8be","actor_session_id":"agent-k3f2w58q7f8rmm3qj0v8","thread":"main","anchor_state":"hd-sqr398dvx9ay","anchor_root":"32fc0aff","task_assignment_id":null,"status":"released","path":null,"heartbeat_at":"2026-07-12T23:16:00Z","lease_expires_at":"2026-07-12T23:16:00Z","liveness":"released"},"token":null,"verification":{"verified":true,"status":"clean","repository_mode":"native-heddle","heddle_initialized":true,"git_branch":null,"heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"not_applicable","mapping_state":"not_applicable","remote_drift":"clean","active_operation":null,"default_remote":null,"clone_verification":"not_applicable","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Repository is healthy","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}}
```

---

## `heddle agent capture --output json`

`agent capture` is the token-authenticated writer-lease form of `capture`; the success
shape is the same capture envelope.

```json
{
  "output_kind": "capture",
  "status": "captured",
  "action": "capture",
  "state_id": "hd-sqr398dvx9ay",
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

`agent ready` is the token-authenticated writer-lease form of `ready`; the success shape is
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

## `heddle agent task create --output json`

```json
{
  "output_kind": "agent_task_create",
  "task": {
    "schema_version": 1,
    "task_id": "task-output-kind",
    "title": "Output kind",
    "body": "Persist task provenance locally.",
    "status": "open",
    "target_thread": "feature/task",
    "base_state": null,
    "base_root": null,
    "parent_task_id": null,
    "coordination_discussion_id": null,
    "allow_offline": true,
    "delegated_by": "coordinator",
    "created_at": "2026-06-30T12:00:00Z",
    "updated_at": "2026-06-30T12:00:00Z",
    "completed_at": null
  },
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

## `heddle agent task list --output json`

```json
{
  "output_kind": "agent_task_list",
  "tasks": [
    {
      "schema_version": 1,
      "task_id": "task-output-kind",
      "title": "Output kind",
      "body": "Persist task provenance locally.",
      "status": "open",
      "target_thread": "feature/task",
      "base_state": null,
      "base_root": null,
      "parent_task_id": null,
      "coordination_discussion_id": null,
      "allow_offline": true,
      "delegated_by": "coordinator",
      "created_at": "2026-06-30T12:00:00Z",
      "updated_at": "2026-06-30T12:00:00Z",
      "completed_at": null
    }
  ],
  "thread": "feature/task",
  "status": null,
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

## `heddle agent task show --output json`

```json
{
  "output_kind": "agent_task_show",
  "task": {
    "schema_version": 1,
    "task_id": "task-output-kind",
    "title": "Output kind",
    "body": "Persist task provenance locally.",
    "status": "open",
    "target_thread": "feature/task",
    "base_state": null,
    "base_root": null,
    "parent_task_id": null,
    "coordination_discussion_id": null,
    "allow_offline": true,
    "delegated_by": "coordinator",
    "created_at": "2026-06-30T12:00:00Z",
    "updated_at": "2026-06-30T12:00:00Z",
    "completed_at": null
  },
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

## `heddle agent task update --output json`

```json
{
  "output_kind": "agent_task_update",
  "task": {
    "schema_version": 1,
    "task_id": "task-output-kind",
    "title": "Output kind complete",
    "body": "Persist task provenance locally.",
    "status": "complete",
    "target_thread": "feature/task",
    "base_state": null,
    "base_root": null,
    "parent_task_id": null,
    "coordination_discussion_id": null,
    "allow_offline": true,
    "delegated_by": "coordinator",
    "created_at": "2026-06-30T12:00:00Z",
    "updated_at": "2026-06-30T12:05:00Z",
    "completed_at": "2026-06-30T12:05:00Z"
  },
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

## `heddle agent fanout plan --output json`

```json
{
  "output_kind": "agent_fanout_plan",
  "title": "Coordinate Wave 4 lanes",
  "parent_thread": "main",
  "base_state": "hd-base123",
  "base_root": "tr-root123",
  "coordination_discussion_id": "discussion-123",
  "parent_task": null,
  "lanes": [
    {
      "thread": "feature/lane-d4",
      "path": "../lane-d4",
      "title": "Implement native fan-out UX",
      "task": null,
      "session_id": null,
      "status": "planned"
    }
  ],
  "commands": [
    {
      "lane_thread": "feature/lane-d4",
      "command": "heddle start feature/lane-d4 --path ../lane-d4 --task 'Implement native fan-out UX'",
      "argv": [
        "heddle",
        "start",
        "feature/lane-d4",
        "--path",
        "../lane-d4",
        "--task",
        "Implement native fan-out UX"
      ]
    }
  ],
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

## `heddle agent fanout start --output json`

```json
{
  "output_kind": "agent_fanout_start",
  "title": "Coordinate Wave 4 lanes",
  "parent_thread": "main",
  "base_state": "hd-base123",
  "base_root": "tr-root123",
  "coordination_discussion_id": "discussion-123",
  "parent_task": {
    "schema_version": 1,
    "task_id": "task-parent",
    "title": "Coordinate Wave 4 lanes",
    "body": "- feature/lane-d4 -> ../lane-d4: Implement native fan-out UX",
    "status": "in_progress",
    "target_thread": "main",
    "base_state": "hd-base123",
    "base_root": "tr-root123",
    "parent_task_id": null,
    "coordination_discussion_id": "discussion-123",
    "allow_offline": true,
    "delegated_by": "heddle agent fanout start",
    "created_at": "2026-06-30T12:00:00Z",
    "updated_at": "2026-06-30T12:00:00Z",
    "completed_at": null
  },
  "lanes": [
    {
      "thread": "feature/lane-d4",
      "path": "../lane-d4",
      "title": "Implement native fan-out UX",
      "task": {
        "schema_version": 1,
        "task_id": "task-child",
        "title": "Implement native fan-out UX",
        "body": "Fan-out child lane for parent task task-parent",
        "status": "in_progress",
        "target_thread": "feature/lane-d4",
        "base_state": "hd-base123",
        "base_root": "tr-root123",
        "parent_task_id": "task-parent",
        "coordination_discussion_id": "discussion-123",
        "allow_offline": true,
        "delegated_by": "task-parent",
        "created_at": "2026-06-30T12:00:00Z",
        "updated_at": "2026-06-30T12:00:00Z",
        "completed_at": null
      },
      "session_id": "agent-123",
      "status": "started"
    }
  ],
  "commands": [
    {
      "lane_thread": "feature/lane-d4",
      "command": "heddle start feature/lane-d4 --path ../lane-d4 --task 'Implement native fan-out UX'",
      "argv": [
        "heddle",
        "start",
        "feature/lane-d4",
        "--path",
        "../lane-d4",
        "--task",
        "Implement native fan-out UX"
      ]
    }
  ],
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

## `heddle auth logout --output json`

```json
{
  "output_kind": "auth_logout",
  "server": "grpc.heddle.sh",
  "removed": true,
  "device_identity_removed": true
}
```

---

## `heddle auth status --output json`

```json
{
  "output_kind": "auth_status",
  "server": "grpc.heddle.sh",
  "authenticated": true,
  "proof_key_available": true,
  "subject": "did:key:z6Mk...",
  "credential_id": "cred-123",
  "expires_at": "2026-06-27T00:00:00Z",
  "recommended_action": null
}
```

---

## `heddle auth create-service-token --output json`

```json
{
  "output_kind": "auth_create_service_token",
  "name": "github-ci-main",
  "namespace": "heddle/platform",
  "scope": "namespace:heddle/platform",
  "token": "heddle_sa_...",
  "private_key_pem": "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----\n",
  "expires_in_days": 30
}
```

---

## `heddle agent provenance begin --output json`

`heddle agent provenance begin|show|end --output json` emit:

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

## `heddle agent provenance segment --output json`

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

## `heddle agent provenance list --output json`

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

`heddle pull --output json` emits:

```json
{
  "output_kind": "pull",
  "action": "pull",
  "status": "updated",
  "pulled": true,
  "changed": true,
  "success": true,
  "transport": "heddle",
  "remote": "origin",
  "thread": "main",
  "state": "hs-head456",
  "objects": 12,
  "verification": {"verified":true,"status":"clean","repository_mode":"native-heddle","heddle_initialized":true,"git_branch":null,"heddle_thread":"main","worktree_dirty":false,"worktree_state":"clean","import_state":"not_applicable","mapping_state":"not_applicable","remote_drift":"clean","active_operation":null,"default_remote":"origin","clone_verification":"not_applicable","machine_contract":"available","workflow_status":"idle","workflow_summary":"No ready thread is waiting to merge","summary":"Repository is healthy","recommended_action":null,"recommended_action_template":null,"recovery_commands":[],"recovery_action_templates":[],"checks":[]}
}
```

`heddle push --output json` emits:

```json
{
  "output_kind": "push",
  "action": "push",
  "status": "pushed",
  "pushed": true,
  "changed": true,
  "success": true,
  "transport": "heddle",
  "remote": "origin",
  "push_scope": "current_thread",
  "force": false,
  "thread": "main",
  "state": "hs-head456",
  "objects": 12,
  "next_action": null,
  "next_action_template": null,
  "recommended_action": null,
  "recommended_action_template": null,
  "verification": {}
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `output_kind`, `action`, `status`, `success`, `cloned`, `transport`, `remote`, `local`, `branch`, `repository_capability` | mixed | required for successful `clone` | Stable clone envelope, transport, source, destination, checked-out branch, and initialized repository capability. |
| `objects`, `state` | int/string \| null | Heddle clone | Transferred object count and resulting Heddle state. |
| `commits_imported`, `states_created` | int | Git Overlay clone | Git commits streamed through Sley and Heddle states created during overlay initialization. |
| `remotes` | array<object> | required for `remote list` | Configured remotes. Empty if none. |
| `name`, `url`, `source`, `is_default` | string/string/string/bool | required for `remote show` and remote entries | Remote identity and default marker. |
| `pulled`, `pushed`, `success` | bool \| null | required when present | Transport result booleans. Pull reports `pulled`; push reports `pushed`. |
| `action`, `status`, `transport` | string | required for pull/push | Stable action name, outcome status, and authority transport (`git` or `heddle`). |
| `state`, `objects` | string/int \| null | pull/push | Resulting Heddle state and transferred object count. |
| `push_scope`, `thread` | string \| null | push | Whether the push published the current thread or all threads, and the named thread when applicable. |
| `force` | bool \| null | push | Whether Heddle-native ref protection was explicitly overridden. |
| `next_action`, `recommended_action`, `next_action_template`, `recommended_action_template` | mixed | required for push | Post-push action metadata promoted from verification; all are `null` when the push closes the remote loop. |
| `verification` | object | required for clone, remote mutations, pull, push, and commit | Post-operation repository verification proof. Observe-only `remote list` and `remote show` do not emit this field. |

---

## `heddle adopt --output json`

One-command Git adoption. Initializes Heddle sidecar data when needed,
imports the requested Git refs, atomically moves Repository Source Authority to
native Heddle, and returns the post-adoption verification proof. The existing
`.git` remains available through explicit Git Projection commands.

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
  "already_in_sync": false
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `adopted`, `initialized`, `already_in_sync` | bool | required | Adoption outcome, whether `.heddle/` was created, and whether import found no new states. A successful result has native Heddle source authority. |
| `path` | string | required | Path to the Heddle sidecar data. |
| `refs` | array<string> | required | Refs explicitly requested with `--ref`; empty means all local refs were imported. |
| `commits_imported`, `states_created`, `branches_synced`, `tags_synced` | int | required | Git import counts. |
| `skipped_non_commit_refs` | int | required | Non-commit Git refs skipped during import. |
| `verification` | object | required | Post-adoption repository verification proof. |

---

## `heddle status --output json`

Canonical surface for Git Overlay and native Heddle state. Plain Git first-run
flows recommend `heddle init`; Git Overlay reconciliation uses explicit
`heddle import git ...`; `heddle adopt` is the explicit authority transition.

`verification` is the public proof block. Legacy `git_overlay_import_hint`
and `git_overlay_health` sidecars are internal render data, not public JSON
contract fields.

### Sample

```json
{
  "output_kind": "status",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "verification": {
    "verified": false,
    "status": "needs_import",
    "import_state": "needs_import",
    "mapping_state": "needs_import",
    "summary": "1 Git branch tip(s) still need Heddle import",
    "checks": [],
    "recommended_action": "heddle import git --ref support/import-me",
    "recovery_commands": ["heddle import git --ref support/import-me"]
  },
  "recommended_action": "heddle import git --ref support/import-me",
  "recovery_commands": ["heddle import git --ref support/import-me"]
}
```

### Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `repository_capability` | string | required | Same vocabulary as `heddle status`. |
| `storage_model` | string | required | Same. |
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

`heddle log --timeline --output json` emits the current agent
tool-call navigation state:

```json
{
  "output_kind": "timeline_log",
  "status": "ok",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "thread": "main",
  "cursor": {
    "branch_id": "tlb-main",
    "step_id": "tls-2",
    "state": "hd-def456",
    "state_full": "hd-def4561234567890abcdef"
  },
  "branches": [
    {
      "branch_id": "tlb-main",
      "parent_branch_id": null,
      "forked_from_step_id": null,
      "forked_from_state": null,
      "reason": null,
      "created_at_ms": 1770000000000,
      "step_ids": ["tls-1", "tls-2"],
      "is_active": true,
      "is_on_active_path": true
    }
  ],
  "steps": [
    {
      "step_id": "tls-2",
      "branch_id": "tlb-main",
      "parent_step_id": "tls-1",
      "native": {
        "harness": "opencode",
        "session_id": "ses-123",
        "message_id": "msg-456",
        "tool_call_id": "call-789"
      },
      "tool_name": "edit",
      "status": "succeeded",
      "changed": true,
      "touched_paths": ["src/lib.rs"],
      "labels": ["repo-reversible"],
      "before_state": "hd-abc123",
      "after_state": "hd-def456",
      "capture_state": "hd-def456",
      "cursor_state": "hd-def456",
      "cursor_state_full": "hd-def4561234567890abcdef",
      "payload_summary": "edit src/lib.rs",
      "payload_hash": "hpb-abc123",
      "capture_oplog_batch_id": 42,
      "started_at_ms": 1770000000100,
      "finished_at_ms": 1770000000200,
      "operation_ids": ["hop-1"],
      "is_current": true,
      "is_on_active_branch_path": true,
      "can_seek": true,
      "can_fork": true,
      "can_reset": true,
      "can_materialize": true,
      "has_boundary_warning": false
    }
  ],
  "active_branch_path": ["tlb-main"],
  "actions": {
    "can_undo": true,
    "can_redo": false
  },
  "recovery": null
}
```

`heddle timeline status --output json` emits a scrubbed status envelope for
the selected timeline thread. It reports cursor and summary metadata only; it
does not include raw tool payloads, transcripts, stdout, stderr, environment
values, argv, or filename lists.

```json
{
  "output_kind": "timeline_status",
  "status": "ok",
  "thread": "main",
  "cursor_branch_id": "tlb-main",
  "cursor_step_id": "tls-1",
  "cursor_state": "hd-0123456789abcdefghijklmnop",
  "current_step": {
    "step_id": "tls-1",
    "branch_id": "tlb-main",
    "parent_step_id": null,
    "tool_name": "read",
    "tool_status": "succeeded",
    "changed": false,
    "payload_summary": "Read project metadata",
    "payload_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "labels": ["external-side-effects-unknown"],
    "started_at_ms": 1710000000000,
    "finished_at_ms": 1710000001000,
    "can_seek": true,
    "can_fork": true,
    "can_reset": true,
    "can_materialize": true,
    "has_boundary_warning": false
  },
  "active_branch_path": ["tlb-main"],
  "can_undo": false,
  "can_redo": false,
  "branch_count": 1,
  "step_count": 1,
  "recovery": null
}
```

`heddle timeline record-start --output json` emits the scrubbed append
result after appending a versioned tool-call-start operation body:

```json
{
  "output_kind": "timeline_record_start",
  "status": "ok",
  "action": "record-start",
  "thread": "main",
  "step_id": "tls-1",
  "branch_id": "tlb-main",
  "parent_step_id": null,
  "operation_id": "tl-0123456789abcdefghijklmnopqrstuv",
  "before_state": "hd-0123456789abcdefghijklmnop",
  "after_state": null,
  "changed": null,
  "tool_status": null,
  "payload_summary": "Read project metadata",
  "payload_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "branch_count": 1,
  "step_count": 1
}
```

`heddle timeline record-finish --output json` emits the scrubbed append
result after appending a versioned tool-call-finish operation body:

```json
{
  "output_kind": "timeline_record_finish",
  "status": "ok",
  "action": "record-finish",
  "thread": "main",
  "step_id": "tls-1",
  "branch_id": "tlb-main",
  "parent_step_id": null,
  "operation_id": "tl-1123456789abcdefghijklmnopqrstuv",
  "before_state": "hd-0123456789abcdefghijklmnop",
  "after_state": "hd-1123456789abcdefghijklmnop",
  "changed": true,
  "tool_status": "succeeded",
  "payload_summary": "Read project metadata",
  "payload_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "branch_count": 1,
  "step_count": 1
}
```

`heddle timeline fork|reset|recover --output json` emit timeline
action results:

```json
{
  "output_kind": "timeline_action",
  "status": "ok",
  "action": "reset",
  "thread": "main",
  "branch_id": "tlb-main",
  "parent_branch_id": null,
  "from_step_id": "tls-1",
  "cursor_branch_id": "tlb-main",
  "cursor_step_id": "tls-2",
  "operation_id": "top-1",
  "recovered_operation_id": null,
  "materialized": true,
  "materialization_status": "applied",
  "recovery_status": null,
  "blocker_count": 0,
  "branch_count": 1,
  "step_count": 2
}
```

---

## `heddle show <state> --output json`

State detail view, pretty-printed.

### Sample

```json
{
  "output_kind": "show",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "state_id": "hd-def456",
  "state_id_full": "hd-def4561234567890abcdef",
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
      "task_assignment_id": "task-parser-fast",
      "task_summary": {
        "task_id": "task-parser-fast",
        "title": "Tighten parser validation",
        "status": "in_progress",
        "target_thread": "feature/parser-fast",
        "updated_at": "2026-01-01T00:00:00Z",
        "completed_at": null,
        "coordination_discussion_id": null
      },
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
- `available_git_refs` contains Git refs that Heddle can import but has
  not yet modeled as active/imported threads.
- `repository_label` is the human-facing identity; `repository_context`
  is present when the command is run inside a managed child checkout.
- All `Option<...>` fields serialize as explicit `null`.
- `child_threads`, `sibling_threads`, `blockers`, `changed_paths`, and
  `impact_categories` are empty arrays — never omitted.
- `shared_target_dir` is `null` when the thread uses cargo's default
  per-checkout `target/` (was previously omitted).

---

## `heddle help --output json`

Public command catalog for agents, shell integrations, and generated docs.
Use `heddle help --output json` in automation. The catalog includes
native commands first and lower-level Git Projection actions only where
a command explicitly belongs to that surface.

Agents can bound the response before parsing it:

```bash
heddle help --output json
```

The catalog is intentionally complete. Agents that need a smaller working
set should filter the returned `commands` array by `display`, `tier`,
`mutates`, or `supports_op_id` after parsing the JSON.

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `executable_path` | string | required | Absolute path to the Heddle binary that produced this catalog when discoverable. Agent-facing `argv` values use this path so replay does not depend on `PATH` resolving the same binary. Falls back to `heddle` only when the executable cannot be resolved. |
| `commands` | array<object> | required | One entry per public command path. |
| `commands[].path` | array<string> | required | Command path tokens. |
| `commands[].display` | string | required | Joined command path. |
| `commands[].aliases` | array<string> | required | Alternate command spellings advertised by the command contract table. |
| `commands[].tier` | string | required | Derived discovery tier for broad filtering (`everyday`, `advanced`, or `hidden`). |
| `commands[].surface` | string | required | Product surface from the command contract table (`native`, `source_authority`, `git_projection`, `automation`, `admin`, or `internal`). `source_authority` commands dispatch through the repository's Git Overlay or Native Heddle adapter. |
| `commands[].help_visibility` | string | required | Human discovery placement from the command contract table (`everyday`, `advanced`, `git_projection`, or `hidden`). |
| `commands[].help_rank` | int | required | Stable ordering key for human command discovery. Lower ranks appear earlier. |
| `commands[].canonical_command` | string \| null | required | Canonical Heddle command for Git-shaped aliases; `null` for native commands. |
| `commands[].canonical_action` | object \| null | required | Structured canonical mapping for Git-shaped aliases. Contains `command`, `kind`, `executable`, `note`, `argv`, and `template`; `null` for native commands. `kind` is `direct_command`, `command_family`, `workflow`, or `conceptual_home`. |
| `commands[].command_action` | object \| null | required | Agent-facing invocation advertised by the command contract table. Executable commands carry `argv`; fillable placeholders carry `template`. Group-only commands use `null`. |
| `commands[].summary` | string | required | First help line. |
| `commands[].has_subcommands` | bool | required | Whether the command has public children. |
| `commands[].supports_json` | bool | required | Whether the command supports JSON output. |
| `commands[].output_modes` | array<string> | required | Exact output modes accepted by this command (`text`, optionally `json`, and optionally `json-compact`). Agents should inspect this field instead of probing commands and handling an unsupported-mode failure. |
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
| `commands[].options` | array<object> | required | Flags/options local to that command, including hidden advanced or plumbing flags marked with `hidden: true`. |
| `commands[].arguments` | array<object> | required | Public positional arguments local to that command. |
| `global_options` | array<object> | required | Global flags accepted across commands, including hidden globals marked with `hidden: true`. Conditional behavior such as `--op-id` support is still described by per-command fields. |
| `recommended_action_placeholders` | array<string> | required | Explicit display-only placeholders that cannot parse directly through Clap until the caller supplies the missing value. |
| `recommended_action_templates` | array<object> | required | Structured fillable forms for display-only recommended actions. Agents may fill templates only when `agent_may_fill` is true. When `agent_may_fill` is false, treat `action`/`argv_template` as display-only: do not substitute `<name>`/`<url>` placeholders; surface the template to a human or discard it. Substituting and running it will pass literal `<name>` to Heddle and fail. |

`command_action` is the per-command action contract. For example, `push`
advertises executable argv `["/path/to/heddle", "push"]`, while `adopt`
advertises the fillable template `["/path/to/heddle", "adopt", "--ref",
"<branch>"]` and `land` advertises `["/path/to/heddle", "land", "--thread",
"<thread>"]`.
Agents should execute `argv` directly and fill `template.argv_template`
only when they can supply every `required_inputs` value and
`agent_may_fill` is true.

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

`heddle help --output json` emits:

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
      "output_modes": ["text", "json", "json-compact"],
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
      "help": "Output format: text by default; json and json-compact provide explicit machine contracts",
      "required": false,
      "global": true
    }
  ],
  "recommended_action_placeholders": [
    "heddle capture -m \"...\"",
    "heddle commit -m \"...\"",
    "heddle ready -m \"...\"",
    "heddle remote add <name> <url>",
    "heddle clone <remote> <path>",
    "heddle clone <remote> <new-path>",
    "heddle clone <remote> <fresh-path>",
    "heddle thread switch <branch>",
    "heddle ready --thread <thread>",
    "heddle land --thread <thread>"
  ],
  "recommended_action_templates": [
    {
      "action": "heddle capture -m \"...\"",
      "argv_template": ["/path/to/heddle", "capture", "-m", "<message>"],
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
| `state_id` | string | required | Physical state identifier. |
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
  "state_id": "hd-def456",
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
{"output_kind": "review_sign", "signature_id": "...", "state_id": "..."}
```

`heddle review next --output json` emits a stable envelope keyed by
`output_kind: "review_next"`. When the scan window holds a pending
review, the pending state's view is flattened alongside `output_kind`
(`state_id`, `headline`, `existing_signatures`) and the same view is
echoed under `next`. When the scan window holds no pending review, the
payload carries only `output_kind` and `next: null` — never a
top-level `null`.

```json
{"output_kind": "review_next", "state_id": "hd-def456", "headline": "Tighten parser recovery", "existing_signatures": 0, "next": {"state_id": "hd-def456", "headline": "Tighten parser recovery", "existing_signatures": 0}}
```

`heddle review health --output json` emits:

```json
{"output_kind": "review_health", "entries": [{"module_id": "...", "fire_rate": 0.42, "warn": false}], "window_states": 12}
```

## `heddle integration relay --output json`

Hidden integration relay output is registered as a generic object payload.

```json
{"status": "ok"}
```

---

## `heddle maintenance inspect --output json`

Maintenance inspection reports the repository's rebuildable performance
sidecars and the repository shape they summarize. It does not change repository
meaning.

```json
{
  "output_kind": "maintenance_inspect",
  "commit_graph": {"present": true, "node_count": 3, "bloom_covered_nodes": 3, "bytes": 512, "error": null},
  "worktree_index": {"present": true, "file_entries": 12, "directory_entries": 4, "untracked_directory_entries": 1, "snapshot_bytes": 1024, "journal_bytes": 128, "journal_ops": 3, "journal_replay_ms": 0, "error": null},
  "change_monitor": {"backend": "native", "status": "ready", "reason": null, "changed_path_count": 0},
  "refs": {"total": 2, "threads": 1, "markers": 0, "remotes": 1, "remote_threads": 0, "packed_refs_present": false, "packed_refs_bytes": 0},
  "ref_summary_index": {"present": true, "valid": true, "bytes": 256, "threads": 1, "markers": 0, "remotes": 1, "remote_threads": 0, "packed_threads": 0, "packed_markers": 0, "error": null},
  "pack_files": {"pack_count": 1, "index_count": 1, "unpaired_pack_count": 0, "pending_install_intents": 0},
  "partial_fetch": {"count": 0, "missing_blob_count": 0},
  "pull_planner_cache": {"status": "ready", "present": true, "manifest_count": 1, "planner_entry_count": 3, "total_bytes": 384}
}
```

### Maintenance Inspect Fields

| Field | Type | Semantics |
|-------|------|-----------|
| `output_kind` | string | Always `maintenance_inspect`. |
| `commit_graph` | object | Presence, size, node coverage, and read error for the commit-graph sidecar. |
| `worktree_index` | object | Snapshot and journal shape for the worktree index sidecar. |
| `change_monitor` | object | Active backend, readiness, reason, and pending changed-path count. |
| `refs` | object | Live ref counts and packed-refs storage facts. |
| `ref_summary_index` | object | Validity, size, and category counts for the ref summary sidecar. |
| `pack_files` | object | Pack/index pairing and pending install-intent counts. |
| `partial_fetch` | object | Missing-object markers, including the missing blob count. |
| `pull_planner_cache` | object | Cache readiness, manifest/entry counts, and storage size. |

## `heddle maintenance refresh --output json`

Maintenance refresh rebuilds performance sidecars and reports both the work
performed and the resulting inspection. It may rewrite derived metadata and
prune incomplete pack-install artifacts, but does not change repository meaning.

```json
{
  "output_kind": "maintenance_refresh",
  "rebuilt_commit_graph": true,
  "rebuilt_ref_summary_index": true,
  "rebuilt_worktree_index": true,
  "refreshed_change_monitor": true,
  "rebuilt_pull_planner_cache": true,
  "pruned_pull_planner_entries": 0,
  "pack_install_intents_recovered_completed": 0,
  "pack_install_intents_aborted": 0,
  "pack_install_intents_skipped_in_progress": 0,
  "pack_install_intents_quarantined": 0,
  "pack_install_metrics": {"installs_ok": 0, "installs_err": 0, "recover_completed": 0, "recover_aborted": 0, "recover_skipped_in_progress": 0, "recover_quarantined": 0},
  "unpaired_packs_pruned": 0,
  "unpaired_pack_bytes_freed": 0,
  "report": {
    "commit_graph": {"present": true, "node_count": 3, "bloom_covered_nodes": 3, "bytes": 512, "error": null},
    "worktree_index": {"present": true, "file_entries": 12, "directory_entries": 4, "untracked_directory_entries": 1, "snapshot_bytes": 1024, "journal_bytes": 128, "journal_ops": 3, "journal_replay_ms": 0, "error": null},
    "change_monitor": {"backend": "native", "status": "ready", "reason": null, "changed_path_count": 0},
    "refs": {"total": 2, "threads": 1, "markers": 0, "remotes": 1, "remote_threads": 0, "packed_refs_present": false, "packed_refs_bytes": 0},
    "ref_summary_index": {"present": true, "valid": true, "bytes": 256, "threads": 1, "markers": 0, "remotes": 1, "remote_threads": 0, "packed_threads": 0, "packed_markers": 0, "error": null},
    "pack_files": {"pack_count": 1, "index_count": 1, "unpaired_pack_count": 0, "pending_install_intents": 0},
    "partial_fetch": {"count": 0, "missing_blob_count": 0},
    "pull_planner_cache": {"status": "ready", "present": true, "manifest_count": 1, "planner_entry_count": 3, "total_bytes": 384}
  }
}
```

### Maintenance Refresh Fields

| Field | Type | Semantics |
|-------|------|-----------|
| `output_kind` | string | Always `maintenance_refresh`. |
| `rebuilt_*`, `refreshed_*` | boolean | Whether refresh rebuilt each derived sidecar. |
| `pruned_pull_planner_entries` | integer | Stale pull-planner entries removed. |
| `pack_install_intents_*` | integer | Recovery disposition counts for durable pack-install intents. |
| `pack_install_metrics` | object | Process-local install and recovery counters after refresh. |
| `unpaired_packs_pruned` | integer | Unpaired pack files removed. |
| `unpaired_pack_bytes_freed` | integer | Bytes recovered by pruning unpaired packs. |
| `report` | object | The resulting maintenance-inspect report, without a second discriminator. |

## `heddle export git --output json`

Export emits:

```json
{"output_kind": "export_git", "states_exported": 3, "commits_total": 3, "threads_synced": 1, "markers_synced": 2, "branches": [{"name": "main", "tip": "0123456789abcdef0123456789abcdef01234567"}], "tags": [{"name": "v1.0.0", "tip": "89abcdef0123456789abcdef0123456789abcdef"}], "destination": "/work/project.git"}
```

Export requires an explicit destination and does not default to `.heddle/git`.

## `heddle import git --output json`

Import emits:

```json
{"output_kind": "import_git", "status": "completed", "action": "import git", "summary": "Imported Git history from /work/project; repository verification is clean", "commits_imported": 4, "states_created": 4, "branches_synced": 2, "tags_synced": 1, "skipped_non_commit_refs": 0, "lossy_entries": [], "already_in_sync": false, "recommended_action": null, "recommended_action_template": null, "recovery_commands": []}
```

### Import Git Fields

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `lossy_entries` | array<object> | required | Entries dropped or converted only when `--lossy` was explicitly passed; empty for lossless imports. |

---

## Git Projection import/export/sync JSON

Explicit Git Projection ops emit JSON via `serde_json::json!{}` with consistent
key naming:

| Verb | Shape |
|------|-------|
| `export` | `{"output_kind": "export_git", "states_exported": N, "threads_synced": N, "markers_synced": N, "destination": "..."}` |
| `import` | `{"output_kind": "import_git", "commits_imported": N, "states_created": N, "branches_synced": N, "tags_synced": N, "skipped_non_commit_refs": N, "lossy_entries": [], "already_in_sync": false}` |
| `sync` | `{"output_kind": "sync_git", "states_exported": N, "commits_imported": N, "threads_synced": N, "markers_synced": N}` |

## `heddle export git --output json`

Export emits:

```json
{"output_kind": "export_git", "states_exported": 3, "threads_synced": 1, "markers_synced": 2, "destination": "/work/project.git"}
```

Export requires an explicit destination and does not default to `.heddle/git`.

## `heddle import git --output json`

Import emits:

```json
{"output_kind": "import_git", "commits_imported": 4, "states_created": 4, "branches_synced": 2, "tags_synced": 1, "skipped_non_commit_refs": 0, "lossy_entries": [], "already_in_sync": false}
```

## `heddle sync git --output json`

Sync emits:

```json
{"output_kind": "sync_git", "states_exported": 3, "commits_imported": 4, "threads_synced": 1, "markers_synced": 2}
```

### Import Git Fields

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
verification report and the primary recovery command. Public proof lives in
`verification`; legacy Git-overlay health/import sidecars are internal render
data, not JSON contract fields.

```json
{
  "output_kind": "doctor",
  "repository": "/work/project",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "hosted_enabled": false,
  "verification": {"verified": true, "status": "clean", "checks": [], "recommended_action": "", "recovery_commands": []},
  "operation": null,
  "remote_tracking": null,
  "thread": null,
  "state": null,
  "changes": {"modified": [], "added": [], "deleted": []},
  "workspace": {"thread_count": 0},
  "health": {"output_kind": "doctor_health", "status": "clean"},
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

The command-contract coverage portion of this sample is generated from
runtime facts. Refresh it with `heddle doctor schemas --update-docs`.

```json
{
  "command_contract_schema_coverage": {
    "accepted_opaque_schema_examples": [
      "help",
      "redact apply",
      "redact list",
      "redact show",
      "redact trust add",
      "redact trust list",
      "redact trust remove",
      "redact purge apply"
    ],
    "accepted_opaque_schema_verbs_total": 39,
    "advanced_scope": "advanced_internal_admin",
    "advanced_scope_accepted_opaque_schema_examples": [
      "help",
      "redact apply",
      "redact list",
      "redact show",
      "redact trust add",
      "redact trust list",
      "redact trust remove",
      "redact purge apply"
    ],
    "advanced_scope_json_commands_total": 105,
    "advanced_scope_json_commands_with_accepted_opaque_schema": 39,
    "advanced_scope_mutating_commands_total": 61,
    "advanced_scope_mutating_commands_with_accepted_opaque_schema": 22,
    "catalog_commands_total": 183,
    "catalog_mutating_commands_total": 89,
    "json_commands_total": 148,
    "json_commands_with_accepted_opaque_schema": 39,
    "json_commands_with_schema": 109,
    "json_commands_without_schema": 0,
    "json_mutating_commands_total": 87,
    "missing_mutating_schema_examples": [],
    "missing_schema_examples": [],
    "mutating_commands_total": 87,
    "mutating_commands_with_accepted_opaque_schema": 22,
    "mutating_commands_with_schema": 65,
    "mutating_commands_without_schema": 0,
    "opaque_schema_verbs_total": 39,
    "status": "available",
    "summary": "183 command(s), 148 JSON command(s), 89 mutating command(s), 87 mutating JSON command(s); verified everyday/agent machine surface has 43 concrete schema-backed JSON command(s); advanced/internal/admin surfaces carry 39 accepted opaque schema(s) outside clean verification",
    "unaccepted_opaque_schema_examples": [],
    "unaccepted_opaque_schema_verbs_total": 0,
    "undocumented_schema_examples": [],
    "undocumented_schema_verbs_total": 0,
    "verified_scope": "everyday_and_agent",
    "verified_scope_accepted_opaque_schema_examples": [],
    "verified_scope_json_commands_total": 43,
    "verified_scope_json_commands_with_accepted_opaque_schema": 0,
    "verified_scope_json_commands_with_schema": 43,
    "verified_scope_json_commands_without_schema": 0,
    "verified_scope_missing_schema_examples": [],
    "verified_scope_mutating_commands_total": 26,
    "verified_scope_mutating_commands_with_accepted_opaque_schema": 0,
    "verified_scope_mutating_commands_with_schema": 26,
    "verified_scope_mutating_commands_without_schema": 0
  },
  "doc_path": "/repo/docs/json-schemas.md",
  "documented_verbs": [
    "status",
    "verify",
    "try"
  ],
  "issues": [],
  "output_kind": "doctor_schemas",
  "passing_verbs": [
    "status",
    "verify",
    "try"
  ],
  "recommended_action": null,
  "recovery_commands": [],
  "registered_verbs": [
    "status",
    "verify",
    "try"
  ],
  "status": "available",
  "summary": "183 command(s), 148 JSON command(s), 89 mutating command(s), 87 mutating JSON command(s); verified everyday/agent machine surface has 43 concrete schema-backed JSON command(s); advanced/internal/admin surfaces carry 39 accepted opaque schema(s) outside clean verification",
  "undocumented_verbs": [],
  "unmatched_verbs": [],
  "verified": true
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
  "state_id": "hd-sqr398dvx9ay",
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

## `heddle fsck repair git --output json`

Preview or apply the repair direction allowed by durable source authority.
Git Overlay repairs Git into Heddle metadata; native repositories repair a
named Heddle ref into the retained Git projection. Supplying the opposite
`--prefer` side is a typed refusal and changes nothing.

```json
{
  "valid": true,
  "errors": [],
  "warnings": [],
  "objects_checked": 42,
  "git_projection_checked": true,
  "repair_target": "git",
  "repaired": false,
  "repairs": [
    {
      "name": "git_projection_ref_reconcile_preview",
      "repaired": false,
      "detail": "heddle fsck repair git --prefer git --ref main",
      "count": 0
    }
  ]
}
```

## `heddle revert --output json`

Apply the inverse of a state. With `--no-commit`, `state_id` is
`null`; otherwise it is the new revert state.

```json
{
  "output_kind": "revert",
  "state_id": null,
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

`heddle query --attribution <path> --output json` emits structured attribution that mirrors
`log` / `show`: each line (and each entry in `origins`) carries a
`principal` object (`name`, `email`) and an `agent` field that is either
a structured object (`provider`, `model`, optional `session_id` /
`policy_id`) or `null` for human-only changes — no string-parsing
required:

```json
{"output_kind": "query_attribution", "status": "completed", "file": "src/lib.rs", "lines": [{"line_number": 1, "content": "pub fn run() {}", "change_id": "hd-sqr398dvx9ay", "principal": {"name": "A. Engineer", "email": "a@example.com"}, "agent": {"provider": "anthropic", "model": "claude-opus-4-7"}, "timestamp": "2026-01-01T00:00:00Z", "origins": [{"change_id": "hd-sqr398dvx9ay", "principal": {"name": "A. Engineer", "email": "a@example.com"}, "agent": {"provider": "anthropic", "model": "claude-opus-4-7"}, "timestamp": "2026-01-01T00:00:00Z"}]}]}
```

`heddle context reason git --output json` emits:

```json
{"commits_scanned":2,"commits_with_matches":1,"sessions_mined":3,"points_extracted":4,"states_updated":1,"annotations_written":4}
```

`heddle collapse --output json` emits:

```json
{"change_id": "hd-collapsed123", "collapsed": 3, "message": "collapse feature checkpoints", "parents": ["hd-base123"]}
```

`heddle expand --output json` emits the ordered captures recorded by a squashed land collapse:

```json
{"output_kind": "expand", "status": "completed", "requested": "HEAD", "collapsed": {"change_id": "hd-collapsed123", "change_id_full": "hd-collapsed123000000000000000000", "git_commit": "abc123def456abc123def456abc123def456abcd", "thread": "feature/parser", "source_count": 2}, "captures": [{"change_id": "hd-source111", "change_id_full": "hd-source1110000000000000000000", "content_hash": "h1-source", "intent": "first parser checkpoint", "principal": "A. Engineer <a@example.com>", "agent": null, "confidence": null, "created_at": "2026-01-01 00:00:00", "parents": ["hd-base123"]}, {"change_id": "hd-source222", "change_id_full": "hd-source2220000000000000000000", "content_hash": "h1-source2", "intent": "second parser checkpoint", "principal": "A. Engineer <a@example.com>", "agent": "codex/gpt-5", "confidence": 0.91, "created_at": "2026-01-01 00:05:00", "parents": ["hd-source111"]}]}
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

`heddle discuss open|append|resolve|reopen --output json` emit a write
outcome and the resulting materialized discussion. The operation id is the
durable collaboration-log address; `disposition` is `created`,
`existing_operation`, or `idempotent_replay`:

```json
{"output_kind":"discuss_open","operation_id":"co-01abc","disposition":"created","discussion":{"id":"disc-018f47ea-4a54-7c89-b012-3456789abcde","title":"Please check this edge case.","anchor":{"kind":"symbol","state_id":"hs-01abc","path":"src/lib.rs","symbol":"verify"},"visibility":"team:platform","status":"open","resolution":null,"conflict_operation_ids":[],"head_operation_ids":["co-01abc"],"display_head_operation_id":"co-01abc","turns":[{"operation_id":"co-01abc","author_name":"A. Engineer","author_email":"a@example.com","agent":null,"occurred_at_ms":1767225600000,"body":"Please check this edge case.","content_hash":"0123456789abcdef"}]}}
```

## `heddle discuss show --output json`

`heddle discuss show --output json` uses the same discussion object beneath
the `discuss_show` discriminator:

```json
{"output_kind":"discuss_show","discussion":{"id":"disc-018f47ea-4a54-7c89-b012-3456789abcde","title":"Please check this edge case.","anchor":{"kind":"symbol","state_id":"hs-01abc","change_id":null,"path":"src/lib.rs","symbol":"verify"},"visibility":"team:platform","status":"open","resolution":null,"conflict_operation_ids":[],"head_operation_ids":["co-01abc"],"display_head_operation_id":"co-01abc","turns":[]}}
```

## `heddle discuss list --output json`

`heddle discuss list --output json` emits:

```json
{"output_kind":"discuss_list","discussions":[]}
```

`heddle fsck --output json` emits:

```json
{"valid": true, "errors": [], "warnings": [], "objects_checked": 42, "git_projection_checked": false, "repair_target": null, "repaired": false, "repairs": []}
```

`heddle oplog recover --output json` emits an operator recovery report
(`output_kind` is `oplog_recover`; `strategy` is `footer-guided` or
`forward-greedy`; `prior_recovery` is `true` when the everyday read path's
auto-fallback already salvaged the oplog, in which case the detail is read
back from the `.oplog.recovery` sidecar and `quarantine_path` is omitted;
`quarantine_path` is present only when this command performed the salvage
itself):

```json
{"output_kind": "oplog_recover", "already_healthy": true, "prior_recovery": true, "strategy": "forward-greedy", "entries_recovered": 3, "entries_lost": 1, "damaged_byte_start": 412, "damaged_byte_end": 690, "sidecar_path": "/work/project/.heddle/oplog/oplog.bin.oplog.recovery"}
```

`heddle hook list|install|uninstall|events --output json` emit:

```json
{"hooks": [{"event": "pre-capture", "command": "cargo test"}], "installed": true, "uninstalled": false, "events": [{"name": "pre-capture", "description": "before capture"}]}
```

`heddle integration list|doctor --output json` emit:

```json
[
  {
    "harness": "opencode",
    "scope": "repo",
    "method": "hooks",
    "status": "installed",
    "healthy": true,
    "paths": [".opencode/plugin/heddle.ts"],
    "capabilities": ["timeline"],
    "capability_paths": [".opencode/plugin/heddle.timeline.json"],
    "path_mode": "repo-relative"
  }
]
```

`heddle integration install|uninstall|upgrade --output json` emit:

```json
{"integrations": [{"name": "github", "installed": true, "version": "1"}], "installed": true, "uninstalled": false, "upgraded": false, "issues": []}
```

`heddle maintenance gc --output json` emits the pack/prune report (counts
are zero on a fresh repository; `pinned_redactions` / `preserved_redactions`
report redacted blobs the collector refused to touch; `consolidated_mirror_loose`
counts loose legacy Bridge Mirror objects packed into the mirror's own pack):

```json
{"output_kind": "gc", "action": "gc", "status": "ok", "dry_run": false, "prune": false, "packed_count": 1, "bytes_saved": 0, "pruned_loose": 0, "bytes_freed": 0, "unpaired_packs_pruned": 0, "pack_install_intents_completed": 0, "pack_install_intents_aborted": 0, "pack_install_metrics": {"installs_ok": 0, "installs_err": 0, "recover_completed": 0, "recover_aborted": 0, "recover_skipped_in_progress": 0, "recover_quarantined": 0}, "pinned_redactions": 0, "preserved_redactions": 0, "pruned_git_mapping_entries": 0, "consolidated_mirror_loose": 0}
```

`heddle redact purge apply|list --output json` emit (each carries `output_kind`
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

`heddle query --attribution --output json` emits structured attribution
for a tracked file:

```json
{"output_kind": "query_attribution", "status": "completed", "file": "src/lib.rs", "lines": [{"line_number": 1, "content": "pub fn run() {}", "change_id": "hd-sqr398dvx9ay", "principal": {"name": "A. Engineer", "email": "a@example.com"}, "agent": null, "timestamp": "2026-01-01T00:00:00Z", "origins": null}]}
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
{"output_kind": "resolve", "message": "Resolved src/lib.rs; completed integration", "resolved": ["src/lib.rs"], "remaining": [], "continued": true, "continuation_status": "continued", "continuation_message": "Completed the in-progress Heddle integration", "next_action": "heddle land --thread feature/auth", "recommended_action": "heddle land --thread feature/auth"}
```

`heddle retro --output json` emits the same shape with bounded session data;
`timeline_steps` is `[]` unless expanded with `--full`. `heddle retro
--full --output json` emits expanded timeline summaries:

```json
{"since": "hd-base123", "until": "hd-head456", "duration_secs": 3600, "states_captured": [{"change_id": "hd-head456", "intent": "capture parser fix", "confidence": 0.91, "agent": "codex/gpt-5", "principal": "A. Engineer <a@example.com>", "timestamp": "2026-01-01T00:00:00Z"}], "agents_active": [{"session_id": "session-123", "provider": "codex", "model": "gpt-5", "status": "active", "started_at": "2026-01-01T00:00:00Z", "completed_at": null, "tokens": {"input": 1200, "output": 800, "reasoning": 300, "tool_calls": 12}}], "agent_tasks": [{"task_id": "task-parser-fast", "title": "Tighten parser validation", "status": "in_progress", "target_thread": "feature/parser-fast", "updated_at": "2026-01-01T00:00:00Z", "completed_at": null, "coordination_discussion_id": null}], "timeline_steps": [{"thread": "feature/parser-fast", "step_id": "tls-1", "branch_id": "tlb-main", "parent_step_id": null, "tool_name": "edit", "tool_status": "succeeded", "changed": true, "payload_summary": "Edit parser validation", "payload_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "before_state": "hd-base123", "after_state": "hd-head456", "capture_state": "hd-head456", "started_at_ms": 1770000000000, "finished_at_ms": 1770000001000}], "markers_created": [{"name": "verified-parser", "state": "hd-head456", "timestamp": "2026-01-01T00:00:00Z"}], "context_annotations": [{"path": "src/lib.rs", "scope": "file", "kind": "rationale", "content_excerpt": "Parser accepts the new token form.", "attribution": "A. Engineer <a@example.com>", "created_at": "2026-01-01T00:00:00Z"}], "verify_signals": [{"kind": "test_passed", "label": "verified: cargo test", "timestamp": "2026-01-01T00:00:00Z"}], "merges": [{"description": "Collapsed feature/parser", "timestamp": "2026-01-01T00:00:00Z"}], "undos": [{"description": "Undo capture", "timestamp": "2026-01-01T00:00:00Z"}]}
```

`heddle semantic hot --output json` emits:

```json
{"hotspots": [{"path": "src/lib.rs", "score": 0.87, "reasons": ["changed often"]}]}
```

## Other verbs

The following verbs also emit `--output json`. Their shapes follow the same
discipline; see the corresponding handler in `crates/cli/src/cli/commands/`:

`heddle clone`, `heddle collapse`,
`heddle context get/set`, `heddle diff`, `heddle expand`,
`heddle discuss`, `heddle doctor docs`,
`heddle fsck`, `heddle init`, `heddle integration`,
`heddle maintenance`, `heddle ready`,
`heddle remote`, `heddle resolve`, `heddle retro`,
`heddle agent provenance`, `heddle capture`,
`heddle thread show/start`,
`heddle try`, `heddle undo`, `heddle watch`.

Each of these:

- Emits a single JSON document on `--output json` (or one document per line for streaming verbs like `watch`).
- Uses `state_id` for immutable State identity and `change_id` for logical change lineage.
- Uses `created_at` (not `timestamp` or `recorded_at`) for state-creation timestamps.
- Serializes `Option<...>` semantic fields as explicit `null`.
- Serializes empty collections as `[]` / `{}`.
- Does not carry retired `git_overlay_import_hint` sidecars or raw
  `missing_branches` payloads; import guidance, when present, is exposed
  through current command-specific fields.

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
| `primary_command_template` | object \| null | required | Fillable template (`argv_template`/`required_inputs`/`agent_may_fill`) for `primary_command`. When `agent_may_fill` is false, treat `action`/`argv_template` as display-only: do not substitute `<name>`/`<url>` placeholders; surface the template to a human or discard it. Substituting and running it will pass literal `<name>` to Heddle and fail. The canonical machine-readable shape; the always-null `_argv` sidecar was dropped (HeddleCo/heddle#254). |
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
| `no_merge_in_progress` | A merge continue/resolve/abort-style command was requested when no merge operation is active. |
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
