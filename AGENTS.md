# Heddle

Heddle is an AI-native VCS. This repo is the public CLI. The hosted store is weft. The web is tapestry. The Git engine is sley. Shared wire types live in heddle-api.

The job is the fastest VCS a human or an agent can actually use. If a change makes the first screen, the help, or a verb slower or weirder, it is not done.

## What we do not trade away

1. **Everyday verbs.** The product is ~23 verbs: init, clone, status, diff, capture, start, ready, land, undo, pull, push, resolve, continue, log, show, query, review, discuss, context, whoami, daemon, doctor, help. Help is the first screen.
2. **The human is the audience.** Agents drive the CLI. A person reads the output. Quiet chrome. Help is the product.
3. **Fail closed, page don't ceiling.** Admission is a page. Depth is the client's problem.
4. **Simple Rust that will still read next year.** Prefer reuse over a new helper. `impl` over `dyn`. No `unwrap`/`expect` in production paths. Tests may `expect`. Zero-copy where it actually matters.

## A note from Luke

I like ambitious ideas, simple systems, and software that feels obvious. Do not preserve complexity just because it already exists. Do not introduce machinery because it looks architecturally impressive. Understand the real constraint, then fight for the smallest model that makes the correct behavior unsurprising.

These are good defaults. My preferences in the thread override them.

## A small glossary

- **you** — the agent changing this repo
- **we** — Luke and the people building Heddle
- **user** — the person (or agent) running `heddle`
- **thread** — isolated work with a hidden checkout
- **context** — annotations that live with the code
- **discuss** — a scoped conversation
- **query** — ask history; `log` walks it

## The ways to hurt this repo

1. **A second view RPC.** heddle and tapestry read the same proto. Add a field on the existing message.
2. **Minting authority on the server.** This CLI mints client roots. Weft only verifies.
3. **Proof that didn't run.** Compile-only is not proof. Run the cargo tests for the files you changed, and paste the output.

## Hit every surface

The usual defect is a change that works on the path you tested and is missing everywhere else. Before calling CLI work done, say which of these applied:

- **The verb.** Help text, clap args, and the rust path must agree. If you added a flag, the first screen has to know what to do next.
- **Human and agent.** Default output is for a person. `--json` / agent mode is additive.
- **Git interop.** Import, export, and projection go through sley.
- **The wire.** If the change crosses weft, the field lives in heddle-api first.
- **Reverse states.** If you added a way in, add the way out and the way to see it.

## Verify

Smallest proof the change works. Targeted `cargo test` for the crate and behavior you touched. CI owns the full suite.

A test that hardcodes the return value you wanted is not a test of intent.

## Where code lives

- `crates/cli` — verbs, help, output
- `crates/cli-args` — clap surface; if it's not here, the user can't type it
- `crates/core`, `crates/objects`, `crates/ingest` — the store and history
- `crates/repo` — working tree / checkout
- sley — Git engine (separate repo)
- heddle-api — proto (separate repo)
- weft, tapestry — other repos

## Taste

- Complexity belongs at the Git/weft boundary. The verb path stays boring.
- Comments say how a thing is used, and move when the code moves.
- Prefer the current model over a compatibility shim. Redesign the seam or finish the migration.
- If a default here fights the task, say so and get a human before breaking it.
