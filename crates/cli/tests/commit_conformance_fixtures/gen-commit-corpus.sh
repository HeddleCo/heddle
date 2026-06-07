#!/usr/bin/env bash
# Regenerate `commit-corpus.bundle` — the signed-object slice of the #566
# byte-exact commit serializer conformance corpus (spike §9 cases C8/C9).
#
# The plain edge cases (C1 plain, C2 empty message, C3 no-trailing-newline, C4
# CRLF, C5 weird/negative timezones, C6 non-UTF8 encoding, C7 octopus merge) are
# generated in-process by `tests/commit_conformance.rs` with the `git` CLI —
# their SHAs are deterministic and need no signing key. The two cases that need
# a GPG key are vendored here instead, exactly like the round-trip fidelity
# gate's `gen-signed-objects.sh`:
#
#   * C8 — a SIGNED COMMIT: the PGP signature lives in a folded `gpgsig` header
#     after the `committer` line (spike §3);
#   * C9 — a SIGNED MERGE carrying a `mergetag` header: merging a SIGNED
#     annotated tag with `--no-ff` embeds the full (folded) tag object as a
#     `mergetag` header, and `-S` adds a `gpgsig` header — so this single commit
#     exercises BOTH extension headers AND their canonical ordering (git emits
#     `mergetag` before `gpgsig`, spike §3).
#
# The signing key is EPHEMERAL (generated fresh, never stored), so the signed
# object SHAs are NOT reproducible across runs of this script. That is fine: the
# bundle is checked in, so once committed the SHAs are stable, and the test
# recomputes every SHA from the live repo rather than hardcoding one. Re-run this
# only to refresh the fixture; commit the resulting bundle.
#
# Requires `gpg` + `git` on PATH. CI does NOT run this — it consumes the
# checked-in bundle — so the gate never depends on a gpg binary on the runner.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$SCRIPT_DIR/commit-corpus.bundle"
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

# Pin identity + dates so the unsigned objects are stable; the signed ones vary
# only by the ephemeral signature.
export GIT_AUTHOR_NAME="Heddle Conformance" GIT_AUTHOR_EMAIL="conformance@heddle.test"
export GIT_COMMITTER_NAME="Heddle Conformance" GIT_COMMITTER_EMAIL="conformance@heddle.test"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null LC_ALL=C TZ=UTC

REPO="$SCRATCH/repo"
git init -q -b main "$REPO"
git -C "$REPO" config gpg.program gpg
git -C "$REPO" config user.signingkey "$KEY"

# Base commit (unsigned).
echo hello > "$REPO/f"
git -C "$REPO" add f
GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    git -C "$REPO" commit -q -m "base commit"
C1="$(git -C "$REPO" rev-parse HEAD)"

# C8 — signed commit (folded `gpgsig` header after `committer`).
echo more >> "$REPO/f"
git -C "$REPO" add f
GIT_AUTHOR_DATE="1700000900 +0000" GIT_COMMITTER_DATE="1700000900 +0000" \
    git -C "$REPO" commit -q -S"$KEY" -m "signed commit"
C8="$(git -C "$REPO" rev-parse HEAD)"

# Side commit + signed annotated tag, to be merged for the mergetag case.
git -C "$REPO" checkout -q -b side "$C1"
echo side > "$REPO/s"
git -C "$REPO" add s
GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    git -C "$REPO" commit -q -m "side commit"
GIT_COMMITTER_DATE="1700001050 +0000" \
    git -C "$REPO" tag -s -u "$KEY" -m "signed side tag" sidetag HEAD

# C9 — signed merge of the SIGNED tag: `mergetag` header (the embedded folded tag
# object) AND `gpgsig` header, in git's canonical order.
git -C "$REPO" checkout -q main
GIT_AUTHOR_DATE="1700001100 +0000" GIT_COMMITTER_DATE="1700001100 +0000" \
    git -C "$REPO" merge -q --no-ff -S"$KEY" -m "merge with mergetag" sidetag
C9="$(git -C "$REPO" rev-parse HEAD)"

# Sanity: the fixture must actually contain the headers it claims to.
git -C "$REPO" cat-file commit "$C8" | grep -q '^gpgsig ' \
    || { echo "FATAL: C8 lacks a gpgsig header" >&2; exit 1; }
git -C "$REPO" cat-file commit "$C9" | grep -q '^mergetag ' \
    || { echo "FATAL: C9 lacks a mergetag header" >&2; exit 1; }
git -C "$REPO" cat-file commit "$C9" | grep -q '^gpgsig ' \
    || { echo "FATAL: C9 lacks a gpgsig header" >&2; exit 1; }

# Cryptographic proof the signatures are genuinely valid (the property a
# fast-export|fast-import re-root would silently destroy). The ephemeral key is
# still in the keyring here, so the checked-in bundle is one that DID verify.
git -C "$REPO" verify-commit "$C8" \
    || { echo "FATAL: signed commit does not verify" >&2; exit 1; }
git -C "$REPO" verify-commit "$C9" \
    || { echo "FATAL: signed merge does not verify" >&2; exit 1; }
git -C "$REPO" fsck --full --strict

# Bundle ONLY refs/heads/main: its history is C1 -> C8 and (side) S -> C9 merge,
# so the four commits travel but the `sidetag` tag OBJECT (not an ancestor of any
# commit) does not — the harness reconstructs commits only (tag objects are #575).
git -C "$REPO" bundle create "$OUT" refs/heads/main
echo "Wrote $OUT"
echo "  base commit (unsigned):   $C1"
echo "  signed commit (C8):       $C8"
echo "  signed merge/mergetag(C9):$C9"
