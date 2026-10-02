# Attribution validation on macOS

This checkpoint adds operation-bound attribution, source-format negotiation,
collection-method provenance, and source-pack preservation. Missing causal
identity remains unknown; a current model is never assumed to be the producer.

## Real harness checks (2026-10-02)

| Harness | Version | Tested models | Result |
|---|---|---|---|
| Codex | 0.159.3 | gpt-6-luna, gpt-6-sol | Native hooks, request-reported, two edits in one capture |
| Claude Code | 2.1.287 | claude-opus-5-5, claude-haiku-4-5-20251001 | Native hooks omit model; initial two-model stream-join proof, followed by reusable-collector live Haiku proof |
| OpenCode | 1.18.30 | gpt-6-luna, gpt-6-sol | Plugin hook/event joins; native apply_patch path adapter corrected |
| Hermes | 0.21.0 | None certified | Version-gated adapter included and tested offline; live authentication pending |

All successful captures survived source-pack export, fresh-store import and
complete typed evidence comparison. Models changed across invocations. Claude
subagent identity was observed separately. These are bounded synthetic-source
smokes, not whole-platform certification. No private source or transcripts are
part of this commit.

Eight focused operation-hook tests and eight earlier CLI harness tests passed
on macOS. Synthetic fault injection checked delayed completion, duplicate
hooks and published-ID replay. A Mac /var symlink alias fix canonicalizes the
repository path while retaining SQLite NOFOLLOW and rejecting index symlinks.

The reusable Claude runner and Hermes project plugin are included under
[tools/attribution](../tools/attribution/README.md). The Claude runner completed
a real `claude-haiku-4-5-20251001` edit with response-reported model identity,
exact tool/session IDs, both hook/event_stream origins, and unchanged evidence
through source-pack transfer. The initial two-model proof used the earlier test
bridge; it is not two-model live coverage of the reusable runner. The live smoke
preceded a small final correction labeling harness-version provenance as process;
that correction passed the final offline suite.

Hermes 0.21.0 passed offline two-model capture fixtures and registration against
the installed version. Live access remains blocked by existing provider
authentication; no successful live Hermes inference is claimed. The plugin is
identity-only by default, with explicitly configured absolute local-file binding.
Relative paths, V4A patches, remote backends and shell edits remain unbound.

Seventeen Rust journal tests and thirteen adapter/integration tests pass. They
cover metadata-only late enrichment without resnapshotting files, conflicts,
unknowns, duplicates, bounded input/maps, path escapes, explicit prompt forwarding
and timeout cleanup. The shared collection boundary rejects raw payload bags.
No global hook/plugin configuration or persistent telemetry exporter is installed.

Generic telemetry/proxy collectors, full backend rollout, in-session model
switching, live Hermes and broader subagent coverage remain unverified. Only
macOS was exercised for the POSIX collector runner; Windows is unsupported.

The API dependency is pinned to the companion feature commit rather than a
new registry release. Native format writes require explicit capability
negotiation. The API feature branch is based on alpha.16; historical state
bytes and existing protobuf field numbers remain unchanged.

## Reproduce the branch checks

The companion API commit is
`1b97aa7951441814b2c74eeb8b4defad88f976b1` in `HeddleCo/api`.
It passed 226 Rust tests, five TypeScript attribution tests, protobuf lint/format,
and three Python attention-contract tests. No registry package was published.
The Heddle dependency and lockfile use that Git revision directly.

```sh
export HEDDLE_HOME="$(mktemp -d)"
cargo build --locked -p heddle-cli --no-default-features \
  --features git-overlay,native,local,semantic,zstd,client --bin heddle
cargo test --locked -p heddle-agent-relay --lib operation_hook::tests
cargo test --locked -p heddle-cli --no-default-features \
  --features git-overlay,native,local,semantic,zstd,client --lib harness::tests
cargo test --locked -p heddle-verbs --lib save::attribution_tests
cargo test --locked -p heddle-verbs --lib operation_attribution
HEDDLE_COLLECTOR_TEST_BINARY="$PWD/target/debug/heddle" \
  python3 -m unittest discover -s tools/attribution -p 'test_*.py'
cargo fmt --all --check
```

The alpha.16 reconciliation initializes newly added optional display/search,
provider-ref and thread-lifecycle fields with protobuf defaults; it does not
manufacture metadata. Source-only publication declares no semantic indexes.
Mount was excluded from these Mac checks. Existing disabled-mount/dead-code
warnings are not evidence of a warning-clean full workspace clippy run.

The first restricted Mac CLI rerun could not open the default user database.
Rerunning with an isolated HEDDLE_HOME avoids touching user configuration.
