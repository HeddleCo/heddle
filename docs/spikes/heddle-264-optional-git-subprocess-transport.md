# heddle#264 — optional Git-subprocess transport for the Git lane

**Status:** spike (decision doc). No production implementation is included.
**Scope:** transport selection and execution for explicit Git-lane remote operations.
Sley remains Heddle's Git-format engine and the default Git transport. Native Heddle
repositories and native Heddle remotes remain fully functional without a Git executable.
Verified against this checkout on 2026-08-09.

**Decision up front:** add an optional transport-only backend named
`GitSubprocess`, selected before any network I/O when an explicit Git-lane request
or a recognized remote capability requires the user's installed Git. Do not retry a
failed Sley operation through Git. Keep Sley responsible for object identity, object
reads and writes, and local/offline Git behavior; keep Heddle's existing ref planner,
projection, and ingest layers above either transport. Require Git 2.41 or newer only
after `GitSubprocess` has been selected, because 2.41 is the first supported baseline
for parseable `git fetch --porcelain` output.

## 1. Why this is a transport backend, not a second Git engine

The domain model names Sley as Heddle's native Git-format engine and assigns Git
object identity and Git operation behavior to it (`CONTEXT.md:23-25`). Git Overlay
uses the checkout's real `.git` repository as source authority
(`CONTEXT.md:27-32`), while explicit native-to-Git bridge work can use the bare
Bridge Mirror at `.heddle/git` (`CONTEXT.md:43-45`). The workspace currently pins
both `sley` and `sley-transport` at 0.5.2 (`Cargo.toml:61-62`). None of those
ownership rules should change.

`GitSubprocess` is therefore below the projection and reconcile layers. It may ask
the installed `git` executable to advertise refs, negotiate, transfer packs, and
perform remote compare-and-swap updates. Once bytes are in the selected Git object
database, Heddle resumes its existing projection, ingest, and ref-publication flow
using Sley for inspection, object identity, and reachability. Local filesystem remotes
and offline object work never need the subprocess.

This distinction also preserves Heddle's first-class Gitlink model. Heddle already
represents mode `160000` as `FileMode::Gitlink` (`crates/object-model/src/object/tree.rs:42-90`),
imports a Gitlink as its foreign commit object ID without reading it as a blob
(`crates/ingest/src/importer.rs:855-884`), and exports it as a Git commit entry
(`crates/git-projection/src/git_export.rs:272-290`). Selecting Git for transport
must not turn submodule contents into superproject objects or replace this model.

## 2. Current Sley transport surface

The selection seam must wrap the functions Heddle actually calls today rather than
introduce a parallel projection path.

| Git-lane operation | Current Sley-backed function set | Design consequence |
|---|---|---|
| Local clone/copy | `copy_local_repo_to_bare` opens both repositories with Sley, copies reachable objects, and applies refs without Git (`crates/git-projection/src/git_core.rs:3988-4004`). | Always stay native for local paths and `file://` full copies. |
| URL clone | `clone_url_to_bare` supports depth, rejects filters, and delegates network transfer to `clone_url_to_bare_via_sley` (`crates/git-projection/src/git_core.rs:4036-4078`); that helper calls `fetch_with_http_client` with `depth` and no filter (`crates/git-projection/src/git_core.rs:4113-4167`). | Depth-only clone remains a Sley capability. Partial filters and advanced shallow controls are Git-subprocess requirements. |
| Bridge fetch/pull | `GitProjection::fetch` selects branch/note scope (`crates/git-projection/src/git_core.rs:1163-1171`), while URL remotes enter `fetch_network_remote` (`crates/git-projection/src/git_core.rs:1227-1237`). That function calls Sley `fetch_with_http_client` with no depth or filter (`crates/git-projection/src/git_core.rs:4544-4600`). | Keep the existing scope and post-fetch ref publication; replace only the transfer executor when selected. |
| Git-overlay pull | The top-level overlay path calls Sley `fetch_with_http_client` against the authoritative checkout and then reads `ref_updates` before its fast-forward and ingest work (`crates/cli/src/cli/commands/remote/remote_ops.rs:361-429`). | A subprocess fetch must return the same typed ref-update facts without moving the user's branch or worktree. |
| Bridge network push | Heddle reads the remote with Sley `ls_remote_with_http_client` (`crates/git-projection/src/git_core.rs:4637-4659`), feeds those refs into its destination reconcile plan (`crates/git-projection/src/git_core.rs:4661-4673`), and executes `push_actions_with_http_client` (`crates/git-projection/src/git_core.rs:4682-4728`). | Preserve the reconcile plan and exported-ref manifest; translate its planned writes/deletes into Git arguments. |
| Authoritative Git-overlay push | `push_authoritative_git_refs` derives the served frontier and scope (`crates/git-projection/src/git_core.rs:4748-4778`), lists the remote and plans reconciliation (`crates/git-projection/src/git_core.rs:4780-4818`), then calls Sley `push_actions_with_http_client` (`crates/git-projection/src/git_core.rs:4829-4869`). | Put backend selection at this call boundary. Do not rebuild overlay push semantics in the subprocess layer. |
| HTTPS credentials | `EmbeddingSafeCredentialProvider` separates embedded and external helpers (`crates/git-projection/src/credential.rs:40-79`) and runs external helper processes with bounded output and timeout (`crates/git-projection/src/credential.rs:120-210`). | Sley remains valid for ordinary credentials. The subprocess is chosen when Git's complete helper/config behavior is explicitly required or detected. |

The existing no-Git contract is intentional, not accidental: the runtime spawn lint
currently has an empty allowlist (`crates/cli/tests/git_process_lint.rs:1-16`) and
the public help says Git is never invoked (`crates/cli-contract/src/cli/help.rs:848-862`).
Implementation must revise both surfaces narrowly and test the new conditional
contract; it must not hide a spawn from the lint.

## 3. Selection policy

### 3.1 Inputs

Every Git-lane network request resolves a `GitTransportPreference` before opening a
socket or starting authentication. The public preference should be
`auto | native | subprocess`, with `auto` as the default. The recommended surface is
a Git-lane-only `--git-transport` option plus a durable per-remote
`remote.<name>.heddleTransport` setting. The CLI option wins over remote config.
Native Heddle transport does not expose or consult this setting.

The resolver consumes only information Heddle can inspect without starting Git:
repository source authority, operation and requested options, remote URL scheme,
Sley's effective Git config snapshot, and an allowlisted set of environment-presence
signals. Capability detection itself must not run `git --version`.

### 3.2 Exact decision order

Use the first matching rule:

1. A Native Heddle repository operation, `heddle://` remote, or non-Git command
   selects the Native Heddle path. Ignore
   `--git-transport` outside an explicit Git lane. Do not search for or start Git.
2. A local filesystem Git remote or full `file://` copy selects Sley. Local-first
   behavior cannot be redirected to a subprocess by `auto`; an explicit
   `subprocess` preference may be rejected as unnecessary rather than weakening the
   offline contract.
3. Explicit `native` selects Sley if the requested operation is in Sley's capability
   set. If an operation requires a capability Sley does not provide, fail before I/O
   with `git_transport_capability_unsupported`; never silently discard an option.
4. Explicit `subprocess` on a network Git-lane operation selects `GitSubprocess` and
   records `selection_reason=explicit`.
5. Under `auto`, select `GitSubprocess` when the request itself requires Git behavior:
   a partial filter; `--deepen`, `--shallow-since`, `--shallow-exclude`,
   `--unshallow`, or `--update-shallow`; exact Git `force-with-lease`; explicit
   submodule recursion/materialization; a strict protocol-v2 request; or a remote
   helper URL understood by installed Git rather than Sley.
6. Under `auto`, select `GitSubprocess` when effective remote configuration exposes a
   recognized Git-owned integration: an HTTPS credential helper chain, an HTTP(S)
   proxy, a per-remote proxy, `core.sshCommand`, `GIT_SSH`/`GIT_SSH_COMMAND`, or an
   SSH remote with `SSH_AUTH_SOCK` present. The last condition covers ordinary agents
   and agents backed by smart cards/security keys without trying to identify hardware
   behind the socket.
7. Otherwise select Sley and record `selection_reason=default_native`.

Depth-only clone is deliberately absent from rule 5 because the current Sley clone
already passes `depth` into `FetchOptions` (`crates/git-projection/src/git_core.rs:4153-4163`).
Likewise, Heddle's existing generic `--force` does not itself select Git: the current
reconcile planner already computes expected old values and forced writes before
transport (`crates/git-projection/src/git_core.rs:4661-4718`). Only a new request for
Git's exact `force-with-lease` contract does.

Selection is one-shot. An authentication, proxy, negotiation, fetch, or push failure
from Sley is returned as a Sley failure with advice to retry explicitly through
`--git-transport=subprocess` when applicable. Heddle must never begin a second
transport automatically. This prevents duplicate prompts, duplicate pack traffic,
and—most importantly—an ambiguous second push after the first server may already have
moved some refs.

### 3.3 Selection examples

| Request | `auto` result | Reason |
|---|---|---|
| Native Heddle push to `heddle://…` with no Git installed | Native Heddle | Git is outside the lane and is never probed. |
| Git-overlay full fetch over ordinary HTTPS, no helper/proxy signal | Sley | Baseline capability is present. |
| Git-overlay HTTPS fetch with `credential.helper` configured | `GitSubprocess` | User Git owns helper orchestration. |
| SSH push with `SSH_AUTH_SOCK` set | `GitSubprocess` | Preserve agent/smart-card behavior. |
| Network clone with only `--depth=1` | Sley | Current Sley supports depth. |
| Fetch with `--filter=blob:none` or `--deepen=20` | `GitSubprocess` | Git owns partial/advanced-shallow semantics. |
| Push with an exact expected remote OID | `GitSubprocess` | Execute Git's reference lease contract. |
| Ordinary local-path fetch | Sley | Offline local copy remains subprocess-free. |

## 4. `GitSubprocess` execution plan

### 4.1 Backend boundary and repository target

The backend implements the same three transport outcomes used above: advertise refs,
fetch objects plus typed ref updates, and execute a precomputed push plan. It does not
offer object parsing, revision walking, merge, index, checkout, or projection APIs.

For Git Overlay, run against the checkout's authoritative common Git directory so
Git and Sley see the same packs, shallow boundary, promisor configuration, and refs.
For explicit Native Heddle Git bridge work, run against the Bridge Mirror. Never
create a hidden second clone merely to use installed Git.

Every operation gets a private namespace under `refs/heddle/transport/<operation-id>/`.
Fetch maps requested remote refs into this namespace; push may create private source
refs for planned object IDs. Sley snapshots and validates those refs before and after
the subprocess, publishes only the refs approved by the existing Heddle planner, and
removes private refs at completion or next repository startup. Downloaded objects are
content-addressed and may remain after a failed operation; user branches, tags, notes,
index, and worktree must not move merely because transport ran.

### 4.2 Lazy startup and version gate

Resolve the configured Git executable and run `git --version` only after selection.
Parse vendor suffixes after the numeric major/minor/patch and require at least 2.41.
Cache the result for the process, keyed by resolved executable path. The 2.41 floor
is tied to the supported `git fetch --porcelain` contract, not to native Heddle.

Failure occurs before staging refs or network I/O and names the operation, selection
reason, executable path/search result, detected version when any, required version,
and remedies. Examples of remedies are installing Git 2.41+, choosing
`--git-transport=native` for a Sley-supported request, or removing the Git-only
option. Do not emit this error during global CLI startup, repository discovery,
status, local capture, native clone, native pull/push, or any Sley-selected Git
operation.

### 4.3 Deterministic environment

Start the child with an empty environment. Populate a platform-specific allowlist,
then pin Heddle-owned values. This both prevents ambient cloud/Heddle tokens from
leaking into configured helpers and preserves the user integrations that motivated
the backend.

The default Unix allowlist is `PATH`, `HOME`, `XDG_CONFIG_HOME`, `USER`, `LOGNAME`,
`TMPDIR`, `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE`,
`DISPLAY`, `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`,
`GIT_ASKPASS`, `GIT_SSH`, `GIT_SSH_COMMAND`, `GIT_SSH_VARIANT`, `HTTP_PROXY`,
`HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, and their lowercase proxy forms. Windows
adds the standard executable/config discovery variables such as `SystemRoot`,
`ComSpec`, `PATHEXT`, `USERPROFILE`, `HOMEDRIVE`, `HOMEPATH`, `APPDATA`, and
`LOCALAPPDATA`. Any additional credential-helper variable requires explicit
per-remote opt-in by exact name; wildcard pass-through and Heddle/cloud credential
defaults are forbidden.

Pin `LC_ALL=C`, `LANG=C`, `GIT_PAGER=cat`, `PAGER=cat`, `GIT_EDITOR=true`,
`GIT_SEQUENCE_EDITOR=true`, color off, and `GIT_TERMINAL_PROMPT` according to the
CLI's interactive policy. Strip inherited `GIT_DIR`, `GIT_WORK_TREE`,
`GIT_INDEX_FILE`, object-directory/alternate/namespace/replace-ref controls,
`GIT_CONFIG_*` injection variables, `GIT_PROTOCOL`, and all `GIT_TRACE*` variables.
Pass the repository location as an explicit command argument and pin
`protocol.version=2`; Git itself is responsible for translating that preference to
the HTTPS header, SSH environment, or other transport negotiation.

Use the normal system, user, XDG, and repository Git config stack derived from the
allowlisted home/config directories. That is required for credential helpers,
proxies, URL rewrites, SSH commands, and certificate policy. Heddle's pinned parser
and safety options override conflicting output/color/pager settings, but it must not
replace the user's authentication and network policy with `/dev/null` config.

### 4.4 Fetch porcelain and ref publication

Run `git fetch --porcelain` with explicit source-to-private-destination refspecs,
`--no-write-fetch-head`, and no implicit pruning or tag following. Pass shallow or
filter arguments only from the typed request; do not allow config to widen the ref
scope. The supported porcelain grammar is Git's documented four-field fetch record:
status flag, old object ID, new object ID, and local ref. Parse stdout as bytes,
accept only documented flags, require full-width object IDs for the repository's
object format, reject unknown/malformed records, and require exactly one consistent
terminal record for every private destination the request expects. See the upstream
[fetch porcelain documentation](https://git-scm.com/docs/git-fetch#Documentation/git-fetch.txt---porcelain).

Drain stderr concurrently. Git has already demultiplexed protocol sideband progress,
warnings, and remote messages onto that stream; see §4.6 for capture and rendering.
After a zero exit and complete parse, refresh Sley's object view, verify each reported
object/ref, and pass typed updates into the same fast-forward, ingest, tracking-ref,
and note-ref logic used by the Sley outcome. A nonzero exit, malformed stdout, missing
record, or Sley postcondition failure prevents publication.

Git documents `fetch --porcelain` as incompatible with recursive submodule fetching.
The backend therefore always fetches the superproject with recursion disabled. An
explicit submodule request is a second phase: enumerate Gitlinks from the fetched
superproject tree, then fetch already-populated submodules individually through the
same porcelain runner. Initializing a newly introduced submodule is a separate,
explicit worktree mutation and never occurs during ordinary `heddle fetch` or
`heddle pull`.

### 4.5 Advertisement, push porcelain, and leases

Advertisement uses `git ls-remote --refs --symref` and its tab-delimited machine
output. Heddle still filters the advertised map and runs its existing destination
reconcile planner. The subprocess receives only that immutable plan.

For each planned write, create or validate a private local source ref and emit an
exact source-to-destination refspec. For each delete, emit an exact empty-source
refspec. Add `--force-with-lease=<destination>:<expected-old>` for every existing
destination and an empty expected value for a planned creation. This makes the
planner's observed old value a server-enforced compare-and-swap even when the write
is a normal fast-forward; a forced non-fast-forward is allowed only when both the
plan authorizes it and the lease still matches. See Git's
[force-with-lease contract](https://git-scm.com/docs/git-push#Documentation/git-push.txt---no-force-with-lease).

Run `git push --porcelain` and parse only its tab-delimited status records: flag,
source/destination pair, and summary/reason. Match records against the exact plan,
classify new/fast-forward/forced/delete/up-to-date/rejected statuses, and reject
duplicates, unknown destinations, or missing terminal records. The process must have
a zero exit and every planned action must be acknowledged before Heddle writes its
exported-ref manifest. On a partial server update, report the per-ref facts, leave the
manifest unchanged, and let the next attempt re-advertise rather than guessing. The
format is specified by the upstream
[push porcelain documentation](https://git-scm.com/docs/git-push#_output).

Do not parse localized human summaries to determine success. Locale pinning improves
diagnostics and tests, but correctness comes from porcelain fields, exit status, the
precomputed plan, and Sley postcondition reads.

### 4.6 Sideband, prompts, cancellation, and secrets

Drain stdout and stderr concurrently to avoid child deadlock. Keep parser stdout
separate from diagnostic stderr. Bound in-memory data and spool overflow to a
mode-`0600` operation file under Heddle's temporary directory; retain only a bounded,
redacted diagnostic tail after failure and delete successful-operation spools.

In human interactive mode, forward sanitized progress and remote sideband lines to
stderr and allow the controlling terminal/askpass path selected by Git. In
noninteractive or machine mode, set terminal prompting off; keep stdout exclusively
for Heddle's machine envelope and attach bounded sideband to structured diagnostics.
Never echo credential-helper requests/responses, URLs with userinfo, authorization
headers, or environment values.

Cancellation terminates the child process group, waits for it, drains both streams,
removes private refs with Sley, and reports whether remote state may have changed. A
cancelled fetch cannot publish Heddle refs. A cancelled push is explicitly
indeterminate until Heddle re-advertises the remote, because the server may have
accepted actions before local cancellation.

### 4.7 Shallow and partial state

Git owns the syntax and wire behavior for advanced shallow and partial requests, but
backend selection does not weaken Heddle's completeness invariants. Current explicit
Git ingest rejects a source containing a `shallow` file
(`crates/git-projection/src/git_ingest.rs:117-128`), and current filtered clone fails
closed because native import expects a complete object graph
(`crates/git-projection/src/git_core.rs:4051-4067`). Those are downstream policy
boundaries, not transport parser limitations.

For shallow operations, Git atomically maintains the Git directory's shallow
boundary; the backend returns that boundary as part of its outcome. Git-overlay
inspection may continue to use the authoritative shallow repository, but conversion
into complete Native Heddle history remains blocked until an explicit unshallow
completes. A failed or missing Git subprocess never edits Heddle's native shallow
state.

For partial operations, persist promisor remote/filter configuration only after the
fetch succeeds, represent missing-object state explicitly in repository verification,
and delegate later hydration to the same selected Git backend. Heddle must not mint a
Native Heddle state whose required blobs are absent. The closure verifier already
defines Gitlinks as outside the superproject closure while treating other missing
objects as an error (`crates/git-projection/src/git_core.rs:4402-4416`); partial-clone
support needs a promisor-aware state ahead of that hard error, not a blanket bypass.

Submodule recursion never changes this rule. A Gitlink target belongs to another
repository. Ordinary fetch preserves its mode and OID without fetching that target.
Explicit materialization operates in the submodule repository and verifies that its
checked-out commit equals the superproject Gitlink.

## 5. Conformance test matrix

Each fixture has a Git CLI reference run, a forced-Sley run, and a forced-subprocess
run. `auto` tests separately assert the selection reason. When Sley intentionally
lacks a requested capability, its conformance result is a typed, pre-I/O unsupported
error and `auto` must select subprocess; it is not an attempt to emulate partial Git
semantics badly. All success comparisons include ref names/OIDs, loose or packed
object bytes reachable from the requested refs, Gitlink modes/targets, Heddle's typed
outcome, and absence of unintended branch/index/worktree movement.

| Capability fixture | What the test asserts |
|---|---|
| HTTPS credential helpers | A loopback smart-HTTP server first returns 401. A deterministic helper records `get`/approval/rejection calls and returns a scoped credential. Git reference and subprocess fetch/push the same refs and object IDs, invoke the configured helper chain in order, approve only after success, reject after auth failure, never print the secret, and produce the same typed auth class. Forced Sley proves its existing helper path for the supported subset; helper shapes marked Git-owned select subprocess in `auto`. |
| SSH agent | A disposable SSH server accepts only a key held by a disposable agent. Git reference and subprocess succeed using `SSH_AUTH_SOCK`, never copy private key material, preserve ref/object parity, and map an empty/wrong agent to authentication failure. `auto` records `ssh_agent`. Forced Sley either passes its supported baseline or fails once without automatic replay. |
| SSH smart-card/security key | A hardware-in-loop job exposes the key only through an agent; a normal CI contract test uses an agent protocol test double to prove socket forwarding and prompt/cancellation behavior. Success requires an agent signature and identical remote refs; missing device, denied touch/PIN, and timeout remain distinguishable and secrets/PINs never enter captured sideband. |
| Proxy | A recording HTTP CONNECT proxy is the only route to the origin. Git reference and subprocess honor Git config and upper/lowercase proxy variables, respect `NO_PROXY`, preserve certificate policy, and emit no proxy credentials. The origin log proves no direct connection. Forced Sley covers only its declared proxy subset; `auto` selects subprocess when a proxy signal is present. |
| Force-with-lease | Advertise remote value A, move it concurrently to B, then attempt a write leased to A. No ref moves, porcelain maps to `lease_rejected`, and the exported-ref manifest is unchanged. Re-advertise B and retry with a B lease; exactly the planned ref moves. Repeat with create, delete, forced rewind, and a multi-ref partial rejection to prove per-ref reporting and retry behavior. |
| Shallow fetch | Compare depth-one clone, deepen, shallow-since/exclude where supported by the fixture, and unshallow. Git reference and subprocess have identical advertised tips and shallow-boundary OIDs; no user ref moves before Heddle publication. Depth-only `auto` remains Sley. Advanced requests select subprocess. Attempted Native Heddle ingest stays blocked until full ancestry exists. |
| Partial fetch | A filter-capable protocol-v2 server runs `blob:none` and `blob:limit`. Git reference and subprocess agree on tips, present/missing object sets, promisor/filter config, and on-demand hydration bytes. Heddle verification reports incomplete/promisor state and refuses complete native-state minting before hydration. Forced Sley returns typed unsupported with zero ref/config mutation. |
| Submodules/Gitlinks | The superproject contains nested Gitlinks whose targets are not in the superproject object database. All paths preserve tree mode `160000` and exact target OIDs; default fetch neither traverses nor initializes submodules. Explicit recursion selects subprocess, fetches each populated child through the runner, initializes new children only with explicit materialization, and checks each checkout HEAD against its Gitlink. A child failure leaves the superproject ref unpublished and identifies the child path. |
| Protocol v2 | A recording HTTP/SSH test server captures capability negotiation. On a v2-capable server, Git reference and subprocess request v2 and receive the same `ls-refs`/fetch capabilities; the Sley baseline is checked independently. A server that falls back to v0/v1 succeeds for an ordinary request but fails with `protocol_version_unavailable` for a strict-v2 request. No trace environment contaminates porcelain stdout. |
| Missing/old Git | Remove Git from the backend search path and separately provide versions 2.40 and 2.41. Native Heddle and Sley-selected Git operations remain byte-for-byte unchanged and never invoke the probe. A selected subprocess fails before ref/object/config mutation with `git_subprocess_unavailable` or `git_subprocess_version_unsupported`; 2.41 proceeds. |
| Porcelain/sideband robustness | Fixture executables emit every documented status, malformed/duplicate/missing records, large stderr, remote warnings, prompts, cancellation, and non-UTF-8 diagnostic bytes. The parser accepts only planned valid records, never treats stderr prose as success, remains bounded, redacts secrets, and marks cancelled push state indeterminate. |

Run the portable matrix on Linux, macOS, and Windows with Git 2.41 and the current
supported Git. Run network fixtures hermetically. Keep actual smart-card coverage as
a scheduled hardware job, with the agent-protocol contract test required on every PR.

## 6. Discoverability and error surface

Keep the existing top-level machine field `transport: "git"`: it distinguishes the
Git lane from Heddle transport today (`crates/core/src/remote.rs:666-709`), and the
schema currently models Git as that single transport discriminator
(`crates/cli-contract/src/cli/commands/schemas.rs:2247-2259`). Add an orthogonal
`git_backend` value (`sley` or `git_subprocess`) and `git_backend_reason` to Git-lane
operation outcomes. Do not relabel subprocess traffic as native Heddle transport.

Human progress should say `using installed Git for <reason>` once, only when the
subprocess is selected. Ordinary Sley output should remain quiet about backend choice.
`heddle remote show` and `heddle verify` may report the configured preference and the
last operation's backend without probing Git. A dedicated diagnostic such as
`heddle doctor git-transport` may explicitly resolve and version-check Git.

Rewrite `heddle help git-dependencies` from its current unconditional statement
(`crates/cli-contract/src/cli/help.rs:848-862`) to the conditional contract:

- Native Heddle and the default Sley Git path never require Git.
- Installed Git 2.41+ is optional and is used only for a selected Git-lane backend.
- The help names selection signals, `--git-transport`, the per-remote preference,
  version diagnostic, and how to force Sley when the requested capability permits it.

Errors are typed and stable in machine output. At minimum distinguish unavailable
binary, unsupported version, native capability unsupported, malformed porcelain,
credential rejected, proxy/connect/TLS failure, SSH agent/device failure, protocol
version unavailable, lease rejected, remote ref rejected, partial fetch incomplete,
and cancelled/indeterminate push. Include the selected backend and reason in every
transport error. Preserve bounded sanitized Git stderr as diagnostic context, never
as the primary classification.

### Missing-Git degradation contract

A missing Git binary changes only an operation whose preflight selected
`GitSubprocess`. That operation fails before mutation and offers a Sley retry only
when the request is actually Sley-capable. Repository discovery, status, capture,
commit/checkpoint, log, diff, verify, local remotes, Native Heddle clone/pull/push,
and ordinary Sley Git-lane operations behave exactly as they do without this feature.
No global warning, startup failure, reduced native capability, or delayed background
probe is allowed.

## 7. Alternatives rejected

**Keep Sley only.** This preserves the current zero-process rule but leaves Heddle
responsible for matching the long tail of installed Git credential, proxy, SSH,
shallow/partial, and protocol behavior. It does not meet the issue's interoperability
goal.

**Use installed Git for every Git remote.** This is simpler selection but makes Git a
de facto prerequisite for Git Overlay, contradicts the local-first contract, bypasses
working Sley paths, and turns a compatibility escape hatch into a new default.

**Retry through Git after any Sley error.** This appears convenient but cannot know
whether a push partially succeeded, repeats authentication and side effects, and
makes machine outcomes nondeterministic. Selection must happen before I/O.

**Let Git own projection/object behavior when selected.** That creates two engines
for object identity, ref policy, and Heddle mapping. The subprocess is deliberately
limited to transport and Git-owned shallow/promisor metadata.

## 8. Open questions for owners

1. **Public preference names:** approve `--git-transport=auto|native|subprocess` and
   `remote.<name>.heddleTransport`, or choose different names before implementation.
   The precedence and selection algorithm above should remain unchanged.
2. **Promisor scope:** should the first implementation permit promisor-backed state
   only inside authoritative Git Overlay, or also introduce promisor-aware Native
   Heddle states? This note recommends Git Overlay only until native state identity,
   offline guarantees, and hydration have their own accepted design.
3. **Explicit environment extensions:** approve exact-name per-remote pass-through
   for unusual credential helpers, or require users to wrap those helpers instead.
   This note recommends exact-name opt-in with secrets redacted from all diagnostics;
   it rejects arbitrary inheritance.
4. **Smart-card release gate:** decide whether hardware-in-loop failures block a
   release or remain scheduled signals. The agent-protocol test should block every PR
   either way.

## 9. Proposed follow-ups

Do not file these from this spike.

1. **Transport abstraction and selection resolver.** Add the three-outcome backend
   seam, typed capability inventory, exact selection order, per-remote/CLI preference,
   backend fields in machine output, lazy capability errors, and explicit updates to
   the runtime Git-process allowlist.
2. **Hardened subprocess runner.** Implement executable resolution and Git 2.41 gate,
   environment allowlist/pinning, process-group cancellation, bounded concurrent
   stdout/stderr capture, porcelain parsers, redaction, and startup cleanup for stale
   operation refs/spools.
3. **Fetch/clone integration and completeness state.** Stage refs, connect fetch
   outcomes to existing overlay/bridge publication, support advanced shallow
   controls, add promisor-aware verification/hydration, and retain the complete-native
   ingest gate.
4. **Push and exact lease integration.** Translate the existing destination reconcile
   plan into exact refspecs and per-ref leases, parse acknowledgements, preserve
   exported-ref manifest rules, and model partial/indeterminate push outcomes.
5. **Explicit submodule materialization.** Add the user surface and two-phase
   superproject/child flow without changing first-class Gitlink identity or making
   ordinary fetch recurse.
6. **Git conformance harness.** Land the hermetic HTTPS/helper, SSH agent, proxy,
   shallow/partial, lease, Gitlink, protocol-v2, missing-version, and malformed-output
   matrix across forced Sley, forced subprocess, `auto`, and Git reference runs; add
   the scheduled smart-card hardware job.

## 10. Acceptance criteria for implementation

- Sley remains the default transport/object implementation and every Native Heddle
  operation passes with Git absent.
- Backend selection is observable, deterministic, complete before network I/O, and
  never changes backend automatically after failure.
- Git 2.41 is checked only after subprocess selection; missing/old Git cannot affect a
  native or Sley-selected operation.
- Fetch/push correctness is derived from porcelain plus Sley postconditions, with
  bounded redacted sideband and no human-prose parsing.
- The existing Heddle reconcile, expected-old, ref ownership, projection mapping, and
  exported-ref manifest rules remain authoritative for both backends.
- Shallow, partial, and submodule behavior is explicit about completeness and never
  silently mints incomplete Native Heddle source history.
- The conformance matrix proves object/ref parity, failure mapping, environment
  isolation, and the missing-Git degradation contract.
