#!/usr/bin/env bash
# Regenerate `signed-objects.bundle` — the deterministic signed-object fixture
# for the round-trip fidelity gate (heddle#562).
#
# The bundle carries a small repo with the two most error-prone fidelity cases
# that the vendored real-world fixtures never exercised (vendor.sh used to pass
# `--no-tags` + `--signed-tags=strip`, so signed objects never reached the gate):
#
#   * a SIGNED COMMIT — the PGP signature lives in a folded `gpgsig` header
#     after the `committer` line;
#   * a SIGNED ANNOTATED TAG — the signature is appended UNFOLDED in the tag
#     body, after the human message (NOT a header — git's two signing
#     mechanisms are not symmetric).
#
# The signing key is EPHEMERAL (generated fresh here, never stored), so the
# signed object SHAs are NOT reproducible across runs of this script. That is
# fine: the bundle is checked in, so once committed the SHAs are stable, and
# the test recomputes every SHA from the live repo rather than hardcoding one.
# Re-run this only to refresh the fixture (e.g. to widen the corpus); commit
# the resulting bundle.
#
# Requires `gpg` + `git` on PATH. CI does NOT run this — it consumes the
# checked-in bundle — so the gate never depends on a gpg binary being present
# on the runner.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$SCRIPT_DIR/signed-objects.bundle"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

export GNUPGHOME="$SCRATCH/gnupg"
mkdir -p "$GNUPGHOME"
chmod 700 "$GNUPGHOME"

# Ephemeral ed25519 key, no passphrase so batch signing needs no agent prompt.
cat > "$SCRATCH/keyparams" <<'EOF'
%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Subkey-Type: eddsa
Subkey-Curve: ed25519
Name-Real: Heddle Conformance
Name-Email: conformance@heddle.test
Expire-Date: 0
%commit
EOF
gpg --batch --quiet --gen-key "$SCRATCH/keyparams"
KEY="$(gpg --list-secret-keys --with-colons | awk -F: '/^sec/{print $5; exit}')"

# Pin identity + dates so the unsigned objects are stable; the signed ones
# vary only by the ephemeral signature.
export GIT_AUTHOR_NAME="Heddle Conformance" GIT_AUTHOR_EMAIL="conformance@heddle.test"
export GIT_COMMITTER_NAME="Heddle Conformance" GIT_COMMITTER_EMAIL="conformance@heddle.test"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null LC_ALL=C TZ=UTC

REPO="$SCRATCH/repo"
git init -q -b main "$REPO"
git -C "$REPO" config gpg.program gpg
git -C "$REPO" config user.signingkey "$KEY"

echo hello > "$REPO/f"
git -C "$REPO" add f
GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    git -C "$REPO" commit -q -m "base commit"
C1="$(git -C "$REPO" rev-parse HEAD)"

# Signed commit: folded `gpgsig` header after `committer`.
echo more >> "$REPO/f"
git -C "$REPO" add f
GIT_AUTHOR_DATE="1700000900 +0000" GIT_COMMITTER_DATE="1700000900 +0000" \
    git -C "$REPO" commit -q -S"$KEY" -m "signed commit"
C8="$(git -C "$REPO" rev-parse HEAD)"

# Unsigned annotated tag (an annotated-tag object with no signature).
GIT_COMMITTER_DATE="1700001200 +0000" \
    git -C "$REPO" tag -a -m "annotated v0.9" v0.9 "$C1"

# Signed annotated tag: signature appended UNFOLDED in the tag body.
GIT_COMMITTER_DATE="1700001300 +0000" \
    git -C "$REPO" tag -s -u "$KEY" -m "signed release v1.0" v1.0 "$C8"

# Sanity: the fixture must actually contain the signed objects it claims to.
git -C "$REPO" cat-file commit "$C8" | grep -q '^gpgsig ' \
    || { echo "FATAL: signed commit lacks a gpgsig header" >&2; exit 1; }
git -C "$REPO" cat-file tag v1.0 | grep -q 'BEGIN PGP SIGNATURE' \
    || { echo "FATAL: signed tag lacks an inline PGP signature" >&2; exit 1; }
git -C "$REPO" fsck --full --strict

git -C "$REPO" bundle create "$OUT" --all
echo "Wrote $OUT"
echo "  base commit (unsigned): $C1"
echo "  signed commit:          $C8"
echo "  signed annotated tag:   $(git -C "$REPO" rev-parse v1.0)"
