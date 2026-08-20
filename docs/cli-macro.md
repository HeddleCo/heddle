# CLI verb declaration (heddle#205)

**Status:** shipped for `init`. Remaining schema-bearing verbs still use
hand-written mirrors.

Heddle used to declare each CLI verb twice: `clap::Args` for parse /
`--help`, and a separate schemars mirror for `heddle schemas`. Drift
was only caught later by `heddle doctor schemas` / `doctor docs`.

A migrated verb now has one construction-time key and one output type:

```rust
#[derive(Clone, Debug, clap::Args, heddle_cli_macro::HeddleVerbArgs)]
#[heddle_verb("init")]
pub struct InitArgs { /* flags + positionals */ }

#[derive(Serialize, JsonSchema, heddle_cli_macro::HeddleVerbOutput)]
#[heddle_verb("init")]
#[schemars(rename = "InitSchema")]
pub struct InitOutput { /* wire fields */ }
```

`HeddleVerbArgs` / `HeddleVerbOutput` are thin. They stamp
`HEDDLE_VERB` only. They do **not** add `clap::Args` or
`JsonSchema` — those stay as explicit sibling derives so `--help`,
completions, and schema dialect keep flowing through clap and
schemars.

The existing command catalog still owns path / mutates / op-id /
side-effect facts. The existing `schema_registry!` table still owns
lookup. Constructed verbs point that table at the real output type
(`InitOutput`) instead of a parallel `InitSchema` mirror.

## Descriptions and examples

- `--help` text lives on **args** fields as `///`.
- JSON Schema `description` lives on **output** fields as `///`.
  Args and output do not share fields, so they do not share comments.
- Skip-serialized output fields must also take `#[schemars(skip)]`.
  Otherwise schemars re-exposes them as required `writeOnly`
  properties the wire never emits.
- Wire-string paths use `#[schemars(with = "String")]`.
- Published schema titles stay stable with `#[schemars(rename = "...")]`
  (same pattern as `StatusReport` → `StatusSchema`).

## What this does not do

- It does not consolidate the 85-verb surface (heddle#473).
- It does not version CLI docs (heddle#479).
- It does not unify arg shapes (heddle#1161).
- It does not resolve command context / plan-outcome types.
- Unmigrated verbs still have mirrors. Construction-enforced pairing
  applies only to [`CONSTRUCTED_SCHEMA_VERBS`](../crates/cli-contract/src/cli/commands/verb_surface.rs).

## Remaining migration

Migrate the next batches against the same derives and the same
registry, one output type at a time. Suggested order after `init`:

1. Core-loop writes: `adopt`, `capture`, `commit`, `ready`, `land`
2. Thread lifecycle: `start`, `thread *` (except clone/checkout paths)
3. Remote / review / discuss / agent envelopes
4. Operator and maintenance verbs

Stay off `clone` / `checkout` while heddle#1446 / #1216 are in flight.
Do not add a second catalog.
