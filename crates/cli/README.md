# heddle-cli

The `heddle` binary — Heddle's foreground command-line interface.

For the full Heddle product overview, install instructions, and quickstart,
see [the workspace README](../../README.md). This file documents verbs whose
implementation details are too specific to belong in the product-level
README.

## Stack workflow

A *stack* is the descendant tree of related threads formed by walking
`ThreadRecord::parent_thread` links. The model lives in
[`repo::thread_stack`](../repo/src/thread_stack.rs); the value-type
projection used by tooling lives in
[`repo::stack_snapshot`](../repo/src/stack_snapshot.rs).

### Verbs

| Command | What it does |
|---|---|
| `heddle stack` | Describe the stack containing the current thread (or `--thread <name>`). Renders the root, member count, depth, and indented tree. |
| `heddle stack ready` | Surface the next stack-level action — `ready` / `blocked by <thread>` / `waiting-on-review (<thread>)`. Pipe-friendly. |
| `heddle stack snapshot` | Emit the JSON `RepositorySnapshot` projection for agentic tooling. Pretty-printed in TTY mode, compact in pipe mode. |

All three verbs are read-only.

### Next-action verdicts

`heddle stack ready` walks the stack and emits exactly one of four
verdicts. The rules:

1. **`blocked by <thread>`** — at least one member has
   `state = Blocked`. Blocked wins over everything; the operator must
   unblock before progress.
2. **`ready`** — every member is in `Ready`, `Merged`, `Promoted`, or
   `Abandoned`. The whole stack is shippable.
3. **`waiting-on-review (<thread>)`** — the stack is otherwise clean but
   one thread is still `Active` or `Draft`. The deepest in-flight thread
   is reported as the bottleneck.
4. **`unknown`** — the thread isn't part of any known stack, or its
   state is exotic (some mix the resolver can't classify).

### JSON shape

`heddle stack snapshot` emits this shape (additive fields may be added
without bumping `version`; non-additive changes bump it):

```jsonc
{
  "version": 1,
  "captured_at": "2026-05-23T17:08:00Z",
  "stacks": [
    {
      "root": {
        "name": "feature-a",
        "children": [{ "name": "feature-b", "children": [] }]
      }
    }
  ],
  "threads": [
    {
      "thread": "feature-a",
      "parent_thread": null,
      "base_state": "hd-…",
      "current_state": "hd-…",
      "state": "active",
      "freshness": "current"
    }
  ]
}
```

### Agentic harness integration

The Claude Code `PreToolUse` hook reads the snapshot and appends a stack
summary to `additionalContext` whenever the current thread is part of a
multi-member stack. Single-thread stacks add no signal worth burning
context on and are skipped. See
[`harness/claude_hook.rs`](src/harness/claude_hook.rs) for the
`format_stack_context` rendering.
