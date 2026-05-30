# heddle#327 — CLI schema-macro shape (schemars vs custom emitter + attribute design)

**Status:** spike (decision doc + PoC). The PoC crate `crates/cli-macro-poc/`
lands with this spike *only* as the measurement artifact; it is **not** wired
into `crates/cli` and heddle#205 deletes it when it lands the production
`crates/cli-macro/`. No production CLI behavior changes in this issue.

**Scope:** resolve the three open design questions that block heddle#205
(single-source-of-truth CLI macro): (1) where examples/descriptions live,
(2) schemars vs a custom emitter, (3) how to layer the input shape (clap) and
the output shape (JSON Schema) under one macro. Deliverable = this spec + a PoC
that renders one real verb (`heddle init`) both ways and measures the result.

> **EXISTING vs PROPOSED — read this first.** Everything under
> "Today's architecture" is verified against the tree at the cited `path:line`
> (2026-05-30). Everything under "Recommended shape" and "Impl shape for #205"
> is **proposed** — the `#[heddle_verb]` / `HeddleVerbOutput` macro names, the
> `inventory`-style auto-registration, and the deletion of the
> `schema_registry!` table do **not** exist yet; they are #205's work. The PoC
> crate proves the *measurable* claims (both emitters cover the documented
> keys; schemars lifts doc comments; the discriminator-const gap is real); it
> does **not** implement the proc-macro itself.

---

## 1. Today's architecture (verified 2026-05-30)

A single `--output json` verb's shape is declared in **three** places, kept in
sync by two PR-time gates rather than by construction:

| Declaration | For `init` | Derives | Drives |
|---|---|---|---|
| Clap args struct | `InitArgs` — `crates/cli/src/cli/cli_args/commands_args.rs:12` | `clap::Parser` | parsing + `--help` |
| Real output struct | `InitOutput` — `crates/cli/src/cli/commands/init.rs:23` | `serde::Serialize` only | the actual `--output json` bytes |
| Schema mirror | `InitSchema` — `crates/cli/src/cli/commands/schemas.rs` (`pub struct InitSchema`) | `schemars::JsonSchema` | `heddle schemas init` + drift checks |

The mirror is registered by the hand-maintained `schema_registry!` macro table
(`crates/cli/src/cli/commands/schemas.rs:58`, e.g. `(&["init"], InitSchema)` at
:59). schemas.rs:1–14 states the mirror exists *deliberately* — to avoid
threading `JsonSchema` through every workspace output type (`repo`, `objects`,
…) — at the cost that "when a real output struct changes, the mirror here must
change too."

Drift is policed at PR time, not prevented:

- `heddle doctor schemas` (`crates/cli/src/cli/commands/doctor_schemas.rs`)
  extracts the literal sample under each `## heddle <verb> --output json`
  heading in `docs/json-schemas.md` and compares its **top-level keys** against
  the registered schema's `properties` keys (keys-only, by design —
  doctor_schemas.rs:15–19).
- `heddle doctor docs` (`crates/cli/src/cli/commands/doctor_docs.rs`) walks
  `heddle <verb> [flags]` invocations in markdown and flags clap-level drift,
  built on `Cli::command()` so it tracks the binary.

So the macro heddle#205 wants must collapse declarations **2 and 3** into one
(emit the registered schema *from* the real output struct) and auto-populate
the registry, deleting the mirror and the `schema_registry!` table. The clap
side (declaration 1) stays — clap remains the user-facing parser; the macro
only needs to *co-locate* the args declaration with the output declaration and
share a verb key.

---

## 2. The PoC — one verb, both ways

`crates/cli-macro-poc/` reproduces `init` faithfully: the field set and
doc-comment text are copied from the registered `InitSchema` mirror and the
`docs/json-schemas.md` sample. It emits the output schema two ways:

- **Path A — schemars** (`schemars_path::schema()`): `#[derive(JsonSchema)]` on
  the output struct + `schema_for!`. This is what heddle registers **today**.
- **Path B — custom emitter** (`custom_path::schema()`): a hand-written field
  table → `serde_json::Value`, no derive, no `schemars` dependency. Represents
  the code a heddle-specific emitter macro would generate.

`cargo test -p heddle-cli-macro-poc -- --nocapture` prints both schemas and the
table below; the assertions in `tests/measure.rs` are the contract checks.

### Measured comparison (`init`, 17 output fields)

| metric | schemars | custom |
|---|---|---|
| pretty-printed schema bytes | 4435 | 4070 |
| top-level property keys | 17 | 17 |
| covers all 13 documented sample keys | ✅ | ✅ |
| JSON Schema dialect | draft-07 | draft 2020-12 |
| nested types (`InitPrincipal`) | `$ref` + `definitions` | inlined |
| `output_kind` discriminator pinned | ❌ (`{"type":"string"}`) | ✅ (`"const":"init"`) |
| field descriptions from `///` | ✅ (native) | ✅ (table column) |
| carries example payload | ✅ | ✅ |
| net-new schema code per verb | 0 (derive) | ~1 field-table row each |
| extra dependency | `schemars` (already in tree) | none |

(Bytes/keys are emitted by `print_measurement_table`; coverage and the
discriminator gap are asserted by the other four tests.)

### What the measurement decides

1. **schemars is already the registered shape.** Path A *is* what
   `heddle schemas init` returns today, so adopting it changes **zero** schema
   semantics — the macro's only job is to move the derive from the throwaway
   mirror onto the real `InitOutput` and auto-register it. Migration risk ≈ 0
   for the gate corpus.
2. **The discriminator gap is real and matters.** schemars renders
   `output_kind` as a bare `{"type":"string"}` — it cannot express the
   `"const":"init"` pin from a plain `String`/`&'static str` field. heddle
   routes machine consumers on `output_kind` (the `json_discriminators` list in
   `crates/cli/src/cli/commands/command_catalog.rs`). The custom emitter pins
   it trivially. This is the single concrete thing schemars-as-derive costs us
   — and it's a *quality* gap, not a *correctness* one (the runtime output
   still carries the right value; only the schema under-describes it).
3. **`$ref` vs inlined is a wash for the current gate.** `doctor schemas` is
   keys-only and top-level, so schemars' `definitions`/`$ref` nesting passes
   fine. It only bites if we later want the flat `docs/json-schemas.md` samples
   to validate against the schema with an off-the-shelf validator that doesn't
   resolve `$ref` — not a requirement today.

---

## 3. Spike answers

### Q1 — Where do examples and descriptions live?

**Descriptions: `#[doc]` (`///`). Decided — proven, no new attribute needed.**
schemars lifts both struct-level and field-level doc comments into the schema's
`description` natively (PoC test `schemars_emits_field_descriptions_from_doc_comments`;
the rendered schemars schema shows `"path": { "description": "Path to the
initialized `.heddle` …" }`). So the narrative that lives as prose blocks in
`docs/json-schemas.md` today moves onto the field as a doc comment, and the
macro carries it into *both* `--help` (via clap, which also reads `///`) and the
JSON schema (via schemars). One source, two surfaces.

**Examples: a sibling `#[heddle_verb(example = …)]` key the macro lowers to a
typed example function — NOT inline literals, NOT prose blocks.** schemars'
native form is `#[schemars(example = "init_example")]`, where the value names a
free function returning a `Serialize` value; it lands in the schema's `examples`
array (PoC test `schemars_carries_the_example_payload`). The awkwardness worth
designing around:

- The example must be a *function path*, not an inline literal — schemars 0.8
  has no inline-example attribute.
- It's one example function per type.

Recommendation: the `#[heddle_verb]` macro exposes `example = path::to::fn` and
forwards it to schemars on the schema side. The win over today's prose samples
in `docs/json-schemas.md`: the example is a **real typed value of the output
struct**, so it cannot drift from the struct's shape — it stops compiling if a
field is renamed. That is strictly better than the current literal-JSON blocks
that `doctor schemas` has to police after the fact.

### Q2 — schemars vs custom emitter

**Recommendation: schemars (Path A) for #205 v1. Keep the custom emitter PoC as
the documented fallback.** Rationale, from the measurement:

- It is already the registered shape → the macro is a *refactor* (delete the
  mirror, derive on the real struct, auto-register), not a re-baseline of every
  schema sample. The custom emitter would change dialect, nesting, and
  const-pinning, forcing a rewrite of the `docs/json-schemas.md` corpus and a
  re-blessing of `doctor schemas`.
- The one real loss (discriminator-const) is recoverable *within* the schemars
  path without adopting the whole custom emitter: a small `JsonSchema` helper /
  newtype for `output_kind` that emits `{"const": "<verb>"}`, applied by the
  macro from the `verb` key it already knows. File that as a #205 follow-up, not
  a blocker.

**When to revisit the custom emitter:** if we ever publish these schemas for
external validation and need (a) inlined flat schemas that validate the
`docs/json-schemas.md` samples with a stock validator, and (b) discriminator
const-pinning across the board, the custom emitter delivers both with no
`schemars` opinions to fight. The PoC's `custom_path` module is a working
~90-line template for that future. Until then it's net-new code that buys us a
gap the gate doesn't care about.

### Q3 — Layering input (clap) + output (JSON) under one macro

The input and output are genuinely different types: `InitArgs` has 7 input
fields, `InitOutput` has 17 output fields, **zero overlap**. Forcing them into
one struct is wrong. Two viable couplings:

- **(a) Attribute on a module:** `#[heddle_verb(name = "init")] mod init { pub
  struct Args {…} pub struct Output {…} }` — the macro emits the clap derive on
  `Args`, the schema derive + registry entry on `Output`.
- **(b) Two cooperating derives keyed by verb:** `#[derive(HeddleVerbArgs)]` on
  the args struct and `#[derive(HeddleVerbOutput)]` on the output struct, tied
  by a shared `#[heddle_verb("init")]` key. The `Output` derive is the one that
  matters — it adds `JsonSchema` *and* emits the registry registration that
  today is the hand-written `(&["init"], InitSchema)` row.

**Recommendation: (b).** It keeps clap and schema as separate concerns on
separate types (which they are), couples them by a verb-name key rather than by
forcing a shared type, and localizes the new magic to the output side — exactly
where the drift problem lives. The clap side barely changes: `HeddleVerbArgs`
is mostly a passthrough that re-exports `clap::Parser` and records the verb key,
so `--help`, completions, and error messages keep flowing through clap
unchanged (a heddle#205 discipline guard).

The registry registration should use an `inventory`/`linkme`-style collected
registration so the `schema_registry!` table at schemas.rs:58 disappears
entirely — each verb registers itself at link time, and
`schema_for_registered_verb` becomes a lookup over the collected set instead of
a hand-maintained `if $verbs.contains(&verb)` chain.

---

## 4. Impl shape for heddle#205 (proposed — confirm scope with user)

1. **Land `crates/cli-macro/`** with the `HeddleVerbOutput` derive (schemars +
   collected registration) and the thin `HeddleVerbArgs` passthrough. Delete
   `crates/cli-macro-poc/`.
2. **Migrate one proof verb** (`init` is the smallest real output; `status` is
   the richest — pick `init` for the first landing per #205's "pick a small
   one"). Derive `HeddleVerbOutput` on the real `InitOutput`
   (`crates/cli/src/cli/commands/init.rs:23`); delete the `InitSchema` mirror
   and its `schema_registry!` row.
3. **Add the discriminator-const helper** so `output_kind` emits
   `{"const":"init"}` — recovers the one measured schemars gap.
4. **Run the full gate set** the migrated verb must still pass:
   `cargo test --locked -p heddle-cli --test cli_integration doctor_schemas_has_no_drift_or_unmatched_registered_verbs`,
   `… doctor_docs`, plus `target/debug/heddle doctor schemas --output json` and
   `doctor docs --all --output json` (the exact commands CI runs —
   `.github/workflows/rust-tests.yml:181-184`).
5. **Known friction to budget for:** real output structs that embed foreign
   workspace types (e.g. `InitOutput.trust: RepositoryVerificationState`,
   `#[serde(skip_serializing)]` at init.rs) must either skip those fields on the
   schema side too or grow `JsonSchema` impls. The mirror existed precisely to
   dodge this (schemas.rs:6–13); the macro re-confronts it. Each migration batch
   should enumerate the foreign types its verbs touch.
6. **Then** the verb-by-verb migration batches (~30 verbs, the registered set in
   `schema_registry!`), one issue each, as #205 already scopes.

---

## 5. Recommendation summary

| Question | Decision | Confidence |
|---|---|---|
| Descriptions | `#[doc]` `///`, carried to both `--help` and schema | high — proven in PoC |
| Examples | `#[heddle_verb(example = fn)]` → typed example fn → schemars `examples` | high — proven in PoC |
| schemars vs custom | **schemars** for v1 (zero re-baseline); custom emitter is the documented fallback | high — it is already the registered shape |
| Discriminator const | schemars gap; fix with a small `output_kind` `JsonSchema` helper, not the custom emitter | medium — helper not yet built |
| Macro layering | two derives (`HeddleVerbArgs` + `HeddleVerbOutput`) keyed by verb; output derive auto-registers via `inventory` | medium — proposed, not prototyped |

Nothing here blocks heddle#205; it unblocks it. The one open build-time
question (`inventory` vs `linkme` for collected registration) is a #205
implementation choice, not a design fork.
