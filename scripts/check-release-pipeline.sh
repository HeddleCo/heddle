#!/usr/bin/env bash
# Asserter for the binary-release pipeline contract (heddle#56).
#
# The release pipeline is invoked only on `v*` tag pushes, so we can't
# observe its artifacts in normal CI. Instead we statically verify that
# `.github/workflows/release.yml` declares the contract every downstream
# packaging channel (HomeBrew, Scoop, apt) relies on:
#
#   - tag-push trigger
#   - all 5 target triples
#   - tarball/zip packaging
#   - sha256 checksums
#   - signing step
#   - GitHub Release upload
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

# All five target triples.
targets=(
  "aarch64-apple-darwin"
  "x86_64-apple-darwin"
  "aarch64-unknown-linux-gnu"
  "x86_64-unknown-linux-gnu"
  "x86_64-pc-windows-msvc"
)
for t in "${targets[@]}"; do
  if grep -F "$t" "$WF" >/dev/null; then
    ok "target $t declared"
  else
    err "target $t missing from $WF"
  fi
done

# Packaging: tarball for unix, zip for windows.
grep -E "\.tar\.gz" "$WF" >/dev/null && ok "tar.gz packaging" || err "no tar.gz packaging in $WF"
grep -E "\.zip"    "$WF" >/dev/null && ok "zip packaging"    || err "no zip packaging in $WF"

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

if ! command -v python3 >/dev/null 2>&1; then
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

# Every job that runs AFTER validate-tag (i.e. that produces or ships
# artifacts) must declare it as a needs dependency. Listing the set
# explicitly keeps this honest: adding a new downstream job requires
# updating this list, which forces a conscious decision about whether
# the new job needs the trust gate.
downstream = ["build", "release"]
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
