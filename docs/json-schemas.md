# Heddle JSON output schemas

This document is the canonical reference for every CLI verb that emits
machine-readable output. Every entry below pairs a literal sample
output with a field-by-field table — the contract callers can rely
on.

## Runtime introspection

Schemas in this document are mirrored at runtime by
`crates/cli/src/cli/commands/schemas.rs`. Generate the canonical JSON
Schema for any verb with:

    heddle schemas <verb>             # e.g. heddle schemas status
    heddle schemas log --reflog       # subcommands taking --flags work too
    heddle schemas bridge git status

(Indented as plain text rather than a fenced block so the
`heddle doctor docs` flag-checker doesn't flag `--reflog` as
unknown — the schemas verb takes its argument as
`trailing_var_arg`.)

CI runs `heddle doctor schemas` on every PR and validates each literal
JSON sample below against the registered schema. Drift — a sample
field the schema doesn't declare, or vice versa — exits non-zero so
this doc cannot silently fall behind the implementation. Pair with
`heddle doctor docs` (which covers flag-level drift) for full doc
coverage.

## Discipline

Every `--json` output in Heddle's CLI follows the same rules. These
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
   lives only in `heddle bridge git status --json` (and the
   comprehensive `heddle diagnose --json`). Per-command outputs do not
   carry it. Transports do not silently piggy-back state.
4. **Empty collections serialize as `[]` / `{}`, not omitted.** An
   empty `blockers: []` is more useful than a missing field, and the
   discipline prevents tooling from writing brittle "key exists?"
   guards.
5. **Pretty printing is reserved for `heddle show`.** Every other verb
   emits compact, single-line JSON suitable for line-oriented streaming
   (one document per line for `heddle watch`, etc.).

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

* **Full change ID** — the 32-character form printed by `show --json`'s
  `change_id_full`, e.g. `hd-sqr398dvx9ayt9bf8bf5gz0jg8`.
* **Short change ID** — the 12-character prefix printed by every other
  `--json` verb's `change_id` field, e.g. `hd-sqr398dvx9ay`. Any
  unambiguous prefix of length 4 or more works; ambiguous prefixes
  yield an `ambiguous state ID prefix '<X>' matches: <list>` error.
* **Marker name** — anything created by `heddle marker create <name>`,
  e.g. `failed-build-2026-05-09`.
* **`HEAD`, `@`, `HEAD~N`, `@~N`** — relative walks from the active
  thread's tip.
* **Thread name** — resolves to that thread's tip.

Verbs covered: `show`, `diff`, `compare`, `revert`, `cherry-pick`,
`goto`, `bisect`, `blame --state`, `log --since`, `review show`,
`review sign`, `discuss open|list|resolve --state`, `retro --since`.
The `heddle log --json` `change_id` field is the canonical short form
that downstream verbs consume.

---

## `heddle status --json`

Snapshot of the repository's current thread, worktree state, and any
in-progress operation.

### Sample

```json
{
  "repository_capability": "git-overlay",
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
  "trust": {
    "trusted": true,
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
  "thread": "feature/parser-fast",
  "base_state": "hd-abc123",
  "base_root": "hd-abc123",
  "current_state": "hd-def456",
  "path": "/repo",
  "execution_path": "/repo",
  "session_id": null,
  "heddle_session_id": null,
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
| `repository_capability` | string | required | `"git-overlay"` or `"native"`. |
| `storage_model` | string | required | E.g. `"git+heddle-sidecar"`. |
| `hosted_enabled` | bool | required | Whether the repo is connected to a hosted server. |
| `operation` | object \| null | required | In-progress operation (`merge`, `rebase`, …) or `null`. |
| `remote_tracking` | object \| null | required | Remote drift summary or `null`. |
| `git_overlay_health` | object | required | Compatibility health view derived from the shared trust checks. |
| `trust` | object | required | Full `RepositoryTrustState`; status next actions defer to this when trust is blocked. |
| `thread` | string \| null | required | Current thread name; `null` for detached HEAD. |
| `base_state`, `base_root` | string \| null | required | Thread base anchor change-ids. |
| `current_state` | string \| null | required | Thread tip change-id. |
| `path` | string \| null | required | Materialized worktree path. |
| `execution_path` | string \| null | required | Effective execution root. |
| `actor` | object \| null | required | `{provider, model}`. `null` when no agent is attached. |
| `thread_mode` | enum \| null | required | `lightweight` / `materialized` / `virtualized`. |
| `thread_state` | enum \| null | required | `active` / `ready` / `merged` / `abandoned`. |
| `freshness` | enum \| null | required | `current` / `stale` / `unknown`. |
| `child_threads` | array<string> | required | Names; empty array if none. |
| `impact_categories` | array<enum> | required | Empty array if none. |
| `heavy_impact_paths` | array<string> | required | Empty array if none. |
| `blockers` | array<string> | required | Human-readable blockers; empty array if clean. |
| `recommended_action` | string | required | Primary next command; trust blockers take precedence. |
| `recovery_commands` | array<string> | required | Recovery commands from `trust`; empty when trusted. |
| `coordination_status` | enum | required | `clean` / `ahead` / `diverged` / `blocked` / `merge-ready`. |
| `parallel_threads` | array<object> | required | Empty array if none. |
| `state` | object \| null | required | Current state summary. |
| `git_checkpoint` | object \| null | required | Latest git checkpoint, when configured. |
| `changes` | object | required | Worktree status: `{modified: [], added: [], deleted: []}`. |

**Note:** Bridge import-hint information is not part of this output.
Use `heddle bridge git status --json`.

---

## `heddle trust --json`

Concise proof that Git, Heddle, mapping, worktree, remotes, operations, clone checks, and machine contracts agree.

### Sample

```json
{
  "trusted": true,
  "status": "clean",
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
  "recommended_action": "",
  "recovery_commands": [],
  "trust": {
    "trusted": true,
    "status": "clean",
    "repository_mode": "git-overlay",
    "heddle_initialized": true,
    "git_branch": "main",
    "heddle_thread": "main",
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
    ]
  }
}
```

---

## `heddle bridge git status --json`

Canonical surface for the Git-overlay bridge state. This is the only
command whose JSON output carries `git_overlay_import_hint`.

### Sample

```json
{
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
  "trust": {
    "trusted": false,
    "status": "needs_import",
    "repository_mode": "git-overlay",
    "heddle_initialized": true,
    "git_branch": "main",
    "heddle_thread": "main",
    "worktree_dirty": false,
    "import_state": "needs_import",
    "mapping_state": "clean",
    "remote_drift": "clean",
    "active_operation": null,
    "default_remote": null,
    "clone_verification": "not_applicable",
    "machine_contract": "available",
    "summary": "1 Git branch tip(s) still need Heddle import",
    "recommended_action": "heddle bridge git import --ref support/import-me",
    "recovery_commands": ["heddle bridge git import --ref support/import-me"],
    "checks": [
      {
        "name": "Mapping",
        "status": "needs_import",
        "clean": false,
        "summary": "1 Git branch tip(s) still need Heddle import",
        "recommended_action": "heddle bridge git import --ref support/import-me",
        "recovery_commands": ["heddle bridge git import --ref support/import-me"],
        "details": {}
      }
    ]
  }
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
| `git_overlay_health` | object | required | Legacy-compatible health summary derived from the shared trust engine. |
| `trust` | object | required | Full `RepositoryTrustState` proof payload shared with `heddle trust`. |

---

## `heddle log --json`

State history walking from a given starting state.

### Sample

```json
{
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

`heddle log --reflog --json` emits a different shape:

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

## `heddle show <state> --json`

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
  "agent": {"provider": "anthropic", "model": "claude-opus-4-7", "session_id": null, "policy_id": null},
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
| `tree` | string | required | Hex tree hash (alias of `content_hash`, kept for compatibility). |
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

## `heddle marker list --json`

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

## `heddle thread list --json`

```json
{
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
  "current": "feature/parser-fast",
  "trust": {
    "trusted": true,
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
- All `Option<...>` fields serialize as explicit `null`.
- `child_threads`, `sibling_threads`, `blockers`, `changed_paths`, and
  `impact_categories` are empty arrays — never omitted.
- `shared_target_dir` is `null` when the thread uses cargo's default
  per-checkout `target/` (was previously omitted).

---

## `heddle workspace show --json`

Control-tower view across every active thread.

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `repository`, `repository_capability`, `storage_model`, `hosted_enabled` | scalars | required | |
| `operation` | object \| null | required | |
| `remote_tracking` | object \| null | required | |
| `trust` | object | required | Full `RepositoryTrustState`; top-level workspace recommendations defer to this when trust is blocked. |
| `recommended_action` | string | required | Empty string when no action. |
| `current_thread` | string \| null | required | |
| `groups` | array<object> | required | One per non-empty bucket; can be empty. |
| `groups[].id` | enum string | required | `current` / `stacked` / `parallel` / `ready` / `blocked` / `recent`. |
| `groups[].label` | string | required | Human label. |
| `groups[].threads` | array<ThreadSummary> | required | At least one element per emitted group. |
| `thread_count` | int | required | |

```json
{
  "repository": "/work/project",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "hosted_enabled": false,
  "operation": null,
  "remote_tracking": null,
  "trust": {
    "trusted": true,
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
  "recommended_action": "heddle capture",
  "current_thread": "feature/parser-fast",
  "groups": [
    {
      "id": "current",
      "label": "Current",
      "threads": []
    }
  ],
  "thread_count": 1
}
```

---

## `heddle commands --json`

Public command catalog for agents, shell integrations, and generated docs.

| Field | Type | Optionality | Semantics |
|-------|------|-------------|-----------|
| `commands` | array<object> | required | One entry per public command path. |
| `commands[].path` | array<string> | required | Command path tokens. |
| `commands[].display` | string | required | Joined command path. |
| `commands[].tier` | string | required | `everyday` or `advanced` for public commands. |
| `commands[].summary` | string | required | First help line. |
| `commands[].has_subcommands` | bool | required | Whether the command has public children. |
| `commands[].supports_json` | bool | required | Whether the command supports JSON output. |
| `commands[].mutates` | bool | required | Whether the command can change repository or process state. |
| `commands[].supports_op_id` | bool | required | Whether the command accepts idempotent `--op-id`. |
| `commands[].persists_op_id` | bool | required | Whether the command contract preserves a generated op-id across an interrupted retry loop. |
| `commands[].observe_only` | bool | required | Whether the command is contractually observe-only. |
| `commands[].may_initialize` | bool | required | Whether the command may create `.heddle`/repository metadata. |
| `commands[].may_import_git` | bool | required | Whether the command may import Git history or mappings. |
| `commands[].may_write_worktree` | bool | required | Whether the command may materialize or rewrite worktree files. |
| `commands[].may_move_ref` | bool | required | Whether the command may move Heddle or Git refs. |
| `commands[].destructive_requires_force` | bool | required | Whether destructive execution requires explicit force. |
| `commands[].side_effect_class` | string | required | Derived side-effect class from the command contract table. |
| `commands[].first_run_behavior` | string | required | Derived first-run policy from the command contract table. |
| `commands[].json_kind` | string | required | JSON output class (`json`, `jsonl`, `json_or_jsonl`, or `none`). |
| `commands[].schema_verbs` | array<string> | required | Runtime schema verb(s) registered for this command. |
| `commands[].documented_schema_verbs` | array<string> | required | Schema verb(s) checked against samples in this document. |
| `commands[].options` | array<object> | required | Public flags/options local to that command. |
| `commands[].arguments` | array<object> | required | Public positional arguments local to that command. |
| `global_options` | array<object> | required | Global flags accepted across commands. |
| `recommended_action_placeholders` | array<string> | required | Explicit placeholder/raw-recovery actions that cannot parse directly through Clap. |

```json
{
  "commands": [
    {
      "path": ["status"],
      "display": "status",
      "tier": "everyday",
      "summary": "Show repository status",
      "has_subcommands": false,
      "supports_json": true,
      "mutates": false,
      "supports_op_id": false,
      "persists_op_id": false,
      "observe_only": true,
      "may_initialize": false,
      "may_import_git": false,
      "may_write_worktree": false,
      "may_move_ref": false,
      "destructive_requires_force": false,
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
    "git add <files> && heddle continue"
  ]
}
```

---

## `heddle review show --json`

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

`heddle review sign --json` emits:

```json
{"signature_id": "...", "change_id": "..."}
```

`heddle review next --json` emits either a `NextStateView`
(`{change_id, headline, existing_signatures}`) or the literal `null`
when there are no pending reviews in the scan window.

```json
{"change_id": "hd-def456", "headline": "Tighten parser recovery", "existing_signatures": []}
```

`heddle review health --json` emits:

```json
{"entries": [{"module_id": "...", "fire_rate": 0.42, "warn": false}], "window_states": 12}
```

---

## `heddle transaction commit --json`

```json
{"change_id": "hd-def456", "op_count": 7}
```

`change_id` was previously named `state_id`; the rename matches the
canonical naming for state identifiers across the CLI.

---

## `heddle bridge git init|export|import|sync|push|pull --json`

All bridge ops emit JSON via `serde_json::json!{}` with consistent
key naming:

| Verb | Shape |
|------|-------|
| `init` | `{"initialized": true, "path": "..."}` |
| `export` | `{"states_exported": N, "threads_synced": N, "markers_synced": N, "destination": "..."}` |
| `import` | `{"commits_imported": N, "states_created": N, "branches_synced": N, "tags_synced": N, "skipped_non_commit_refs": N, "partial_mirror_refs": N}` |
| `sync` | `{"states_exported": N, "commits_imported": N, "threads_synced": N, "markers_synced": N}` |
| `push` | `{"pushed": true, "remote": "origin", "trust": {...}}` |
| `pull` | `{"pulled": true, "remote": "origin", "trust": {...}}` |

`heddle bridge git init --json` emits:

```json
{"initialized": true, "path": "/work/project/.heddle/git"}
```

`heddle bridge git export --json` emits:

```json
{"states_exported": 3, "threads_synced": 1, "markers_synced": 2, "destination": "/work/project/.heddle/git"}
```

`heddle bridge git import --json` emits:

```json
{"commits_imported": 4, "states_created": 4, "branches_synced": 2, "tags_synced": 1, "skipped_non_commit_refs": 0, "partial_mirror_refs": 0, "already_in_sync": false}
```

`heddle bridge git sync --json` emits:

```json
{"states_exported": 3, "commits_imported": 4, "threads_synced": 1, "markers_synced": 2}
```

`heddle bridge git push --json` emits:

```json
{"pushed": true, "remote": "origin", "trust": {}}
```

`heddle bridge git pull --json` emits:

```json
{"pulled": true, "remote": "origin", "trust": {}}
```

---

## `heddle diagnose --json`

Comprehensive doctor-style report. This is the one place outside
`bridge git status` where `git_overlay_import_hint` is part of the
JSON contract — diagnose is the catch-all health surface and its job
is to surface every relevant signal for the operator.

Top-level fields: `repository`, `repository_capability`,
`storage_model`, `hosted_enabled`, `git_overlay_import_hint` (object
or `null`), `operation`, `remote_tracking`, `thread`, `state`,
`changes`, `workspace`, `health`, `profile`. All fields are required;
`Option<...>` fields serialize as explicit `null`.

```json
{
  "repository": "/work/project",
  "repository_capability": "git-overlay",
  "storage_model": "git+heddle-sidecar",
  "hosted_enabled": false,
  "git_overlay_import_hint": null,
  "operation": null,
  "remote_tracking": null,
  "thread": null,
  "state": null,
  "changes": {"modified": [], "added": [], "deleted": []},
  "workspace": {"thread_count": 0},
  "health": {"status": "clean"},
  "profile": null
}
```

---

## Other verbs

The following verbs also emit `--json`. Their shapes follow the same
discipline; see the corresponding handler in `crates/cli/src/cli/commands/`:

`heddle blame`, `heddle bisect`, `heddle checkpoint`, `heddle cherry-pick`,
`heddle clean`, `heddle clone`, `heddle collapse`, `heddle compare`,
`heddle conflict show`, `heddle context get/set`, `heddle diff`,
`heddle discuss`, `heddle doctor docs`, `heddle fetch`, `heddle fork`,
`heddle fsck`, `heddle goto`, `heddle init`, `heddle integration`,
`heddle maintenance`, `heddle merge`, `heddle monitor`, `heddle ready`,
`heddle rebase`, `heddle remote`, `heddle resolve`, `heddle retro`,
`heddle revert`, `heddle session`, `heddle capture`, `heddle stash`,
`heddle support`, `heddle thread show/start/captures/refresh`,
`heddle try`, `heddle attempt`, `heddle undo`, `heddle watch`.

Each of these:

- Emits a single JSON document on `--json` (or one document per line for streaming verbs like `watch`).
- Uses `change_id` (not `state_id` or `id`) for state identifiers.
- Uses `created_at` (not `timestamp` or `recorded_at`) for state-creation timestamps.
- Serializes `Option<...>` semantic fields as explicit `null`.
- Serializes empty collections as `[]` / `{}`.
- Does not carry `git_overlay_import_hint` or `missing_branches`
  payloads; those live only in `heddle bridge git status` and
  `heddle diagnose`.

---

## Error envelope (cross-cutting)

`error` emits the following stderr envelope when JSON output is selected
and the command fails. Stdout schemas above describe the success shape;
this schema describes the failure shape so scripts can parse failures
without scraping freeform text.

```json
{
  "error": "repository not found at /tmp/scratch",
  "hint": "Run `heddle init` to initialize a repository here.",
  "kind": "repository_not_found"
}
```

### Fields

| Field   | Type   | Optionality | Semantics |
|---------|--------|-------------|-----------|
| `error` | string | required    | Human-readable failure message (the anyhow chain rendered via `{:#}`). Never empty. |
| `hint`  | string | required    | One-line actionable next step (e.g. ``Run `heddle init`…``). Empty string when no specific guidance applies. |
| `kind`  | string | required    | Stable predicate name keying the hint class. Empty string when the failure didn't match a known class. |

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

### Stream contract

- Envelope is always on **stderr**, never stdout. Stdout stays available
  for partial output (an interrupted streaming verb may still flush
  bytes before the envelope appears on stderr).
- One envelope per process invocation. Polling scripts that retry on
  failure won't get a second envelope unless they re-invoke `heddle`.
- The text mode equivalent is `Error: <message>\nHint: <hint>` on
  stderr; the envelope is the structured form of the same information.
