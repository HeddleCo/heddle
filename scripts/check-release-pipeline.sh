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

# Tag-push trigger.
if grep -E "^\s*tags:" "$WF" >/dev/null && grep -E "['\"]?v\*['\"]?" "$WF" >/dev/null; then
  ok "tag-push trigger on v*"
else
  err "missing tag-push trigger on v* in $WF"
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

if (( fail )); then
  echo "release-pipeline check FAILED" >&2
  exit 1
fi
echo "release-pipeline check passed"
