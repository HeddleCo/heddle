# `@heddle/*` wrapper substrate — JSON contract + generated TypeScript types

This directory holds the machine-readable contract a Node/Electron agent
harness uses to drive `heddle` (npm wrapper spike #580; API class #583;
package + CI #584). It is **generated** from heddle's own runtime
schema introspection — there are no hand-authored types to drift.

## What's here

| Path | Source | Purpose |
|---|---|---|
| `generated/heddle-schemas.ts` | generated | TypeScript types for every `--output json` verb, a `HeddleVerbOutputs` verb→payload map, `HeddleSchemaVerb` union, `HEDDLE_SCHEMA_VERBS` array, and the `HEDDLE_SCHEMA_VERSION` pin. |
| `generated/heddle-schemas.json` | generated | The raw JSON Schemas keyed by verb (`{ schemaVersion, verbs }`), for runtime validation (e.g. Ajv) in the wrapper. |

Import from the package (#584) like:

```ts
import type { CommitSchema, HeddleVerbOutputs } from "./generated/heddle-schemas";
```

## Regenerating

The types come straight from `crates/cli/src/cli/commands/schemas.rs` via the
runtime introspection the CLI already ships (`heddle schemas <verb>`,
`heddle commands --output json`):

```sh
scripts/gen-ts-types.sh
# or, directly:
cargo run -p heddle-cli --example gen_ts_types -- clients/npm/generated
```

Output is deterministic (everything sorted), so regenerating against an
unchanged contract is a no-op diff. After any schema change, regenerate and
also run `heddle doctor schemas` so the doc samples in
[`docs/json-schemas.md`](../../docs/json-schemas.md) stay consistent.

## Schema version pin

`HEDDLE_SCHEMA_VERSION` (and `heddle-schemas.json`'s `schemaVersion`) is the
`heddle-cli` crate version the types were generated from. Per
[`docs/exit-codes.md`](../../docs/exit-codes.md#schemacontract-stability),
**the CLI's cargo version IS the JSON contract version**: catalogued output
shapes (`output_kind`, declared discriminators, `exit_codes`) are stable
within a minor. The wrapper should pin a `heddle-cli` constraint matching the
version these types were generated from and regenerate on a minor bump.

## Harness-op coverage (audited #581)

Every operation an Electron agent harness drives already has a stable,
documented, schema-backed JSON shape — no gaps. The generated map keys are the
canonical verbs; the doc column links the field-by-field reference.

| Harness op | Verb | Output type | Doc |
|---|---|---|---|
| adopt | `adopt` | `AdoptSchema` | `heddle adopt --output json` |
| init | `init` | `InitSchema` | `heddle init --output json` |
| status | `status` | `StatusSchema` | `heddle status --output json` |
| start thread | `start` / `thread create` | `ThreadStartSchema` | `heddle start --output json` |
| commit | `commit` | `CommitSchema` | Core loop mutation schemas |
| log | `log` | `LogSchema` | `heddle log --output json` |
| diff | `diff` | `DiffSchema` | `heddle diff --output json` |
| fetch | `fetch` | `FetchSchema` | (transport schemas) |
| push | `push` | `PushSchema` | (transport schemas) |
| export | `bridge git export` | `BridgeExportSchema` | `heddle bridge git …` |

## `--op-id` retry convention

Mutating verbs accept a caller-supplied `--op-id <id>` for **idempotent
retries**. heddle records the operation under that id; a retry with the same
`--op-id` and args replays the recorded result instead of re-applying the
mutation. The wrapper should:

1. **Generate one stable `--op-id` per logical operation** (e.g. a UUID minted
   when the harness first issues the command) and reuse it verbatim on retry.
2. **Only retry on exit code `75` (`TempFail`).** Per
   [`docs/exit-codes.md`](../../docs/exit-codes.md), `75` is the *only*
   safe-to-retry code (transient failure, same args are safe). `76`
   (`Protocol`), `78` (`Config`), `65` (`DataErr`) mean the inputs are the
   problem — surface, don't loop.
3. **Read back the replay markers** on the JSON payload of op-id-supporting
   verbs: `op_id`, `idempotency_status`, `replayed` (bool), and
   `operation_record`. `replayed: true` means the result came from the
   recorded operation, not a fresh apply — the retry was a safe no-op.

All mutating harness ops support `--op-id` (`init`, `adopt`, `commit`,
`start`, `fetch`, `push`, `bridge git export`, …); read-only ops (`status`,
`log`, `diff`, `show`) don't mutate and so take none. The authoritative
per-command list is the command catalog: `heddle commands --output json`,
field `supports_op_id`.
