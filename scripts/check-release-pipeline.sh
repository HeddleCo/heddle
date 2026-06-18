#!/usr/bin/env bash
# Asserter for the binary-release pipeline contract (heddle#56).
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
#   - non-publishing branch dry-run path for release-only verification
#   - sha256 checksums
#   - signing step
#   - GitHub Release upload
#   - stable-only Homebrew manifest PR publication
#
# This is the failing test that drives the red-commit-first DoD.

set -euo pipefail

WF=".github/workflows/release.yml"
fail=0

err() { echo "::error::$*" >&2; fail=1; }
ok()  { echo "ok: $*"; }

if [[ ! -f "$WF" ]]; then
  err "$WF does not exist"
  echo "::error::Release pipeline not implemented. See heddle#56."
  exit 1
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
# classification. We assert the structural pieces here; the rule
# content lives in the workflow itself.
if grep -E "^\s*validate-tag:" "$WF" >/dev/null; then
  ok "validate-tag job present"
else
  err "missing validate-tag job in $WF"
fi
if grep -E "git merge-base --is-ancestor" "$WF" >/dev/null; then
  ok "validate-tag enforces ancestry on origin/main"
else
  err "validate-tag must reject tags not reachable from origin/main"
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
if grep -F 'workflow_dispatch refuses stable tag' "$WF" >/dev/null; then
  ok "validate-tag refuses stable tags from workflow_dispatch (downgrade-attack guard)"
else
  err "validate-tag must refuse stable tags (vX.Y.Z) from workflow_dispatch; see RELEASING.md and release.yml comment on softprops update-if-exists"
fi

if grep -F 'branch_dry_run:' "$WF" >/dev/null \
   && grep -F 'publish_release=false' "$WF" >/dev/null \
   && grep -F "if: \${{ github.event_name != 'workflow_dispatch' || !inputs.branch_dry_run }}" "$WF" >/dev/null \
   && grep -F "if: needs.validate-tag.outputs.publish_release == 'true'" "$WF" >/dev/null; then
  ok "temporary branch dry-run path skips generic builds and GitHub Release publication"
else
  err "temporary branch dry-run path must be explicit and must skip generic builds and GitHub Release publication"
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

if grep -F 'cargo build --release --locked -p ${{ env.CRATE_NAME }} --features mount --target ${{ matrix.target }}' "$WF" >/dev/null; then
  ok "release CLI build explicitly enables mount backends"
else
  err "release CLI build must pass --features mount so macOS binaries include FSKit support"
fi

if grep -F "cargo build --release --locked -p heddle-mount --features fskit --target" scripts/build-macos-cask-artifact.sh >/dev/null \
   && grep -F "cargo build --release --locked -p heddle-cli --bin heddle --features mount --target" scripts/build-macos-cask-artifact.sh >/dev/null; then
  ok "macOS cask build explicitly enables FSKit/mount features"
else
  err "macOS cask build must compile heddle-mount with --features fskit and heddle-cli with --features mount"
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
   && grep -F "HeddleCo/homebrew-heddle" "$WF" >/dev/null; then
  ok "Homebrew cask manifest publication wired"
else
  err "missing Homebrew cask manifest publication wiring"
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
# We also confirm validate-tag exports `tag_sha` as a documented output.
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
  strict_report=$(ruby --disable-gems - "$WF" <<'RB'
require "yaml"

wf_path = ARGV.fetch(0)
wf = YAML.load_file(wf_path)

jobs = wf.fetch("jobs", {}) || {}
errors = []
oks = []

vt = jobs["validate-tag"]
if !vt.is_a?(Hash)
  errors << "validate-tag job missing or malformed"
else
  outs = vt.fetch("outputs", {}) || {}
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
  strict_report=$("$PY" - "$WF" <<'PY'
import sys
import yaml

wf_path = sys.argv[1]
with open(wf_path) as f:
    wf = yaml.safe_load(f)

jobs = wf.get("jobs", {}) or {}
errors = []
oks = []

vt = jobs.get("validate-tag")
if not isinstance(vt, dict):
    errors.append("validate-tag job missing or malformed")
else:
    outs = vt.get("outputs", {}) or {}
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

if (( fail )); then
  echo "release-pipeline check FAILED" >&2
  exit 1
fi
echo "release-pipeline check passed"
