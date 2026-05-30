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
> crate proves the *measurable* claims by asserting them against types that
> mirror the **real** `init.rs` (both emitters cover the documented keys;
> schemars lifts doc comments; the discriminator-const gap is real; schemars
> re-exposes the skip-serialized `verification` field; the typed example
> diverges from the curated doc sample); it does **not** implement the
> proc-macro itself.

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

The mirror also does hand-curation the real struct can't: the real `InitOutput`
carries a `#[serde(skip_serializing)] #[serde(rename = "verification")] trust`
field (`init.rs:41-44`) that never reaches the wire, and `InitSchema`
(`schemas.rs:842`) simply omits it. So the mirror and the wire bytes agree
today *because a human kept them in agreement* — which is exactly the property a
naive "derive on the real struct" migration loses (see §2 point 1).

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

`crates/cli-macro-poc/` reproduces `init` faithfully against the **real**
`init.rs` types, not the simplified mirror: the args struct is modelled as the
real `clap::Args` `InitArgs` wired through a `Subcommand` enum
(`crates/cli-macro-poc/src/args.rs`), and the output struct replicates the real
private `InitOutput` field-for-field — INCLUDING the `#[serde(skip_serializing)]
#[serde(rename = "verification")] trust` field
(`crates/cli-macro-poc/src/output.rs`). (The real `InitOutput` is a crate-private
`struct`, so a throwaway crate can't import it; it replicates the exact field
types and serde/schemars attributes instead.) It emits the output schema two
ways:

- **Path A — schemars** (`schemars_path::schema()`): `#[derive(JsonSchema)]` on
  the output struct + `schema_for!`. This is what heddle registers **today**.
- **Path B — custom emitter** (`custom_path::schema()`): a hand-written field
  table → `serde_json::Value`, no derive, no `schemars` dependency. Represents
  the code a heddle-specific emitter macro would generate.

`cargo test -p heddle-cli-macro-poc -- --nocapture` prints both schemas and the
table below; the assertions in `tests/measure.rs` are the contract checks.

### Measured comparison (`init`, 17 serialized fields + 1 skip-serialized)

| metric | schemars | custom |
|---|---|---|
| pretty-printed schema bytes | 5954 | 4096 |
| top-level property keys | 18 | 17 |
| covers all 13 documented sample keys | ✅ | ✅ |
| phantom `verification` property (never on wire) | ❌ **present** (`writeOnly`, **required**) | ✅ absent |
| JSON Schema dialect | draft-07 | draft 2020-12 |
| nested types (`InitPrincipalOutput`) | `$ref` + `definitions` | inlined |
| `output_kind` discriminator pinned | ❌ (`{"type":"string"}`) | ✅ (`"const":"init"`) |
| field descriptions from `///` | ✅ (native) | ✅ (table column) |
| carries example payload | ✅ | ✅ |
| net-new schema code per verb | 0 (derive) + skip-attrs as needed | ~1 field-table row each |
| extra dependency | `schemars` (already in tree) | none |

(Bytes/keys are emitted by `print_measurement_table`; the discriminator gap,
the phantom-`verification` drift, and documented-key coverage are each pinned by
a dedicated assertion in `tests/measure.rs`.)

### What the measurement decides

1. **schemars matches the registered shape for the *serialized* fields — but
   the migration is NOT semantics-free.** For the 17 serialized fields, Path A
   reproduces what `heddle schemas init` returns today. The catch the
   mirror-shaped PoC hid: deriving `JsonSchema` on the **real** `InitOutput`
   also picks up the `#[serde(skip_serializing)] trust` field. schemars treats
   `skip_serializing` as a schema-visible `writeOnly` property (it stays in the
   *deserialize* contract) — so the derived schema gains a `verification`
   property, lists it as **required**, and yet `heddle init` never emits it.
   The hand-written `InitSchema` mirror dropped that field by hand; the derive
   re-introduces it. `doctor schemas` checks only that documented sample keys
   *appear* in the schema (not that the schema has no extras), so it would
   **not** catch this. Migration is therefore a per-verb task, not a zero-touch
   refactor: each migrated output struct must add `#[schemars(skip)]` to its
   skip-serialized fields (the PoC asserts the drift in
   `schemars_re_exposes_skip_serialized_verification_field`). This does not sink
   the schemars recommendation — the fix is one attribute per skip-serialized
   field — but it does delete the "migration risk ≈ 0 / zero schema semantics"
   claim. See §4 item 5, which already budgets for this.
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

**Descriptions: `#[doc]` (`///`). Decided — proven, no new sibling attribute
needed *per surface*.** schemars lifts both struct-level and field-level doc
comments into the schema's `description` natively (PoC test
`schemars_emits_field_descriptions_from_doc_comments`; the rendered schemars
schema shows `"path": { "description": "Path to the initialized `.heddle` …" }`).
For each surface, the `///` is the single source — clap reads it for `--help`,
schemars reads it for the schema `description` — so no separate description
attribute is ever required.

**Caveat — it is NOT "one comment, both surfaces".** clap reads doc comments off
the **args** type (`InitArgs`) and schemars reads them off the **output** type
(`InitOutput`), and those are different types with **zero field overlap** (Q3).
A doc comment on an output field feeds the JSON schema but never `--help`; a doc
comment on an args field feeds `--help` but never the schema. So per-field
narrative lives in *two* places (one per type), not one. The prose blocks in
`docs/json-schemas.md` today describe the *output* shape, so they move onto the
**output** fields as doc comments (feeding the schema). The `--help` text is a
separate source on the args fields. The only way to get a literal single doc
comment driving both would be a module-level macro that maps one declaration to
both types (option (a) in Q3) — the two-derive shape we recommend keeps them
separate, by design.

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

**Concretely, the prose sample has *already* drifted, and the PoC proves it.**
The PoC's `init_example()` is the real typed value, so serializing it emits the
always-present `principal_status`, `principal_source`, `principal`, and
`principal_recommended_action` fields (none is `skip_serializing_if`). The
curated `## heddle init --output json` sample in `docs/json-schemas.md` omits
all four. The PoC asserts exactly this divergence
(`typed_example_diverges_from_curated_doc_sample`): the real output is a
superset of the documented sample by those four keys. So the in-code example is
**not** byte-for-byte the documented sample — and that is the point. A typed
example tracks the struct automatically; the hand-curated prose sample fell
behind. heddle#205 should rebaseline the `docs/json-schemas.md` `init` sample
from the typed example when it lands the derive.

### Q2 — schemars vs custom emitter

**Recommendation: schemars (Path A) for #205 v1. Keep the custom emitter PoC as
the documented fallback.** Rationale, from the measurement:

- For the serialized fields it is already the registered shape → the macro is a
  *refactor* (delete the mirror, derive on the real struct, auto-register), not
  a re-baseline of the whole sample corpus. The custom emitter would change
  dialect, nesting, and const-pinning, forcing a rewrite of the
  `docs/json-schemas.md` corpus and a re-blessing of `doctor schemas`. The
  schemars migration's per-verb cost is bounded and mechanical (see below),
  whereas the custom emitter's cost is corpus-wide.
- Two known schemars costs, both recoverable *within* the schemars path without
  adopting the whole custom emitter:
  - **discriminator-const** — a small `JsonSchema` helper / newtype for
    `output_kind` that emits `{"const": "<verb>"}`, applied by the macro from
    the `verb` key it already knows.
  - **`skip_serializing` phantom fields** — each migrated output struct must add
    `#[schemars(skip)]` to fields that serde skips on serialize (e.g. `init`'s
    `verification`/`trust`), or the schema gains required properties the wire
    never emits (§2 point 1). Mechanical, but per-field, so it can't be waved
    away as zero-touch.

  File both as #205 work, not blockers.

**When to revisit the custom emitter:** if we ever publish these schemas for
external validation and need (a) inlined flat schemas that validate the
`docs/json-schemas.md` samples with a stock validator, and (b) discriminator
const-pinning across the board, the custom emitter delivers both with no
`schemars` opinions to fight. The PoC's `custom_path` module is a working,
self-contained template for that future (no `schemars` dependency, and — unlike
the derive — it omits skip-serialized fields and pins the discriminator by
construction). Until then it's net-new code that buys us gaps the gate doesn't
care about today.

### Q3 — Layering input (clap) + output (JSON) under one macro

The input and output are genuinely different types: `InitArgs` has 7 input
fields (and derives `clap::Args`), `InitOutput` has 18 declared fields (17
serialized + the skip-serialized `trust`), **zero overlap**. Forcing them into
one struct is wrong. Two viable couplings:

- **(a) Attribute on a module:** `#[heddle_verb(name = "init")] mod init { pub
  struct Args {…} pub struct Output {…} }` — an *attribute* macro can rewrite the
  items before their derives expand, so it can attach `#[derive(clap::Args)]` to
  `Args` and `#[derive(JsonSchema)]` + the registry entry to `Output`, and can
  even map one declaration to both surfaces.
- **(b) Two cooperating derives keyed by verb:** `#[derive(HeddleVerbArgs)]` on
  the args struct and `#[derive(HeddleVerbOutput)]` on the output struct, tied
  by a shared `#[heddle_verb("init")]` key. **A derive macro cannot add another
  derive (`JsonSchema`) to the very struct it is invoked on** — derives only
  *append* items, they don't rewrite the annotated item's attribute list. So
  `HeddleVerbOutput` cannot "turn on schemars" by itself; the output struct must
  **also** `#[derive(schemars::JsonSchema)]` (i.e. `HeddleVerbOutput` *requires*
  a `JsonSchema` bound it does not provide), and `HeddleVerbOutput`'s own job is
  the registry registration that today is the hand-written `(&["init"],
  InitSchema)` row. (The alternatives — hand-implementing `JsonSchema` inside
  `HeddleVerbOutput`, or making it an attribute macro — collapse (b) into either
  the custom-emitter path or option (a).)

**Recommendation: (b)**, with the output struct co-deriving `JsonSchema`
explicitly. It keeps clap and schema as separate concerns on separate types
(which they are), couples them by a verb-name key rather than by forcing a
shared type, and localizes the new magic to the output side — exactly where the
drift problem lives. The clap side barely changes: `HeddleVerbArgs` is a
passthrough over **`clap::Args`** (the reusable arg set that subcommand tuple
variants like `Commands::Init(InitArgs)` consume — NOT `clap::Parser`, which
stays on the top-level `Cli`) that records the verb key, so `--help`,
completions, and error messages keep flowing through clap unchanged (a
heddle#205 discipline guard). The PoC's `src/args.rs` wires exactly this shape
(`Parser` on `Cli`, `Args` on the leaf) to prove the args type slots into a real
subcommand tree.

(If the literal "one doc comment feeds both `--help` and the schema" property
from Q1 turns out to be a hard requirement, that's the lever that tips the
choice to option (a): only a module-level attribute macro can map a single
declaration onto both types. (b) deliberately accepts two doc sources — one per
type — as the cost of keeping args and output as independent derives.)

The registry registration should use an `inventory`/`linkme`-style collected
registration so the `schema_registry!` table at schemas.rs:58 disappears
entirely — each verb registers itself at link time, and
`schema_for_registered_verb` becomes a lookup over the collected set instead of
a hand-maintained `if $verbs.contains(&verb)` chain.

---

## 4. Impl shape for heddle#205 (proposed — confirm scope with user)

1. **Land `crates/cli-macro/`** with the `HeddleVerbOutput` derive (registry
   registration; the output struct co-derives `schemars::JsonSchema` — the derive
   cannot add it, see Q3) and the thin `HeddleVerbArgs` passthrough over
   `clap::Args`. Delete `crates/cli-macro-poc/`.
2. **Migrate one proof verb** (`init` is the smallest real output; `status` is
   the richest — pick `init` for the first landing per #205's "pick a small
   one"). Co-derive `HeddleVerbOutput` + `JsonSchema` on the real `InitOutput`
   (`crates/cli/src/cli/commands/init.rs:23`); add `#[schemars(skip)]` to its
   `trust` field (else the schema gains a required `verification` property the
   wire never emits — §2 point 1); delete the `InitSchema` mirror and its
   `schema_registry!` row. **Rebaseline** the `docs/json-schemas.md` `init`
   sample from the typed example (it currently omits the always-serialized
   principal fields — Q1).
3. **Add the discriminator-const helper** so `output_kind` emits
   `{"const":"init"}` — recovers the measured discriminator gap.
4. **Run the full gate set** the migrated verb must still pass:
   `cargo test --locked -p heddle-cli --test cli_integration doctor_schemas_has_no_drift_or_unmatched_registered_verbs`,
   `… doctor_docs`, plus `target/debug/heddle doctor schemas --output json` and
   `doctor docs --all --output json` (the exact commands CI runs —
   `.github/workflows/rust-tests.yml:181-184`).
5. **Known friction to budget for — now measured, not hypothetical:** any field
   serde skips on serialize (`#[serde(skip_serializing)]`, e.g.
   `InitOutput.trust`/`verification` at init.rs:41-44) is schema-VISIBLE under
   schemars' derive (`writeOnly`, and required) — the PoC measures and asserts
   this. Each migrated struct must add `#[schemars(skip)]` to such fields, or it
   ships a schema that describes properties the command never emits (and
   `doctor schemas`, which only checks documented keys *appear*, won't catch it).
   Separately, output structs that embed foreign workspace types may need
   `JsonSchema` impls/bounds; the mirror existed precisely to dodge both
   (schemas.rs:6–13), and the macro re-confronts them. Each migration batch
   should enumerate the skip-serialized and foreign-typed fields its verbs touch.
6. **Then** the verb-by-verb migration batches (~30 verbs, the registered set in
   `schema_registry!`), one issue each, as #205 already scopes.

---

## 5. Recommendation summary

| Question | Decision | Confidence |
|---|---|---|
| Descriptions | `#[doc]` `///`, single source *per surface* — but args and output are separate types, so it's two doc sources, not "one comment, both surfaces" | high — proven in PoC |
| Examples | `#[heddle_verb(example = fn)]` → typed example fn → schemars `examples`; #205 rebaselines the prose `docs/json-schemas.md` sample (already drifted) from it | high — proven in PoC |
| schemars vs custom | **schemars** for v1; *not* zero-touch — per-verb `#[schemars(skip)]` for skip-serialized fields + a const helper. Custom emitter is the documented fallback | high — recommendation holds; "zero re-baseline" evidence corrected |
| Discriminator const | schemars gap (measured + asserted); fix with a small `output_kind` `JsonSchema` helper, not the custom emitter | medium — helper not yet built |
| `skip_serializing` drift | schemars re-exposes skip-serialized fields as required `writeOnly` props (measured + asserted); fix per-verb with `#[schemars(skip)]` | high — measured in PoC |
| Macro layering | two derives (`HeddleVerbArgs` over `clap::Args` + `HeddleVerbOutput`) keyed by verb; output struct **co-derives** `JsonSchema` (the derive can't add it); registry via `inventory` | medium — proposed, args shape prototyped in PoC |

The corrected measurements **do not change the overall recommendation** —
schemars for #205 v1, custom emitter as the documented fallback — but they
replace the "migration risk ≈ 0 / zero drift / one comment feeds both surfaces"
evidence with the accurate picture: a bounded, mechanical per-verb cost
(`#[schemars(skip)]` + a const helper + a docs-sample rebaseline). Nothing here
blocks heddle#205; it unblocks it, with eyes open. The one open build-time
question (`inventory` vs `linkme` for collected registration) is a #205
implementation choice, not a design fork.
