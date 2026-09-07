# Weft clean-cut handoff

Heddle's `codex/agent-native-vcs` branch is based on `origin/main` at
`92a9b0705370d2c04f8bcaf9b7141937085d0ad9`. Weft should cut directly to the
Heddle commit produced from this branch. Do not add dual readers, legacy
writers, or server-side migration behavior.

## Durable data contract

- Use `heddle_object_model::object::HeddleNote` as the only
  `refs/notes/heddle` payload. Encode and decode with `to_json_bytes` and
  `from_json_bytes`; do not keep a Weft-local note struct or parser.
- Use `State::encode_current_msgpack` / `State::decode_current_msgpack` and
  `StateAttachment::encode_current_msgpack` /
  `StateAttachment::decode_current_msgpack` for durable object-transfer bytes.
  These named-field codecs now live beside the models. The Heddle wire layer
  only classifies transport errors around them.
- Repository format 5 is the only accepted local format. Heddle no longer runs
  an open-time schema ledger or any runtime format migration. If Weft persists
  repository-format metadata, reject anything other than the paired format at
  admission and migrate stored data offline before deploying the cut.
- Old repository and storage bytes now report `RepositoryFormatTooOld` and
  `StorageFormatTooOld`. Do not preserve the former “migration required” error
  names in Weft; there is no matching runtime migration operation.
- Raw Git Object Residual closures are the only durable source for imported Git
  objects that Heddle cannot reconstruct byte-for-byte. `.heddle/git` and all
  Bridge Mirror fallback/migration behavior are gone. Reject a mapped lossy
  object without a complete residual closure.

## Storage seam

`ObjectStore` contains durable object behavior only. The following are no
longer trait methods:

- `clear_recent_caches`
- `pack_objects`
- `prune_loose_objects`
- `discard_corrupt_clone_packs`

Filesystem maintenance is inherent on `FsStore`; benchmark-only cache control
is the separate `ObjectCacheControl` Interface. Weft's object backend should
delete no-op implementations and any call sites that treated local pack files
or process caches as remote-store capabilities. Keep sidecar persistence
(redaction and visibility) behind `SidecarStore`.

## Operation and client boundaries

- `verbs::capture(&ExecutionContext, CaptureOptions) -> CaptureReport` owns the
  local capture sequence: safety checks, attribution/session resolution,
  mutation, Git-overlay checkpoint, recovery policy, and semantic report. CLI
  code maps process inputs and renders only. Weft should accept the resulting
  typed action/data contracts and verify authority; it must not reproduce CLI
  argument precedence or output logic.
- The hosted client accepts typed domain inputs, returns typed outcomes/events,
  and routes warnings through a caller-owned `WarningSink`. It no longer
  depends on CLI argument, contract, or rendering crates. Preserve this split
  in Weft: Iroh/protobuf types terminate at the transport Adapter;
  domain/storage code consumes Heddle model types. Do not introduce a second
  view RPC for Tapestry.
- Client roots and human-authorized operations remain client-minted. Weft
  verifies them. Tapestry may perform the current passkey-bound human-key step;
  Heddle and agents may submit the unsigned action for a human to sign and
  submit.

## Confidential runtime state

- `EnvProfileVersion` has no unsigned lifecycle field. Derive its state only
  from verified signed `LifecycleRecord`s and fail closed when none exist.
- The env broker exposes one bounded run operation; it does not expose secret
  unwraps, provider keys, grants, or TTL knobs. The removed TTL bounded only the
  broker call and could not constrain a child's copy of an environment value.
- Raw traces/session transcripts may use this private-state primitive only by
  explicit opt-in. Keep them out of default views and make the PII disclosure
  visible before capture.

## Observation contract

Heddle's structural performance profile now exposes counts for
`network_client_initializations`, `network_streams_opened`,
`network_bytes_sent`, and `network_bytes_received`. Instrument Weft at the
corresponding Iroh transport boundary so cross-client tests can distinguish
connection churn from useful traffic. The old boolean “network client
initialized” signal is gone.

## Cutover order

1. Pin Weft to the paired Heddle/object-model revision.
2. Convert stored notes, states, attachments, env lifecycle data, and lossy Git
   residual closures offline.
3. Update Weft's repository/ObjectStore Adapter and delete local-maintenance
   stubs.
4. Deploy Weft readers and writers together; reject old formats rather than
   silently translating them.
5. Run cross-client fixtures through Heddle and Tapestry against the same Weft
   deployment, asserting byte-identical model round trips and identical view
   responses.

The release gate is one writer and one reader for every durable type, no
`.heddle/git`, no schema-ledger activity during repository open, no CLI types in
Weft storage code, and no server-minted client authority.
