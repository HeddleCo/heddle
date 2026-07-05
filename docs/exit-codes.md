# Heddle CLI exit codes

The CLI follows BSD `sysexits.h` so codes mean the same thing to humans, init
systems, and shell scripts that already understand them. Agents that retry on
transient failures can branch on the exit code without parsing stderr.

The canonical mapping lives in
[`crates/cli/src/exit.rs`](../crates/cli/src/exit.rs) as `HeddleExitCode`. Each
command's catalogued codes live on its `CommandContract.exit_codes` entry in
[`crates/cli/src/cli/commands/command_catalog.rs`](../crates/cli/src/cli/commands/command_catalog.rs).

Classification is keyed on typed error kinds — the `RecoveryAdvice.kind`
discriminator, `HeddleError` variants, and typed remote/config errors — never on
user-visible message text, so rewording an error can't silently change its exit
code. Each typed kind-to-code pair is pinned by a regression test in `exit.rs`.

## Codes

| Code | Symbol      | Meaning                                                                 |
| ---: | ---         | ---                                                                     |
|   0  | `Ok`        | Success.                                                                |
|  64  | `Usage`     | Invalid CLI args, unknown subcommand, malformed flag (`EX_USAGE`).      |
|  65  | `DataErr`   | Well-formed input, semantically rejected (`EX_DATAERR`). Includes corrupted/undecodable repository state and `--output json`/`json-compact` against a command without that output contract. |
|  73  | `CantCreat` | Output file refused — exists, unwritable, or state dir uncreatable.     |
|  74  | `IoErr`     | Generic IO failure during read/write (`EX_IOERR`). Default fallback.    |
|  75  | `TempFail`  | Transient failure; safe to retry with the same args (`EX_TEMPFAIL`).    |
|  76  | `Protocol`  | Remote rejected the payload; retrying without changing inputs will fail the same way (`EX_PROTOCOL`). |
|  77  | `NoPerm`    | Operation refused for permission reasons (`EX_NOPERM`).                 |
|  78  | `Config`    | Configuration or a required precondition is missing, ambiguous, or invalid — not just config-*file* errors. Covers unconfigured remotes/upstreams, a missing repository, and conflicting identity. |

`2` is reserved for `set -e` / unhandled panic and is never emitted
intentionally — let it surface naturally.

## Agent notes

- **`75` (TempFail) is the only "safe to retry" code.** If you retry on any
  other failure, you risk doubling state changes (e.g. `push` returned `76`
  → the remote rejected your payload; retrying sends the same payload and
  gets the same answer).
- **`76` (Protocol) means the inputs are the problem, not the network.**
  Don't loop. Surface to the human or change strategy.
- **`78` (Config) is the right code when a precondition is missing**
  (no upstream, no remote configured, no default remote for `push`/`pull`,
  no repository at the requested path, ambiguous identity) — it is not
  limited to config-file parse errors. Agents should print the missing
  setting rather than retry.
- **`65` (DataErr) covers semantic rejection of well-formed input**
  (e.g. `commit` with nothing to capture, `merge` with unresolvable
  conflict, `fsck --repair git` that needs a `--prefer` side chosen,
  repository state that fails decoding — `state_corrupted` — and
  `--output json`/`json-compact` requested from a command without that
  output contract). Agents must surface the condition (for unsupported
  output, fall back to a supported `--output` mode); no retry with the
  same inputs will help.
- **`74` (IoErr) is the catch-all.** When a command's contract does not
  declare a more specific code, treat a non-zero exit as `IoErr` and surface
  the stderr envelope.

## Schema/contract stability

The CLI's `cargo` version IS the JSON contract version. Pin a
`heddle-cli` version constraint (e.g. `>= 0.X.Y`) in your agent's
dependencies and the catalogued output shapes (`output_kind`,
declared discriminators, `exit_codes`) are stable for that minor.

- Breaking changes to any catalogued output bump the **minor** pre-1.0 and
  the **major** post-1.0.
- Additive changes (new fields with default `null`, new optional exit codes
  added to `exit_codes`) bump the **patch**.
- See [`STABILITY.md`](STABILITY.md) for the broader 1.0 stability gate
  this rule is one component of.

## Coverage

Today, only a representative subset of commands has populated
`exit_codes`:

- `init`, `verify`, `push`, `pull`, `commit`, `merge`, `status`
- `import git`, `fsck --repair git`, `sync git`

Commands not yet swept implicitly contract to `0` on success and an
unspecified non-zero on failure — currently always mapped through
`HeddleExitCode::from_error` to one of the codes above. Treat them as
"may return any code above" until their contract entry declares
`exit_codes`.

The lint in
[`crates/cli/tests/cli_integration/oss_cli_polish.rs`](../crates/cli/tests/cli_integration/oss_cli_polish.rs)
(`exit_codes_declared_have_doc_entry`) asserts every non-`Ok` code that
appears in any command's `exit_codes` is documented in the table above.
