# heddle#2010 consumer resolution and verification

API alpha.43 uses commit `4a96c6d803962766fc4733baa7f9e8846cc9314f` and exact version pins. Repository format v6 is a rebuild-only cutover: see [preservation and rebuild instructions](../migrations/ref-names-v6.md).

The table covers every Heddle row in the supplied alpha.43 consumer impact report. Site line numbers are the report’s original audit locations, not the new source positions. “Retained” means the existing representation already preserves exact bytes and needs no compatibility shim.

## Validation and import projection

| Audited site | Resolution |
| --- | --- |
| [Cargo.toml](../../Cargo.toml) (audit 56, 224) | API and crates.io patch pinned to alpha.43 / 4a96c6d803962766fc4733baa7f9e8846cc9314f; lock updated. |
| [crates/capability-verifier/Cargo.toml](../../crates/capability-verifier/Cargo.toml) (audit 32) | Exact alpha.43 pin; WASM dependency tree has no Sley. |
| [crates/biscuit-verifier/Cargo.toml](../../crates/biscuit-verifier/Cargo.toml) (audit 22, 37) | Both exact alpha.43 pins updated; target gates retained. |
| [crates/hosted-client/src/hosted_runtime/hosted/import_source.rs](../../crates/hosted-client/src/hosted_runtime/hosted/import_source.rs) (audit 149) | Git branches use ThreadName::from_git_branch (Sley branch type + HEAD guard); signed scopes and manifests still use API validation. |
| [crates/cli/src/cli/commands/remote/mod.rs](../../crates/cli/src/cli/commands/remote/mod.rs) (audit 811) | Structured ConfigEdit subsection replaces dotted branch key; external current-branch and remote-tracking refs retain original Git strings; full remote-name gate retained. |
| [crates/verbs/src/agent_fanout.rs](../../crates/verbs/src/agent_fanout.rs) (audit 276) | ThreadId uses Sley branch syntax; preserve exact thread/HEAD text, quote generated commands; bracket the shared percent encoding in lane descriptors containing equals. |
| [crates/cli/src/cli/commands/agent_cmd.rs](../../crates/cli/src/cli/commands/agent_cmd.rs) (audit 216, 820, 896) | Shared ThreadId admission; quote breadcrumbs; reverse native reserved mapping before constructing Git branch refs. |
| [crates/cli/src/cli/commands/thread.rs](../../crates/cli/src/cli/commands/thread.rs) (audit 2734) | Rename shares Sley-backed ThreadId and v6 storage; advice describes Git branch syntax. |
| [crates/verbs/src/thread_plan.rs](../../crates/verbs/src/thread_plan.rs) (audit 167) | Shared ThreadId admission; removes shell alphabet assumption. |
| [crates/objects/src/thread_record.rs](../../crates/objects/src/thread_record.rs) (audit 23, 101, 81) | Sley BranchRefNameBuf validation + exact HEAD refusal; strict deserialization; full-ref byte limit and native reservation retained. |
| [crates/ingest/src/ref_emit.rs](../../crates/ingest/src/ref_emit.rs) (audit 118) | Validate/map branches and tags with typed incoming-Git constructors before native publication. |
| [crates/object-model/src/object/thread_replication/git_import_graph.rs](../../crates/object-model/src/object/thread_replication/git_import_graph.rs) (audit 136, 177, 265) | Strict UTF-8 admission, literal U+FFFD accepted; Sley syntax and escaped reserved mapping; original signed full refs retained. |
| [crates/ingest/src/git_walk.rs](../../crates/ingest/src/git_walk.rs) (audit 466, 929) | Strict preflight before Sley enumeration; full refs and short branches use Sley; accept U+FFFD; use enumerated OIDs for long packed refs. |
| [crates/ingest/src/importer.rs](../../crates/ingest/src/importer.rs) (audit 315) | Inherits typed classifier; diagnostics may decode lossily but identity never does. Nine round-trip regressions added. |
| [crates/object-model/src/object/identifiers.rs](../../crates/object-model/src/object/identifiers.rs) (audit 120, 140, 153) | Adds from_git_branch/from_git_tag; exact byte limits, Git syntax and collision-free reserved import mapping; native/synthetic separation retained. |
| [crates/refs/src/refs/name.rs](../../crates/refs/src/refs/name.rs) (audit 13, 53) | Full-name Sley syntax; branch and tag storage boundaries enforce their own 1024-byte full-ref limits; BranchRefNameBuf preserves @ and rejects HEAD/leading dash. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 215, 229, 264, 267) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_sync.rs](../../crates/git-projection/src/git_sync.rs) (audit 74, 131) | Validate/map incoming native identities; reverse mapping for outgoing Git refs. |
| [crates/repo/src/git_ref_name.rs](../../crates/repo/src/git_ref_name.rs) (audit 94, 176) | Namespace classifier retained; new strict UTF-8 preflight at external enumeration boundaries, including dangling symbolic targets. Literal namespace prefixes preserve original external names. |
| [crates/hosted-client/src/hosted_runtime/hosted/import_source.rs](../../crates/hosted-client/src/hosted_runtime/hosted/import_source.rs) (audit 40, 190) | Git branches use ThreadName::from_git_branch (Sley branch type + HEAD guard); signed scopes and manifests still use API validation. |
| [crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs](../../crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs) (audit 67, 317) | Existing API discovery and manifest checks inherit alpha.43; exact signed source bytes retained. |
| [crates/capability-verifier/src/import_delegation.rs](../../crates/capability-verifier/src/import_delegation.rs) (audit 217) | Existing API delegation verification inherits alpha.43; no Sley dependency. |
| [crates/repo/src/thread_replication/delegated_import.rs](../../crates/repo/src/thread_replication/delegated_import.rs) (audit 666, 685) | Existing API scope, bundle and signature verification inherits alpha.43 unchanged. |

## Encodings and command output

| Audited site | Resolution |
| --- | --- |
| [crates/repo/src/thread_manifest.rs](../../crates/repo/src/thread_manifest.rs) (audit 183, 225, 249) | Shared reversible UTF-8 escaping, case-distinct ASCII, <=182-byte chunks and entry terminal; scanners stop before checkout. No v5 fallback. |
| [crates/refs/src/refs/refs_storage.rs](../../crates/refs/src/refs/refs_storage.rs) (audit 99, 110, 114, 125, 129, 278) | Every thread/marker/remote path uses bounded canonical encoding; directory enumeration strictly reverses encoding, preserving case and bytes. Tag storage uses the tag namespace length. |
| [crates/refs/src/refs/refs_storage.rs](../../crates/refs/src/refs/refs_storage.rs) (audit 255) | Every thread/marker/remote path uses bounded canonical encoding; directory enumeration strictly reverses encoding, preserving case and bytes. Tag storage uses the tag namespace length. |
| [crates/refs/src/refs/head.rs](../../crates/refs/src/refs/head.rs) (audit 20, 34) | Strip only one LF/CRLF; strict branch validation after reserved mapping reversal; NBSP round trip tested. |
| [crates/object-model/src/refs.rs](../../crates/object-model/src/refs.rs) (audit 28, 53, 61) | Native packed-ref parser preserves Unicode whitespace; ASCII framing remains unambiguous for Git-valid names. |
| [crates/refs/src/refs/refs_manager.rs](../../crates/refs/src/refs/refs_manager.rs) (audit 290, 294) | Snapshot witness retains exact UTF-8 and literal LF record delimiters; Git syntax excludes delimiter bytes. |
| [crates/refs/src/refs/refs_types.rs](../../crates/refs/src/refs/refs_types.rs) (audit 34) | HEAD display retains exact identity; Git admission excludes ASCII control characters; JSON remains additive. |
| [crates/refs/src/refs/ref_summary_index.rs](../../crates/refs/src/refs/ref_summary_index.rs) (audit 158, 212, 221, 236) | Literal TAB/LF parsing retained; scans decode v6 storage including remote names; exact Unicode listing/fetch tested. |
| [crates/object-model/src/object/frontier_ref.rs](../../crates/object-model/src/object/frontier_ref.rs) (audit 78, 116, 125) | Typed synthetic namespace and last-slash / fixed ChangeId parsing retained; no alphabet tightening. |
| [crates/refs/src/refs/refs_synthetic.rs](../../crates/refs/src/refs/refs_synthetic.rs) (audit 25) | Replace unbounded flat hex filename with shared bounded reversible storage; synthetic type remains distinct. |
| [crates/refs/src/refs/facet.rs](../../crates/refs/src/refs/facet.rs) (audit 97, 102) | Fixed namespace prefixes and typed constructors retained; no shell interpolation or name normalization. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 291, 299) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_residual.rs](../../crates/git-projection/src/git_residual.rs) (audit 321, 330, 342) | Residual note names remain exact JSON map keys, never OS paths. |
| [crates/repo/src/thread_advice.rs](../../crates/repo/src/thread_advice.rs) (audit 27, 84, 96) | Reuse shell_quote/thread_flag/positional helpers; regressions exercise raw renderer inputs without bypassing strict deserialization. |
| [crates/cli/src/cli/commands/fsck.rs](../../crates/cli/src/cli/commands/fsck.rs) (audit 350) | Existing shell_quote repair breadcrumb retained. |
| [crates/cli-contract/src/cli/commands/advice.rs](../../crates/cli-contract/src/cli/commands/advice.rs) (audit 1084) | Reuse the canonical import-command renderer to quote the exact branch argument. |
| [crates/cli/src/cli/commands/agent_presence.rs](../../crates/cli/src/cli/commands/agent_presence.rs) (audit 380) | Shell-quote reserve thread argument. |
| [crates/cli/src/cli/commands/clone.rs](../../crates/cli/src/cli/commands/clone.rs) (audit 989) | Shell-quote recovery values; select mapped native identity but configure/write Git refs and HEAD with original name. Structured origin config retained. |
| [crates/cli/src/cli/commands/workflow.rs](../../crates/cli/src/cli/commands/workflow.rs) (audit 3285) | Land breadcrumb uses shared thread_flag renderer. |
| [crates/verbs/src/merge/mod.rs](../../crates/verbs/src/merge/mod.rs) (audit 1543) | Land breadcrumb uses shared thread_flag renderer. |
| [crates/cli/src/cli/commands/remote/collaboration_recovery.rs](../../crates/cli/src/cli/commands/remote/collaboration_recovery.rs) (audit 73) | Existing shell-quoted recovery target retained. |
| [crates/cli/src/cli/commands/remote/mod.rs](../../crates/cli/src/cli/commands/remote/mod.rs) (audit 845, 846) | Structured ConfigEdit subsection replaces dotted branch key; external current-branch and remote-tracking refs retain original Git strings; full remote-name gate retained. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 1947, 1948) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |

## Other Git namespace embeddings

| Audited site | Resolution |
| --- | --- |
| [crates/cli/src/cli/commands/agent_cmd.rs](../../crates/cli/src/cli/commands/agent_cmd.rs) (audit 1122) | Shared ThreadId admission; quote breadcrumbs; reverse native reserved mapping before constructing Git branch refs. |
| [crates/cli/src/cli/commands/clone.rs](../../crates/cli/src/cli/commands/clone.rs) (audit 743) | Shell-quote recovery values; select mapped native identity but configure/write Git refs and HEAD with original name. Structured origin config retained. |
| [crates/cli/src/cli/commands/clone.rs](../../crates/cli/src/cli/commands/clone.rs) (audit 772) | Shell-quote recovery values; select mapped native identity but configure/write Git refs and HEAD with original name. Structured origin config retained. |
| [crates/cli/src/cli/commands/clone.rs](../../crates/cli/src/cli/commands/clone.rs) (audit 797) | Shell-quote recovery values; select mapped native identity but configure/write Git refs and HEAD with original name. Structured origin config retained. |
| [crates/cli/src/cli/commands/clone.rs](../../crates/cli/src/cli/commands/clone.rs) (audit 1175) | Shell-quote recovery values; select mapped native identity but configure/write Git refs and HEAD with original name. Structured origin config retained. |
| [crates/cli/src/cli/commands/clone.rs](../../crates/cli/src/cli/commands/clone.rs) (audit 2699) | Shell-quote recovery values; select mapped native identity but configure/write Git refs and HEAD with original name. Structured origin config retained. |
| [crates/cli/src/cli/commands/remote/mod.rs](../../crates/cli/src/cli/commands/remote/mod.rs) (audit 760) | Structured ConfigEdit subsection replaces dotted branch key; external current-branch and remote-tracking refs retain original Git strings; full remote-name gate retained. |
| [crates/cli/src/cli/commands/remote/mod.rs](../../crates/cli/src/cli/commands/remote/mod.rs) (audit 811) | Structured ConfigEdit subsection replaces dotted branch key; external current-branch and remote-tracking refs retain original Git strings; full remote-name gate retained. |
| [crates/cli/src/cli/commands/remote/mod.rs](../../crates/cli/src/cli/commands/remote/mod.rs) (audit 845) | Structured ConfigEdit subsection replaces dotted branch key; external current-branch and remote-tracking refs retain original Git strings; full remote-name gate retained. |
| [crates/cli/src/cli/commands/remote/mod.rs](../../crates/cli/src/cli/commands/remote/mod.rs) (audit 846) | Structured ConfigEdit subsection replaces dotted branch key; external current-branch and remote-tracking refs retain original Git strings; full remote-name gate retained. |
| [crates/cli/src/cli/commands/remote/mod.rs](../../crates/cli/src/cli/commands/remote/mod.rs) (audit 851) | Structured ConfigEdit subsection replaces dotted branch key; external current-branch and remote-tracking refs retain original Git strings; full remote-name gate retained. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 446) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 447) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 765) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 792) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 1119) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 1947) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 1948) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/remote/remote_ops.rs](../../crates/cli/src/cli/commands/remote/remote_ops.rs) (audit 1953) | Structured ConfigEdit subsection replaces dotted branch key; Git pull maps original local branch to native identity before lookup/publication. Transport refs remain original external names. |
| [crates/cli/src/cli/commands/undo_apply/mod.rs](../../crates/cli/src/cli/commands/undo_apply/mod.rs) (audit 1128) | Undo refs and HEAD use stored external Git branch names; separate Sley arguments and fixed namespace prefixes retain exact identity. |
| [crates/cli/src/cli/commands/undo_apply/mod.rs](../../crates/cli/src/cli/commands/undo_apply/mod.rs) (audit 1153) | Undo refs and HEAD use stored external Git branch names; separate Sley arguments and fixed namespace prefixes retain exact identity. |
| [crates/cli/src/cli/commands/undo_apply/mod.rs](../../crates/cli/src/cli/commands/undo_apply/mod.rs) (audit 1191) | Undo refs and HEAD use stored external Git branch names; separate Sley arguments and fixed namespace prefixes retain exact identity. |
| [crates/cli/src/cli/commands/undo_apply/mod.rs](../../crates/cli/src/cli/commands/undo_apply/mod.rs) (audit 1227) | Undo refs and HEAD use stored external Git branch names; separate Sley arguments and fixed namespace prefixes retain exact identity. |
| [crates/cli/src/cli/commands/undo_apply/mod.rs](../../crates/cli/src/cli/commands/undo_apply/mod.rs) (audit 1272) | Undo refs and HEAD use stored external Git branch names; separate Sley arguments and fixed namespace prefixes retain exact identity. |
| [crates/cli/src/cli/commands/undo_apply/mod.rs](../../crates/cli/src/cli/commands/undo_apply/mod.rs) (audit 1369) | Undo refs and HEAD use stored external Git branch names; separate Sley arguments and fixed namespace prefixes retain exact identity. |
| [crates/cli/src/cli/commands/undo_apply/mod.rs](../../crates/cli/src/cli/commands/undo_apply/mod.rs) (audit 1385) | Undo refs and HEAD use stored external Git branch names; separate Sley arguments and fixed namespace prefixes retain exact identity. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 650) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 3306) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 3313) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 3373) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 3389) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 3468) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_core.rs](../../crates/git-projection/src/git_core.rs) (audit 3907) | Literal full refs use Sley (one wildcard handled explicitly); strict UTF-8 preflight; exact ownership-record parsing; checkout projection reverses mapping. Existing external-Git names remain separate argv/refspec strings. |
| [crates/git-projection/src/git_export.rs](../../crates/git-projection/src/git_export.rs) (audit 856) | Reverse native mapping for branch/tag refs and output; map managed-record tag names back to native before lookup; ownership framing preserves bytes. |
| [crates/git-projection/src/git_export.rs](../../crates/git-projection/src/git_export.rs) (audit 967) | Reverse native mapping for branch/tag refs and output; map managed-record tag names back to native before lookup; ownership framing preserves bytes. |
| [crates/git-projection/src/git_export.rs](../../crates/git-projection/src/git_export.rs) (audit 1453) | Reverse native mapping for branch/tag refs and output; map managed-record tag names back to native before lookup; ownership framing preserves bytes. |
| [crates/git-projection/src/git_export.rs](../../crates/git-projection/src/git_export.rs) (audit 1463) | Reverse native mapping for branch/tag refs and output; map managed-record tag names back to native before lookup; ownership framing preserves bytes. |
| [crates/git-projection/src/git_sync.rs](../../crates/git-projection/src/git_sync.rs) (audit 157) | Validate/map incoming native identities; reverse mapping for outgoing Git refs. |
| [crates/git-projection/src/git_sync.rs](../../crates/git-projection/src/git_sync.rs) (audit 205) | Validate/map incoming native identities; reverse mapping for outgoing Git refs. |
| [crates/git-projection/src/git_sync.rs](../../crates/git-projection/src/git_sync.rs) (audit 222) | Validate/map incoming native identities; reverse mapping for outgoing Git refs. |
| [crates/ingest/src/git_walk.rs](../../crates/ingest/src/git_walk.rs) (audit 325) | Strict preflight before Sley enumeration; full refs and short branches use Sley; accept U+FFFD; use enumerated OIDs for long packed refs. |
| [crates/ingest/src/git_walk.rs](../../crates/ingest/src/git_walk.rs) (audit 326) | Strict preflight before Sley enumeration; full refs and short branches use Sley; accept U+FFFD; use enumerated OIDs for long packed refs. |
| [crates/repo/src/git_ref_name.rs](../../crates/repo/src/git_ref_name.rs) (audit 209) | Namespace classifier retained; new strict UTF-8 preflight at external enumeration boundaries, including dangling symbolic targets. Literal namespace prefixes preserve original external names. |
| [crates/repo/src/git_ref_name.rs](../../crates/repo/src/git_ref_name.rs) (audit 214) | Namespace classifier retained; new strict UTF-8 preflight at external enumeration boundaries, including dangling symbolic targets. Literal namespace prefixes preserve original external names. |
| [crates/repo/src/git_ref_name.rs](../../crates/repo/src/git_ref_name.rs) (audit 223) | Namespace classifier retained; new strict UTF-8 preflight at external enumeration boundaries, including dangling symbolic targets. Literal namespace prefixes preserve original external names. |
| [crates/repo/src/git_ref_name.rs](../../crates/repo/src/git_ref_name.rs) (audit 229) | Namespace classifier retained; new strict UTF-8 preflight at external enumeration boundaries, including dangling symbolic targets. Literal namespace prefixes preserve original external names. |
| [crates/repo/src/overlay.rs](../../crates/repo/src/overlay.rs) (audit 543) | Map Git branch/tag/HEAD identities into native names; reverse mapping for native lookups; Heddle ref framing strips only LF/CRLF. |
| [crates/repo/src/overlay.rs](../../crates/repo/src/overlay.rs) (audit 623) | Map Git branch/tag/HEAD identities into native names; reverse mapping for native lookups; Heddle ref framing strips only LF/CRLF. |
| [crates/verbs/src/fsck/git_projection.rs](../../crates/verbs/src/fsck/git_projection.rs) (audit 125) | Reverse native import mapping when comparing projected Git refs. |
| [crates/verbs/src/status.rs](../../crates/verbs/src/status.rs) (audit 3336) | Current branch/upstream tracking names come from Git, remain original external strings; literal namespace prefixes and structured Sley calls retained. |
| [crates/verbs/src/status.rs](../../crates/verbs/src/status.rs) (audit 3349) | Current branch/upstream tracking names come from Git, remain original external strings; literal namespace prefixes and structured Sley calls retained. |
| [crates/verbs/src/status.rs](../../crates/verbs/src/status.rs) (audit 3380) | Current branch/upstream tracking names come from Git, remain original external strings; literal namespace prefixes and structured Sley calls retained. |

## Additional consumers found during implementation

| Site | Resolution |
| --- | --- |
| `ingest/oplog_emit.rs` | Apply the same import validation/mapping during canonical operation replay as during direct ref emission. |
| `repo/thread_record_store.rs`, `repo/timeline_store.rs` | Shared bounded encoding for persisted thread records, recovery records and materialization locks. |
| CLI worktree target validation | Recognizes the encoded metadata namespace and canonical checkout leaf; refuses nested existing workspaces. |
| `repo/discovery.rs` | Recognize canonical chunk paths and the `entry` terminal when refusing metadata-less managed mounts. |
| `repo/repo_config.rs`, `object-model/error.rs`, CLI error envelope | v5 fails before opening old stores, without mutation, with preservation and rebuild advice. |
| capability-verifier owner authorization native-verifier manifest | Repin both direct API and patch declarations. |

## Surfaces

The verb and help/advice, human and JSON output, Git import/export/projection through Sley, alpha.43 wire verification, and reverse states (listing, rename/drop, reserved mapping reversal, HEAD, pack and fetch) apply. No new RPC, server authority, verb or flag is added.

## Verification

The baseline is the unmodified integration checkout (`0a222ae6`) with only the
new importer test fixtures inserted. Ordinary source refs are loose; the
1011-byte Unicode branch uses Git packed input. The same fixtures run after
the change. Names already accepted in v5 remain passing controls.

| Name | Importer regression suffix | Baseline | V6 | Real native fetch |
| --- | --- | --- | --- | --- |
| `feat/mcp=timeout` | `equals` | pass | pass | pass |
| `a,b` | `comma` | FAIL | pass | pass |
| `ünicode/ブランチ` | `unicode` | FAIL | pass | pass |
| `@` | `at` | pass | pass | pass |
| `x+y` | `plus` | pass | pass | pass |
| trailing U+00A0 | `nbsp` | FAIL | pass | pass |
| literal U+FFFD | `replacement` | FAIL | pass | pass |
| `界` × 337 (full ref 1022 UTF-8 bytes) | `long` | FAIL | pass | pass |
| `heddle/foo` | `reserved` | FAIL | pass | pass |

Importer names are `importer::tests::refname_round_trip_<suffix>`.
Real fetch names are `import_storage_head_pack_list_fetch_<suffix>` in
[refnames_roundtrip.rs](../../crates/cli/tests/refnames_roundtrip.rs).

Actual baseline output:

```text
test importer::tests::refname_round_trip_long ... FAILED
test importer::tests::refname_round_trip_nbsp ... FAILED
test importer::tests::refname_round_trip_comma ... FAILED
test importer::tests::refname_round_trip_unicode ... FAILED
test importer::tests::refname_round_trip_replacement ... FAILED
test importer::tests::refname_round_trip_reserved ... FAILED
test importer::tests::refname_round_trip_plus ... ok
test importer::tests::refname_round_trip_at ... ok
test importer::tests::refname_round_trip_equals ... ok
test result: FAILED. 3 passed; 6 failed; 0 ignored; 0 measured; 216 filtered out; finished in 0.49s
```

Actual filesystem/import/HEAD/pack/list/fetch output:

```text
test import_storage_head_pack_list_fetch_replacement ... ok
test import_storage_head_pack_list_fetch_at ... ok
test import_storage_head_pack_list_fetch_equals ... ok
test import_storage_head_pack_list_fetch_plus ... ok
test import_storage_head_pack_list_fetch_nbsp ... ok
test import_storage_head_pack_list_fetch_unicode ... ok
test import_storage_head_pack_list_fetch_comma ... ok
test import_storage_head_pack_list_fetch_long ... ok
test import_storage_head_pack_list_fetch_reserved ... ok
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.54s
```

`git_branch_alphabet_and_reserved_mapping_export_exact_refs` additionally
round-trips real branches and tags through Git export, comparing every ref OID,
every reachable object OID and `git fsck --full --strict`.
It includes reserved/literal-escape collisions, case-distinct `CON`/`con`, and
shell punctuation. Its suite output is `18 passed; 0 failed; 1 ignored`.
`long_reserved_import_name_has_bounded_exact_synthetic_storage` proves a
long reserved import name stores and lists independently in both
native thread and synthetic stores. The real fetch reserved-name test also imports and fetches that long name. The final refs suite reports `82 passed; 0 failed;
1 ignored`, including `marker_storage_uses_the_full_tag_ref_byte_limit`.

`git_branch_admission_matches_git_and_rejects_invalid_names` compares accepted
names with Git's branch oracle and refuses `..`, `@{`, `.lock` components,
control characters, leading `-`, exact `HEAD`, `.` and `team:scope`; a source
full ref beyond 1024 bytes is refused.

The dangling-symbolic-target regression adds another direct fail-then-pass
check before Sley enumeration:

```text
test git_ref_name::tests::dangling_git_symbolic_ref_refuses_non_utf8_target_before_enumeration ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 980 filtered out; finished in 0.00s
```

After using symlink metadata before testing whether the target exists:

```text
test git_ref_name::tests::dangling_git_symbolic_ref_refuses_non_utf8_target_before_enumeration ... ok
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 974 filtered out; finished in 0.00s
```

All runs use a new `HEDDLE_HOME`, with `CARGO_TARGET_DIR=/runner/heddleco-build/scratch/heddle-2010-target` and `TMPDIR=/home/scratch`.

The workspace command passed with `RUST_TEST_THREADS=4`: **4,467 passed, 0 failed, 20 ignored across 36 test binaries**. Its first run at the runner's default 16 test threads hit the existing ten-second sender-progress deadline. The same test passed alone and in the complete four-thread rerun, without a source or deadline change.

The first complete CLI nextest run collected 2,099 passes, five failures from old alphabet/path assertions, and a hosted-publication timeout. The five assertions are updated; their reruns pass. The unchanged 1,000-state hosted test passes alone in 281.707 seconds under its original timeout. The final full CLI nextest run is recorded below. The last UTF-8 boundary
change was additionally checked by all seven `git_ref_name::tests`, with Clippy
rerun afterward.


## Gate results

Commands come from `.github/workflows/rust-tests.yml` and the existing freshness
checks. The workspace rerun sets `RUST_TEST_THREADS=4`; CLI nextest uses
`--no-fail-fast` to collect all results. Every run uses the target/temp paths and
fresh home described above.

| Gate | Command | Result |
| --- | --- | --- |
| Clippy | `cargo clippy --locked --workspace --all-targets -- -D warnings -D dead-code` | PASS; rerun after the final UTF-8 boundary fix. |
| Workspace lib/bins | `cargo test --locked --workspace --exclude heddle-cli --lib --bins` | PASS: 4,467 passed, 20 ignored; the final Git-ref boundary module additionally passes all 7 tests. |
| CLI lib/bins | `cargo test --locked -p heddle-cli --lib --bins -- --test-threads=1` | PASS: 500 library tests and 5 binary tests. |
| CLI nextest | `cargo nextest run --locked -p heddle-cli --features client --test-threads 4 --no-fail-fast` | PASS: 2,105 passed, 42 skipped; 314.697s for the 1,000-state hosted case. |
| Hosted-client nextest | `cargo nextest run --locked -p heddle-hosted-client --features client` | PASS: 492 passed, 3 skipped. |
| Docsgen freshness | `cargo run --locked -p heddle-docsgen -- --check` | PASS: `llms.txt / llms-full.txt are up to date`. |
| Agent API schema freshness | `cargo test --locked -p heddle-cli --test agent_api_schema` | PASS: `agent_api_schema_matches_committed_snapshot`. |
| Capability verifier WASM | `cargo build --locked -p heddleco-capability-verifier --target wasm32-unknown-unknown --lib` | PASS. |
| Biscuit verifier WASM | `cargo check --locked -p heddle-biscuit-verifier --target wasm32-unknown-unknown --lib` | PASS. |
| WASM graph | `cargo tree --locked -p heddleco-capability-verifier --target wasm32-unknown-unknown -e normal` | PASS: no Sley dependency. |
| Rust source reachability | `cargo run --locked -p heddle-devtools -- check-rust-source-reachability` | PASS: 1,264 source files in 36 Rust crates. |
| Format / whitespace | Nightly Rustfmt edition 2024 checks on the 58 touched Rust files only; `git diff --check` | PASS. |


Final test output excerpts:

```text
test result: ok. 500 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 176.33s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
Summary [1290.780s] 2105 tests run: 2105 passed (3 slow), 42 skipped
Summary [118.134s] 492 tests run: 492 passed (2 slow), 3 skipped
```

Explicit upstream diagnostics were run with `--ignored` and remain failing:

```text
test importer::tests::refname_packed_git_nbsp_blocked_on_sley_243 ... FAILED
imported branch
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 224 filtered out; finished in 0.61s

test git_nbsp_branch_export_blocked_on_sley_write_validation ... FAILED
[trailing-nbsp-export] failed exports: [("refs/heads/trailing\u{a0}", FailedToSet("git error: invalid format: ref name must not have leading or trailing whitespace"))]
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 18 filtered out; finished in 2.69s
```


## Upstream blockers

The native NBSP chain passes. Explicit ignored regressions preserve two failing Sley boundaries: packed Git input drops U+00A0 through Unicode `trim_end()`, and Git export/write validation rejects trailing Unicode whitespace. Heddle does not bypass either path. The owner-supplied [Sley #243](https://github.com/HeddleCo/sley/issues/243) currently describes a pack cursor, so the upstream tracking reference needs correction.
