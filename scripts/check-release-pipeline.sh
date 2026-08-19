#!/usr/bin/env bash
# Asserter for the binary-release pipeline contract (heddle#56, heddle#1415).
#
# The release pipeline is invoked only on `v*` tag pushes, so we can't
# observe its artifacts in normal CI. Instead we statically verify that
# `.github/workflows/release.yml` declares the contract every downstream
# packaging channel (Homebrew, Scoop, apt) relies on:
#
#   - tag-push trigger
#   - all 5 target triples
#   - tarball/zip packaging for CLI targets, with macOS archives produced by
#     the cask job so Apple binaries are built once
#   - signed/notarized macOS cask DMG
#   - final-DMG app signature verification
#   - stable-tag-only GitHub Packages publication for the generated
#     TypeScript gRPC client
#   - explicit refusal of branch-selected release dry-runs
#   - approval-protected release environment on credentialed jobs
#   - exact commit pins for every external action
#   - sha256 checksums
#   - signing step
#   - GitHub Release upload
#   - stable-only Homebrew manifest PR publication
#
# This is the failing test that drives the red-commit-first DoD.

set -euo pipefail

WF=".github/workflows/release.yml"
GATE_WF=".github/workflows/validate-release-tag.yml"
GATE_USES_PREFIX="HeddleCo/heddle/.github/workflows/validate-release-tag.yml@"
fail=0

err() { echo "::error::$*" >&2; fail=1; }
ok()  { echo "ok: $*"; }

if [[ ! -f "$WF" ]]; then
  err "$WF does not exist"
  echo "::error::Release pipeline not implemented. See heddle#56."
  exit 1
fi

# heddle#1415: the authenticity gate must not be the tagged tree's copy of
# this file. GitHub evaluates release.yml from the tag; it fetches a
# `{owner}/{repo}/...@<sha>` reusable workflow from that SHA. `./` and `$/`
# follow the caller commit and would let a tagged rewrite drop the gate.
GATE_PIN=""
if GATE_USES=$(sed -nE 's/^[[:space:]]*uses:[[:space:]]*(HeddleCo\/heddle\/\.github\/workflows\/validate-release-tag\.yml@[0-9a-f]{40}).*/\1/p' "$WF" | head -n1) \
   && [[ -n "$GATE_USES" ]]; then
  GATE_PIN="${GATE_USES##*@}"
  ok "validate-tag calls $GATE_USES"
else
  err "validate-tag must call ${GATE_USES_PREFIX}<40-char-sha> (not ./, $/, @main, or an inline job)"
fi

if grep -E '^    uses:[[:space:]]*(\./|\$/)' "$WF" >/dev/null; then
  err "release.yml must not call a reusable workflow via ./ or \$/ — those follow the tagged caller commit"
else
  ok "release.yml does not use ./ or \$/ reusable-workflow refs"
fi

if [[ -n "$GATE_PIN" ]]; then
  if ! git cat-file -e "${GATE_PIN}:${GATE_WF}" 2>/dev/null; then
    err "gate pin ${GATE_PIN} does not contain ${GATE_WF}"
  else
    ok "pinned SHA ${GATE_PIN} contains ${GATE_WF}"
    if ! git show "${GATE_PIN}:${GATE_WF}" | cmp -s - "$GATE_WF"; then
      err "working tree ${GATE_WF} differs from pin ${GATE_PIN}; bump the uses SHA in ${WF}"
    else
      ok "working tree ${GATE_WF} matches pinned SHA ${GATE_PIN}"
    fi
  fi
  if git rev-parse --verify --quiet origin/main >/dev/null \
     && git merge-base --is-ancestor HEAD origin/main 2>/dev/null \
     && git merge-base --is-ancestor origin/main HEAD 2>/dev/null; then
    if git merge-base --is-ancestor "$GATE_PIN" origin/main; then
      ok "gate pin ${GATE_PIN} is on origin/main"
    else
      err "gate pin ${GATE_PIN} is not an ancestor of origin/main"
    fi
  elif git merge-base --is-ancestor "$GATE_PIN" HEAD 2>/dev/null; then
    ok "gate pin ${GATE_PIN} is in this history (landing; not yet required to be on origin/main)"
  else
    err "gate pin ${GATE_PIN} is not reachable from HEAD"
  fi
fi

# Gate semantics are read from the pinned object, not the working tree.
# A tagged rewrite of the working copy therefore cannot satisfy these
# checks unless the caller also retargets the pin — and a new pin is a
# reviewable SHA on main, not an unreviewed tag tree.
GATE_SRC="$GATE_WF"
if [[ -n "$GATE_PIN" ]] && git cat-file -e "${GATE_PIN}:${GATE_WF}" 2>/dev/null; then
  GATE_SRC="$(mktemp)"
  git show "${GATE_PIN}:${GATE_WF}" > "$GATE_SRC"
fi

# Tag-push trigger. The contract is strict semver only (vX.Y.Z); RC
# tags route through workflow_dispatch so the publish step can mark
# them prerelease+draft. See validate-tag job for the full rule.
if grep -E "^\s*tags:" "$WF" >/dev/null \
   && grep -E "v\[0-9\]\+\.\[0-9\]\+\.\[0-9\]\+" "$WF" >/dev/null; then
  ok "tag-push trigger restricted to strict semver (vX.Y.Z)"
else
  err "missing strict-semver tag-push trigger ('v[0-9]+.[0-9]+.[0-9]+') in $WF"
fi

# Verification gate: a validate-tag job must run before build/release and
# enforce (a) tag existence, (b) ancestry on origin/main, (c) pattern
# classification. Rule content lives in the pinned reusable workflow.
if grep -E "^\s*validate-tag:" "$WF" >/dev/null; then
  ok "validate-tag job present"
else
  err "missing validate-tag job in $WF"
fi
if grep -E "git merge-base --is-ancestor" "$GATE_SRC" >/dev/null; then
  ok "pinned gate enforces ancestry on origin/main"
else
  err "pinned gate must reject tags not reachable from origin/main"
fi
if grep -E '^on:' "$GATE_SRC" >/dev/null \
   && grep -E 'workflow_call:' "$GATE_SRC" >/dev/null \
   && ! grep -E '^[[:space:]]+(push|workflow_dispatch):' "$GATE_SRC" >/dev/null; then
  ok "pinned gate is workflow_call only (no second release path)"
else
  err "pinned gate must be workflow_call only; do not add push or workflow_dispatch"
fi
if grep -E "needs:\s*validate-tag|needs:\s*\[validate-tag" "$WF" >/dev/null; then
  ok "build/release jobs depend on validate-tag"
else
  err "build/release must declare 'needs: validate-tag' so signing is gated on it"
fi

# Publish step must read draft/prerelease from validate-tag.outputs.kind
# so dispatch-triggered runs never auto-publish a normal release.
if grep -E "draft:\s*\\\$\{\{\s*needs\.validate-tag\.outputs\.kind" "$WF" >/dev/null \
   && grep -E "prerelease:\s*\\\$\{\{\s*needs\.validate-tag\.outputs\.kind" "$WF" >/dev/null; then
  ok "publish step keys draft+prerelease off validate-tag.outputs.kind"
else
  err "publish step must set draft+prerelease from needs.validate-tag.outputs.kind"
fi

# Dispatch path must refuse stable (vX.Y.Z) tags. softprops/action-gh-release
# updates an existing release when tag_name already exists, and dispatch
# always classifies as kind=prerelease+draft — so dispatching a previously
# published vX.Y.Z would silently downgrade the public release. The
# validate-tag job must refuse this combination before kind is assigned.
#
# We check for the verbatim error string (rather than just "the regex
# appears near workflow_dispatch") so the assertion is robust to
# variable renames but still flags a block deletion: removing the guard
# also removes its error message.
if grep -F 'workflow_dispatch refuses stable tag' "$GATE_SRC" >/dev/null; then
  ok "pinned gate refuses stable tags from workflow_dispatch (downgrade-attack guard)"
else
  err "pinned gate must refuse stable tags (vX.Y.Z) from workflow_dispatch; see RELEASING.md and release.yml comment on softprops update-if-exists"
fi

if grep -F 'branch_dry_run:' "$WF" >/dev/null \
   && grep -F 'branch_dry_run is disabled:' "$GATE_SRC" >/dev/null \
   && ! grep -F 'tag_sha="$(git rev-parse HEAD)"' "$WF" >/dev/null \
   && ! grep -F 'tag_sha="$(git rev-parse HEAD)"' "$GATE_SRC" >/dev/null; then
  ok "branch-selected release dry-runs fail explicitly before credentialed jobs"
else
  err "branch_dry_run must fail explicitly; branch-selected workflow code cannot receive release credentials"
fi

# GitHub environments are the control-plane boundary that a tag's copy of
# this workflow cannot self-approve. Every job that can sign or publish must
# wait for the approval-protected release environment.
for job in build build-macos-cask release publish-manifests; do
  block=$(
    awk -v wanted="$job" '
      $0 == "  " wanted ":" { in_job=1; next }
      in_job && /^  [A-Za-z0-9_-]+:/ { exit }
      in_job { print }
    ' "$WF"
  )
  if grep -E '^    environment:\s*release\s*$' <<<"$block" >/dev/null; then
    ok "$job uses approval-protected release environment"
  else
    err "$job must declare environment: release before signing or publishing"
  fi
done

# External actions execute inside credentialed release jobs. Mutable tags and
# branches are therefore forbidden even when the repository is trusted.
while IFS= read -r action; do
  [[ -z "$action" || "$action" == ./* ]] && continue
  ref="${action##*@}"
  if [[ "$ref" =~ ^[0-9a-f]{40}$ ]]; then
    ok "external action pinned: $action"
  else
    err "external action must use an exact 40-character commit SHA: $action"
  fi
done < <(sed -nE 's/^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]*([^[:space:]#]+).*/\2/p' "$WF" "$GATE_SRC")

# Least privilege: OIDC is available only to the two artifact-signing jobs,
# and repository write permission only to the GitHub Release publisher.
top_permissions=$(
  awk '
    /^permissions:/ { in_permissions=1; next }
    in_permissions && /^[^[:space:]]/ { exit }
    in_permissions { print }
  ' "$WF"
)
if grep -E '^\s*contents:\s*read\s*$' <<<"$top_permissions" >/dev/null \
   && ! grep -E '^\s*(contents:\s*write|id-token:\s*write)\s*$' <<<"$top_permissions" >/dev/null; then
  ok "top-level permissions are read-only"
else
  err "top-level permissions must be contents: read; grant write/OIDC only to credentialed jobs"
fi

# All five active target triples (win-arm64 parked, see below).
targets=(
  "aarch64-apple-darwin"
  "x86_64-apple-darwin"
  "aarch64-unknown-linux-gnu"
  "x86_64-unknown-linux-gnu"
  "x86_64-pc-windows-msvc"
  # aarch64-pc-windows-msvc parked until cosign ships win-arm64 binaries
  # (cosign-installer@v3 has no asset; signing hard-fails). See the
  # re-enable tracking issue before adding it back here AND in release.yml.
)
for t in "${targets[@]}"; do
  if grep -F "$t" "$WF" >/dev/null; then
    ok "target $t declared"
  else
    err "target $t missing from $WF"
  fi
done

# Linux glibc floor (#549). The two -unknown-linux-gnu legs MUST build on
# ubuntu-22.04 runners (glibc 2.35) so the binaries run on Debian 12 /
# Ubuntu 22.04 forward. Building on a newer runner (ubuntu-24.04, glibc
# 2.39) raises the symbol floor and crashes at runtime on those targets.
# We assert the runner pin per-leg via the parsed-YAML pass below; this
# grep is the cheap smoke screen that flags a wholesale bump.
if grep -E "runner:\s*ubuntu-24\.04(-arm)?\b" "$WF" >/dev/null; then
  err "a job pins runner: ubuntu-24.04 — the linux-gnu legs must stay on ubuntu-22.04 for the glibc 2.35 floor (#549)"
else
  ok "no ubuntu-24.04 runner pin (glibc floor preserved)"
fi
if grep -F "glibc floor" RELEASING.md >/dev/null; then
  ok "RELEASING.md documents the Linux glibc floor"
else
  err "RELEASING.md must document the Linux glibc floor (see #549)"
fi

# macOS FSKit SDK floor. Every Apple release artifact must build on macos-26;
# older runner images can lack the FSKit SDK shape the CLI's mount feature now
# compiles against.
if grep -E "runner:\s*macos-(1[0-9]|2[0-5])\b|runs-on:\s*macos-(1[0-9]|2[0-5])\b" "$WF" >/dev/null; then
  err "release workflow contains a pre-macos-26 Apple runner; macOS release artifacts must build on macos-26"
else
  ok "no pre-macos-26 runner pin in release workflow"
fi

# Packaging: tarball for unix, zip for windows.
grep -E "\.tar\.gz" "$WF" >/dev/null && ok "tar.gz packaging" || err "no tar.gz packaging in $WF"
grep -E "\.zip"    "$WF" >/dev/null && ok "zip packaging"    || err "no zip packaging in $WF"
grep -E "\.dmg"    "$WF" >/dev/null && ok "macOS cask DMG packaging" || err "no macOS cask DMG packaging in $WF"

if grep -F 'FSMONITOR_WORKER_NAME: heddle-fsmonitor-worker' "$WF" >/dev/null \
   && grep -F 'cp "target/${{ matrix.target }}/release/${FSMONITOR_WORKER_NAME}" "dist/${stage}/"' "$WF" >/dev/null \
   && grep -F 'Copy-Item "target/${{ matrix.target }}/release/${env:FSMONITOR_WORKER_NAME}.exe" "dist/$stage/"' "$WF" >/dev/null \
   && grep -F 'cp "target/${target}/release/${FSMONITOR_WORKER_NAME}" "dist/${stage}/"' "$WF" >/dev/null; then
  ok "prebuilt CLI archives stage the fsmonitor worker beside heddle"
else
  err "every prebuilt CLI archive must stage heddle-fsmonitor-worker beside heddle"
fi

if grep -F -- '--bin heddle-fsmonitor-worker' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F 'Contents/Resources/bin/heddle-fsmonitor-worker' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F -B2 '"$STAGED_APP/Contents/Resources/bin/heddle-fsmonitor-worker"' scripts/build-macos-cask-artifact.sh \
      | grep -F 'codesign --force --timestamp --options runtime' >/dev/null; then
  ok "macOS app builds, signs, and stages the fsmonitor worker"
else
  err "the macOS app must build and stage heddle-fsmonitor-worker beside heddle"
fi

if grep -F 'target_cli="$REPO_ROOT/target/$target/release/heddle"' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F 'target_worker="$REPO_ROOT/target/$target/release/heddle-fsmonitor-worker"' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F 'codesign --force --timestamp --options runtime --sign "$DEVELOPER_ID" "$target_cli"' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F 'codesign --force --timestamp --options runtime --sign "$DEVELOPER_ID" "$target_worker"' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F 'xcrun notarytool submit "$STANDALONE_NOTARY_ZIP"' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F 'spctl -a -vvv -t execute "$REPO_ROOT/target/$target/release/heddle-fsmonitor-worker"' scripts/build-macos-cask-artifact.sh >/dev/null; then
  ok "standalone macOS CLI archives contain Developer ID signed and notarized binaries"
else
  err "standalone macOS CLI and fsmonitor worker binaries must be Developer ID signed and notarized before tar packaging"
fi

if grep -F 'install -m 0755 "$WORK/$stage/heddle-fsmonitor-worker" "$root/usr/bin/heddle-fsmonitor-worker"' scripts/build-apt-pool.sh >/dev/null; then
  ok "apt package installs the fsmonitor worker beside heddle"
else
  err "the apt package must install heddle-fsmonitor-worker beside heddle"
fi

if grep -F 'FSMONITOR_WORKER_PATH=' crates/mount/swift/HeddleHost/pkg/make-pkg.sh >/dev/null \
   && grep -F '"$PKGROOT/usr/local/bin/heddle-fsmonitor-worker"' crates/mount/swift/HeddleHost/pkg/make-pkg.sh >/dev/null \
   && grep -F 'heddle-fsmonitor-worker` in `/usr/local/bin/heddle-fsmonitor-worker' crates/mount/swift/HeddleHost/BUILD.md >/dev/null; then
  ok "macOS package installs the fsmonitor worker beside heddle"
else
  err "the macOS package must install heddle-fsmonitor-worker beside heddle"
fi

if grep -F '`heddle-fsmonitor-worker` (or `.exe` on Windows)' RELEASING.md >/dev/null; then
  ok "artifact contract documents the fsmonitor worker"
else
  err "RELEASING.md must list heddle-fsmonitor-worker in every CLI archive"
fi

# macOS cask release path.
if grep -E "^\s*build-macos-cask:" "$WF" >/dev/null \
   && grep -F "runs-on: macos-26" "$WF" >/dev/null \
   && grep -F "scripts/build-macos-cask-artifact.sh" "$WF" >/dev/null; then
  ok "macOS cask artifact job present"
else
  err "missing macOS cask artifact job (build-macos-cask on macos-26)"
fi

if grep -F 'Heddle-${TAG}-macos-universal.dmg' "$WF" >/dev/null \
   || grep -F 'Heddle-${{ needs.validate-tag.outputs.tag }}-macos-universal.dmg' "$WF" >/dev/null; then
  ok "macOS cask DMG artifact name declared"
else
  err "missing deterministic Heddle-<tag>-macos-universal.dmg artifact name"
fi

if grep -F 'cargo build --release --locked -p ${{ env.CRATE_NAME }} --features mount' "$WF" >/dev/null; then
  ok "release CLI build explicitly enables mount backends"
else
  err "release CLI build must include --features mount so macOS binaries include FSKit support"
fi

if grep -F "cargo build --release --locked -p heddle-mount --features fskit --target" scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F "cargo build --release --locked -p heddle-cli" scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F -- "--bin heddle" scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F -- "--features mount,client" scripts/build-macos-cask-artifact.sh >/dev/null; then
  ok "macOS cask build explicitly enables FSKit/mount features"
else
  err "macOS cask build must compile heddle-mount with --features fskit and heddle-cli with mount enabled"
fi

if ! grep -F "target: aarch64-apple-darwin" "$WF" >/dev/null \
   && ! grep -F "target: x86_64-apple-darwin" "$WF" >/dev/null \
   && grep -F "dist/heddle-\${{ needs.validate-tag.outputs.tag }}-aarch64-apple-darwin.tar.gz" "$WF" >/dev/null \
   && grep -F "dist/heddle-\${{ needs.validate-tag.outputs.tag }}-x86_64-apple-darwin.tar.gz" "$WF" >/dev/null; then
  ok "macOS CLI archives are packaged by the cask job from the single Apple build"
else
  err "macOS CLI archives must be packaged by build-macos-cask, and Apple targets must not be duplicated in the generic build matrix"
fi

if ! grep -F "com.apple.security.temporary-exception.files." \
  crates/mount/swift/HeddleHost/HeddleFSModule/HeddleFSModule.entitlements >/dev/null; then
  ok "FSKit extension avoids profile-gated temporary path exceptions"
else
  err "FSKit extension must not request temporary-exception.files entitlements; Developer ID profiles do not authorize them"
fi

staged_app_verify_count="$(grep -F 'verify_app_signature "$STAGED_APP"' scripts/build-macos-cask-artifact.sh | wc -l | tr -d ' ')"
if [[ "$staged_app_verify_count" -ge 2 ]]; then
  ok "macOS cask build verifies app signature before and after app notarization"
else
  err "macOS cask build must verify Heddle.app signature before and after app notarization/stapling"
fi

final_dmg_app_verify_count="$(grep -F 'verify_dmg_app_signature "$DMG_PATH"' scripts/build-macos-cask-artifact.sh | wc -l | tr -d ' ')"
if [[ "$final_dmg_app_verify_count" -ge 2 ]] \
   && grep -F 'HEDDLE_DMG_VERIFY_APP_SIGNATURE=1' scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F 'HEDDLE_DMG_VERIFY_APP_SIGNATURE' crates/mount/swift/HeddleHost/dmg/make-dmg.sh >/dev/null; then
  ok "macOS cask build verifies app signature inside staged and final DMGs"
else
  err "macOS cask build must verify Heddle.app inside the staged and final DMG, not only before packaging"
fi

dmg_signature_verify_count="$(grep -F 'codesign --verify --strict --verbose=4 "$DMG_PATH"' scripts/build-macos-cask-artifact.sh | wc -l | tr -d ' ')"
if [[ "$dmg_signature_verify_count" -ge 2 ]] \
   && grep -F 'xcrun stapler validate "$DMG_PATH"' scripts/build-macos-cask-artifact.sh >/dev/null; then
  ok "macOS cask build verifies DMG signature before and after DMG notarization"
else
  err "macOS cask build must verify the DMG code signature before and after DMG notarization/stapling"
fi

if [[ -x scripts/render-homebrew-cask.sh ]] \
   && grep -F "Casks/heddle.rb" "$WF" >/dev/null \
   && grep -F "actions/create-github-app-token" "$WF" >/dev/null \
   && grep -F "HeddleCo/homebrew-tap" "$WF" >/dev/null; then
  ok "Homebrew cask manifest publication wired"
else
  err "missing Homebrew cask manifest publication wiring"
fi

# Scoop (Windows) manifest publication — parallel channel on the same
# substrate (#233). The renderer emits bucket/heddle.json from the Windows
# zip line(s) in SHA256SUMS; the publish-manifests job opens a PR against
# the scoop-heddle bucket with the same App token, gated stable-only by
# the publish-manifests `if` condition asserted below.
if [[ -x scripts/render-scoop-manifest.sh ]] \
   && grep -F "bucket/heddle.json" "$WF" >/dev/null \
   && grep -F "actions/create-github-app-token" "$WF" >/dev/null \
   && grep -F "HeddleCo/scoop-heddle" "$WF" >/dev/null; then
  ok "Scoop manifest publication wired"
else
  err "missing Scoop manifest publication wiring"
fi

# apt (Debian/Ubuntu) pool — parallel channel on the same substrate (#234).
# scripts/build-apt-pool.sh builds the amd64 + arm64 .deb (and the
# heddle-archive-keyring .deb) from the linux-gnu tarballs in SHA256SUMS, then
# generates + GPG-signs the pool/Packages/Release index in an ephemeral
# GNUPGHOME (Ed25519 subkey, #328 Decision 2). The pool is still built every
# release.
#
# PUBLICATION IS DEFERRED until HeddleCo/apt-heddle exists: the "Publish apt
# pool PR" step and the apt-heddle token scope were removed because scoping the
# token / PRing to a nonexistent repo failed the whole publish-manifests job
# and blocked the Homebrew + Scoop pushes. Contract for the deferred state:
# apt pool is still built + signed, but apt-heddle must NOT be referenced by
# the workflow (no target-repo, no token scope). Revive both when the repo
# exists (and restore the apt-heddle assertions here).
# Grep only for the ACTIVE wiring forms (a `target-repo:` mapping and a
# `repositories:` token scope) so that revivable comments mentioning
# apt-heddle don't count as live wiring.
if [[ -x scripts/build-apt-pool.sh ]] \
   && grep -F "scripts/build-apt-pool.sh" "$WF" >/dev/null \
   && grep -F "HEDDLE_APT_GPG_PRIVATE_KEY" "$WF" >/dev/null \
   && grep -F "actions/create-github-app-token" "$WF" >/dev/null \
   && ! grep -E "^[[:space:]]*target-repo: HeddleCo/apt-heddle" "$WF" >/dev/null \
   && ! grep -E "^[[:space:]]*repositories: .*apt-heddle" "$WF" >/dev/null; then
  ok "apt pool built + signed; publication deferred (apt-heddle absent)"
else
  err "apt pool must be built+signed with publication deferred (no apt-heddle references) until HeddleCo/apt-heddle exists"
fi

if grep -F "if: needs.validate-tag.outputs.kind == 'stable'" "$WF" >/dev/null; then
  ok "manifest publication gated to stable releases"
else
  err "publish-manifests must be gated to stable releases only"
fi

# sha256 checksums.
if grep -Ei "sha256sum|shasum|sha256" "$WF" >/dev/null; then
  ok "sha256 checksums step"
else
  err "no sha256 checksum step in $WF"
fi

# Signing (cosign keyless via Sigstore — chosen because it requires no
# stored secrets; GitHub OIDC is the trust anchor).
if grep -Ei "cosign|sigstore" "$WF" >/dev/null; then
  ok "signing step (cosign/sigstore)"
else
  err "no signing step (cosign/sigstore) in $WF"
fi

# Upload to GitHub Release.
if grep -E "softprops/action-gh-release|gh release (create|upload)" "$WF" >/dev/null; then
  ok "GitHub Release upload step"
else
  err "no GitHub Release upload step in $WF"
fi

# RELEASING.md present and documents the artifact contract.
if [[ ! -f RELEASING.md ]]; then
  err "RELEASING.md is missing"
else
  ok "RELEASING.md present"
  for t in "${targets[@]}"; do
    if ! grep -F "$t" RELEASING.md >/dev/null; then
      err "RELEASING.md does not document target $t"
    fi
  done
fi

# --- Strict structural checks (parsed YAML) -------------------------------
#
# The grep-based checks above answer "does the pipeline mention X anywhere?"
# — useful as a quick smoke screen, but blind to per-job structure. The
# strict checks below parse release.yml and verify each downstream job
# individually:
#
#   - declares `needs: validate-tag` (not just *some* job somewhere)
#   - checks out the SHA validate-tag pinned, not the mutable tag ref
#     (closes the TOCTOU window where a force-moved tag would otherwise
#     redirect build/release to a different commit than the one that
#     passed the ancestry check)
#
# We also confirm the pinned reusable workflow exports `tag_sha`.
# Without that output the pinning above can't reference anything.
#
# These are additive: the legacy "any needs: validate-tag" grep still
# runs and still flags the catastrophic "nothing depends on validate-tag"
# regression, while the strict checks here catch the subtler "one
# downstream job dropped the dep" regression.

ensure_pyyaml() {
  # Echoes the python interpreter to use (with PyYAML importable), or
  # returns non-zero. Prefer the system python3 if PyYAML is already
  # there; otherwise spin up an ephemeral venv and install PyYAML into
  # it. We deliberately don't fall back to `python3 -m pip install` at
  # system scope: on PEP 668-enforcing distros (Ubuntu 24.04+) that
  # errors out with `externally-managed-environment`, which would turn
  # this asserter into a CI breaker on slim runner images.
  if python3 -c 'import yaml' 2>/dev/null; then
    echo python3
    return 0
  fi
  local venv
  venv="$(mktemp -d)/venv"
  python3 -m venv "$venv" >/dev/null 2>&1 || return 1
  "$venv/bin/pip" install --quiet --disable-pip-version-check pyyaml >/dev/null 2>&1 || return 1
  "$venv/bin/python" -c 'import yaml' 2>/dev/null || return 1
  echo "$venv/bin/python"
}

if command -v ruby >/dev/null 2>&1 && ruby --disable-gems -e 'require "yaml"' >/dev/null 2>&1; then
  strict_report=$(ruby --disable-gems - "$WF" "$GATE_SRC" <<'RB'
require "yaml"

wf_path = ARGV.fetch(0)
gate_path = ARGV.fetch(1)
wf = YAML.load_file(wf_path)
gate = YAML.load_file(gate_path)

jobs = wf.fetch("jobs", {}) || {}
errors = []
oks = []

vt = jobs["validate-tag"]
if !vt.is_a?(Hash)
  errors << "validate-tag job missing or malformed"
else
  uses = vt["uses"].to_s
  if uses.match?(/\AHEddleCo\/heddle\/\.github\/workflows\/validate-release-tag\.yml@[0-9a-f]{40}\z/)
    oks << "validate-tag is a SHA-pinned reusable workflow"
  else
    errors << "validate-tag must use HeddleCo/heddle/.github/workflows/validate-release-tag.yml@<40-char-sha>, got '#{uses}'"
  end
  if vt.key?("steps") || vt.key?("runs-on") || vt.key?("environment")
    errors << "validate-tag caller must not inline steps/runs-on/environment; the pinned reusable workflow is the gate"
  else
    oks << "validate-tag caller has no inline steps"
  end
  # YAML 1.1 treats the key `on` as boolean true.
  on_block = gate["on"]
  on_block = gate[true] unless on_block.is_a?(Hash)
  wc = (on_block.is_a?(Hash) ? on_block["workflow_call"] : nil) || {}
  outs = wc.fetch("outputs", {}) || {}
  if !outs.key?("tag_sha")
    errors << "validate-tag must declare a 'tag_sha' output (used by downstream jobs to pin checkout to the validated commit)"
  else
    oks << "validate-tag exports tag_sha output"
  end
  errors << "validate-tag must declare 'tag', 'kind', and 'publish_release' outputs" unless outs.key?("tag") && outs.key?("kind") && outs.key?("publish_release")
end

downstream = ["build", "build-macos-cask", "release", "publish-manifests"]
downstream.each do |name|
  job = jobs[name]
  if !job.is_a?(Hash)
    errors << "#{name} job missing or malformed"
    next
  end
  needs = job.fetch("needs", [])
  needs = [needs] if needs.is_a?(String)
  if !needs.include?("validate-tag")
    errors << "#{name} job does not declare 'needs: validate-tag' (would skip the trust gate)"
  else
    oks << "#{name} job declares needs: validate-tag"
  end
end

sha_ref_ok = "${{ needs.validate-tag.outputs.tag_sha }}"
tag_ref_bad = "refs/tags/"
downstream.each do |name|
  job = jobs[name]
  next unless job.is_a?(Hash)
  steps = job.fetch("steps", []) || []
  checkouts = steps.select do |step|
    step.is_a?(Hash) &&
      step["uses"].is_a?(String) &&
      step["uses"].start_with?("actions/checkout@")
  end
  if checkouts.empty?
    errors << "#{name} job has no actions/checkout step - cannot verify SHA pin"
    next
  end
  checkouts.each do |step|
    ref = step.fetch("with", {}).fetch("ref", "").to_s
    if !ref.include?(sha_ref_ok)
      errors << "#{name} job checks out '#{ref}' instead of needs.validate-tag.outputs.tag_sha - TOCTOU on mutable tag ref"
    elsif ref.include?(tag_ref_bad)
      errors << "#{name} job mixes refs/tags/ with tag_sha ('#{ref}') - refs/tags/ is mutable; remove it"
    else
      oks << "#{name} job pins checkout to validated tag_sha"
    end
  end
end

pm = jobs["publish-manifests"]
if pm.is_a?(Hash)
  condition = pm.fetch("if", "").to_s
  if !condition.include?("needs.validate-tag.outputs.kind == 'stable'")
    errors << "publish-manifests must gate on needs.validate-tag.outputs.kind == 'stable'"
  else
    oks << "publish-manifests is stable-only"
  end
  needs = pm.fetch("needs", [])
  needs = [needs] if needs.is_a?(String)
  if !needs.include?("release")
    errors << "publish-manifests must depend on release so the GitHub Release exists before opening tap PRs"
  else
    oks << "publish-manifests depends on release"
  end
end

build_job = jobs["build"]
if build_job.is_a?(Hash)
  matrix = build_job.fetch("strategy", {}).fetch("matrix", {}) || {}
  include = matrix.fetch("include", []) || []
  gnu_legs = include.select do |entry|
    entry.is_a?(Hash) && entry.fetch("target", "").to_s.end_with?("-unknown-linux-gnu")
  end
  errors << "build matrix has no *-unknown-linux-gnu legs to floor" if gnu_legs.empty?
  gnu_legs.each do |entry|
    runner = entry.fetch("runner", "").to_s
    target = entry["target"]
    if runner.start_with?("ubuntu-22.04")
      oks << "#{target} pinned to #{runner} (glibc 2.35 floor)"
    else
      errors << "#{target} builds on '#{runner}', not ubuntu-22.04 - raises the glibc floor above 2.35 (#549)"
    end
  end
  apple_legs = include.select do |entry|
    entry.is_a?(Hash) && entry.fetch("target", "").to_s.end_with?("-apple-darwin")
  end
  if apple_legs.empty?
    oks << "generic build matrix omits Apple targets; build-macos-cask owns the single macOS build"
  else
    errors << "generic build matrix still includes Apple targets: #{apple_legs.map { |entry| entry["target"] }.join(", ")}"
  end
  cask = jobs["build-macos-cask"]
  if cask.is_a?(Hash)
    runner = cask.fetch("runs-on", "").to_s
    if runner == "macos-26"
      oks << "build-macos-cask runs on macos-26"
    else
      errors << "build-macos-cask runs on '#{runner}', not macos-26"
    end
    cask_text = cask.inspect
    if cask_text.include?("aarch64-apple-darwin,x86_64-apple-darwin") &&
       cask_text.include?("heddle-${TAG}-aarch64-apple-darwin.tar.gz") &&
       cask_text.include?("heddle-${TAG}-x86_64-apple-darwin.tar.gz")
      oks << "build-macos-cask builds both Apple targets and packages standalone macOS CLI archives"
    else
      errors << "build-macos-cask must build both Apple targets and package standalone macOS CLI archives"
    end
  end
end

puts "OKS:"
oks.each { |ok| puts ok }
puts "ERRORS:"
errors.each { |error| puts error }
RB
  )

  in_oks=0
  in_errors=0
  while IFS= read -r line; do
    case "$line" in
      "OKS:")     in_oks=1; in_errors=0; continue ;;
      "ERRORS:")  in_oks=0; in_errors=1; continue ;;
    esac
    [[ -z "$line" ]] && continue
    if (( in_oks )); then
      ok "$line"
    elif (( in_errors )); then
      err "$line"
    fi
  done <<< "$strict_report"
elif ! command -v python3 >/dev/null 2>&1; then
  err "python3 not available; strict structural checks skipped"
elif ! PY=$(ensure_pyyaml); then
  err "PyYAML not available and venv fallback failed; strict structural checks skipped"
else
  strict_report=$("$PY" - "$WF" "$GATE_SRC" <<'PY'
import sys
import yaml
import re

wf_path = sys.argv[1]
gate_path = sys.argv[2]
with open(wf_path) as f:
    wf = yaml.safe_load(f)
with open(gate_path) as f:
    gate = yaml.safe_load(f)

jobs = wf.get("jobs", {}) or {}
errors = []
oks = []

PINNED_USES = re.compile(
    r"^HeddleCo/heddle/\.github/workflows/validate-release-tag\.yml@[0-9a-f]{40}$"
)

vt = jobs.get("validate-tag")
if not isinstance(vt, dict):
    errors.append("validate-tag job missing or malformed")
else:
    uses = str(vt.get("uses") or "")
    if PINNED_USES.match(uses):
        oks.append("validate-tag is a SHA-pinned reusable workflow")
    else:
        errors.append(
            "validate-tag must use HeddleCo/heddle/.github/workflows/validate-release-tag.yml@<40-char-sha>, "
            f"got '{uses}'"
        )
    if any(key in vt for key in ("steps", "runs-on", "environment")):
        errors.append(
            "validate-tag caller must not inline steps/runs-on/environment; the pinned reusable workflow is the gate"
        )
    else:
        oks.append("validate-tag caller has no inline steps")
    on = gate.get("on") if isinstance(gate, dict) else None
    if not isinstance(on, dict) and isinstance(gate, dict):
        on = gate.get(True)
    wc = on.get("workflow_call") if isinstance(on, dict) else None
    outs = (wc or {}).get("outputs", {}) or {}
    if "tag_sha" not in outs:
        errors.append("validate-tag must declare a 'tag_sha' output (used by downstream jobs to pin checkout to the validated commit)")
    else:
        oks.append("validate-tag exports tag_sha output")
    if "tag" not in outs or "kind" not in outs:
        errors.append("validate-tag must declare 'tag' and 'kind' outputs")
    if "publish_release" not in outs:
        errors.append("validate-tag must declare a 'publish_release' output so dry-runs cannot publish releases")

# Every job that runs AFTER validate-tag (i.e. that produces or ships
# artifacts) must declare it as a needs dependency. Listing the set
# explicitly keeps this honest: adding a new downstream job requires
# updating this list, which forces a conscious decision about whether
# the new job needs the trust gate.
downstream = ["build", "build-macos-cask", "release", "publish-manifests"]
for name in downstream:
    job = jobs.get(name)
    if not isinstance(job, dict):
        errors.append(f"{name} job missing or malformed")
        continue
    needs = job.get("needs", [])
    if isinstance(needs, str):
        needs = [needs]
    if "validate-tag" not in needs:
        errors.append(f"{name} job does not declare 'needs: validate-tag' (would skip the trust gate)")
    else:
        oks.append(f"{name} job declares needs: validate-tag")

# Every downstream job's checkout step must pin to the validated SHA.
# Acting on refs/tags/<tag> after validate-tag would re-resolve the
# tag — a window where a force-move would redirect the build.
SHA_REF_OK = "${{ needs.validate-tag.outputs.tag_sha }}"
TAG_REF_BAD = "refs/tags/"
for name in downstream:
    job = jobs.get(name)
    if not isinstance(job, dict):
        continue
    steps = job.get("steps", []) or []
    checkouts = [
        s for s in steps
        if isinstance(s, dict)
        and isinstance(s.get("uses"), str)
        and s.get("uses", "").startswith("actions/checkout@")
    ]
    if not checkouts:
        errors.append(f"{name} job has no actions/checkout step — cannot verify SHA pin")
        continue
    for s in checkouts:
        ref = (s.get("with") or {}).get("ref", "")
        if not isinstance(ref, str):
            ref = str(ref)
        if SHA_REF_OK not in ref:
            errors.append(
                f"{name} job checks out '{ref}' instead of needs.validate-tag.outputs.tag_sha — TOCTOU on mutable tag ref"
            )
        elif TAG_REF_BAD in ref:
            errors.append(
                f"{name} job mixes refs/tags/ with tag_sha ('{ref}') — refs/tags/ is mutable; remove it"
            )
        else:
            oks.append(f"{name} job pins checkout to validated tag_sha")

# The tap update must never run for RC/draft workflow_dispatch releases.
pm = jobs.get("publish-manifests")
if isinstance(pm, dict):
    condition = str(pm.get("if", ""))
    if "needs.validate-tag.outputs.kind == 'stable'" not in condition:
        errors.append("publish-manifests must gate on needs.validate-tag.outputs.kind == 'stable'")
    else:
        oks.append("publish-manifests is stable-only")
    needs = pm.get("needs", [])
    if isinstance(needs, str):
        needs = [needs]
    if "release" not in needs:
        errors.append("publish-manifests must depend on release so the GitHub Release exists before opening tap PRs")
    else:
        oks.append("publish-manifests depends on release")

# Linux glibc floor (#549): the two -unknown-linux-gnu build legs must
# pin an ubuntu-22.04 runner (glibc 2.35). Read the runner per matrix
# entry rather than grepping, so a per-leg regression (one leg bumped)
# is caught even if the other stays correct.
build_job = jobs.get("build")
if isinstance(build_job, dict):
    matrix = ((build_job.get("strategy") or {}).get("matrix") or {})
    include = matrix.get("include", []) or []
    gnu_legs = [e for e in include if isinstance(e, dict)
                and str(e.get("target", "")).endswith("-unknown-linux-gnu")]
    if not gnu_legs:
        errors.append("build matrix has no *-unknown-linux-gnu legs to floor")
    for e in gnu_legs:
        runner = str(e.get("runner", ""))
        target = e.get("target")
        if runner.startswith("ubuntu-22.04"):
            oks.append(f"{target} pinned to {runner} (glibc 2.35 floor)")
        else:
            errors.append(
                f"{target} builds on '{runner}', not ubuntu-22.04 — raises the glibc floor above 2.35 (#549)"
            )
    apple_legs = [e for e in include if isinstance(e, dict)
                  and str(e.get("target", "")).endswith("-apple-darwin")]
    if not apple_legs:
        oks.append("generic build matrix omits Apple targets; build-macos-cask owns the single macOS build")
    else:
        errors.append(
            "generic build matrix still includes Apple targets: "
            + ", ".join(str(e.get("target")) for e in apple_legs)
        )
    cask = jobs.get("build-macos-cask")
    if isinstance(cask, dict):
        runner = str(cask.get("runs-on", ""))
        if runner == "macos-26":
            oks.append("build-macos-cask runs on macos-26")
        else:
            errors.append(f"build-macos-cask runs on '{runner}', not macos-26")
        cask_text = repr(cask)
        if (
            "aarch64-apple-darwin,x86_64-apple-darwin" in cask_text
            and "heddle-${TAG}-aarch64-apple-darwin.tar.gz" in cask_text
            and "heddle-${TAG}-x86_64-apple-darwin.tar.gz" in cask_text
        ):
            oks.append("build-macos-cask builds both Apple targets and packages standalone macOS CLI archives")
        else:
            errors.append("build-macos-cask must build both Apple targets and package standalone macOS CLI archives")

print("OKS:")
for o in oks:
    print(o)
print("ERRORS:")
for e in errors:
    print(e)
PY
  )

  in_oks=0
  in_errors=0
  while IFS= read -r line; do
    case "$line" in
      "OKS:")     in_oks=1; in_errors=0; continue ;;
      "ERRORS:")  in_oks=0; in_errors=1; continue ;;
    esac
    [[ -z "$line" ]] && continue
    if (( in_oks )); then
      ok "$line"
    elif (( in_errors )); then
      err "$line"
    fi
  done <<< "$strict_report"
fi

# Proof for heddle#1415: a tagged tree can rewrite the working copy of
# the gate file. GitHub still fetches the object at GATE_PIN. The caller
# pin is a literal SHA, so emptying a sibling copy cannot drop the check.
if [[ -n "$GATE_PIN" ]] && git cat-file -e "${GATE_PIN}:${GATE_WF}" 2>/dev/null; then
  proof_dir="$(mktemp -d)"
  git show "${GATE_PIN}:${GATE_WF}" > "${proof_dir}/pinned.yml"
  printf '%s\n' 'on:' '  workflow_call: {}' 'jobs: {}' > "${proof_dir}/rewritten.yml"
  if grep -F 'git merge-base --is-ancestor' "${proof_dir}/pinned.yml" >/dev/null \
     && ! grep -F 'git merge-base --is-ancestor' "${proof_dir}/rewritten.yml" >/dev/null \
     && grep -F "${GATE_USES_PREFIX}${GATE_PIN}" "$WF" >/dev/null; then
    ok "tagged-tree rewrite of ${GATE_WF} cannot drop the pin at ${GATE_PIN}"
  else
    err "could not prove the tagged-tree rewrite cannot drop the pinned gate"
  fi
  rm -rf "$proof_dir"
fi

if [[ "$GATE_SRC" != "$GATE_WF" ]]; then
  rm -f "$GATE_SRC"
fi

if (( fail )); then
  echo "release-pipeline check FAILED" >&2
  exit 1
fi
echo "release-pipeline check passed"
