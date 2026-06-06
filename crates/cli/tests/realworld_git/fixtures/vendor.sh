#!/usr/bin/env bash
# Vendor real-world Git fixtures for the launch-quality overlay matrix.
#
# For each entry below: shallow-clone the upstream, then `fast-export |
# fast-import` into a fresh bare repo. The fast-export step is what makes the
# fixture self-contained: shallow clones leave a `.git/shallow` boundary that
# gix/heddle clone refuses to walk past, and `--filter=blob:limit` partial
# clones leave a missing-object set that gix/heddle import refuses to walk.
# Re-rooting via fast-import drops the parent edges at the boundary so every
# extracted fixture is a complete, walkable repository.
#
# A consequence: the recorded `commit` in `realworld_repos.toml` is the
# *post-rewrite* tip OID, not the upstream's tip. The rewrite is deterministic
# (fast-export preserves authorship + committer + timestamps), so re-running
# vendor.sh against the same upstream tip produces an identical fixture and
# OID.
#
# Disk and network: fetches happen in `$TMPDIR/heddle-vendor.$$`, repack
# happens in-place there, and the bare repository is tarballed into this
# directory. Running cleans up its scratch dir on exit.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRATCH="${TMPDIR:-/tmp}/heddle-vendor.$$"
trap 'rm -rf "$SCRATCH"' EXIT
mkdir -p "$SCRATCH"

# url, name, depth, branch
fixtures=(
    "https://github.com/GitoxideLabs/gitoxide.git|gix-shaped|50|main"
    "https://github.com/tokio-rs/tokio.git|tokio-shaped|100|master"
    "https://github.com/BurntSushi/ripgrep.git|ripgrep-shaped|200|master"
    # Deep merge-heavy DAG with a real gitlink (`sha1collisiondetection`).
    # This is what makes this fixture distinct from the others — it
    # exercises heddle's clone-time gitlink-skip path against an
    # honest-to-goodness submodule reference, not a synthetic one.
    "https://github.com/git/git.git|git-shaped|10|master"
)

vendor_one() {
    local url="$1" name="$2" depth="$3" branch="$4"
    local shallow="$SCRATCH/$name.shallow"
    local fresh="$SCRATCH/$name"
    local dump="$SCRATCH/$name.fi"
    local out="$SCRIPT_DIR/$name.tar.gz"

    echo "==> $name (depth=$depth branch=$branch)"
    rm -rf "$shallow" "$fresh" "$dump"

    git clone --bare \
        --depth="$depth" \
        --single-branch \
        --branch="$branch" \
        "$url" "$shallow"

    # Pull annotated tags into the shallow clone. `--no-tags` on the clone
    # (its prior default here) meant tags never arrived, so these fixtures
    # never carried an annotated tag at all. Fetch tags explicitly so the
    # real-world corpus exercises annotated-tag *structure* fidelity;
    # `--tag-of-filtered-object=drop` below still discards any tag whose target
    # fell outside the shallow window.
    git -C "$shallow" fetch --depth="$depth" origin 'refs/tags/*:refs/tags/*' \
        || echo "    (no tags fetched for $name)"

    git init --bare "$fresh" >/dev/null

    # `fast-export --signed-tags=strip --tag-of-filtered-object=drop`:
    #
    # These fixtures are SHALLOW clones re-rooted via fast-export|fast-import.
    # That rewrite re-roots the shallow boundary and rewrites commit IDs (see
    # the header note). A tag signature signs the tag object, which references
    # the commit OID — so once fast-import rewrites the referenced commit, any
    # preserved (`--signed-tags=verbatim`) signature no longer matches the tag
    # it is attached to: `git verify-tag` fails. Shipping such a tag would be
    # *worse* than honest stripping — a fixture that looks signed but isn't.
    # So we `strip` signatures from the real-world tags here; genuine signed
    # fidelity is covered by the directly-generated synthetic fixture in
    # `tests/roundtrip_fidelity_fixtures/` (never passed through fast-export,
    # so its signatures stay valid). We still keep the unsigned annotated-tag
    # *objects* (structure, tagger, message) for non-signature fidelity, and
    # `--tag-of-filtered-object=drop` discards any tag whose target object we
    # filtered out of the shallow window. `--all` is a no-op in a single-branch
    # shallow clone but keeps the script honest if we ever widen the clone.
    git -C "$shallow" fast-export --all \
        --signed-tags=strip \
        --tag-of-filtered-object=drop \
        > "$dump"
    # Gitlink (mode 160000) entries used to be stripped here because
    # they tripped heddle's clone-time reachability walk. With the
    # gitlink-skip in `git_core::collect_reachable_object_ids` they
    # round-trip natively, so we keep them in the fixture — that's
    # what makes git-shaped a real-world submodule test.
    git -C "$fresh" fast-import < "$dump" >/dev/null

    # Point the fresh bare repo's HEAD at the imported branch — `git init
    # --bare` defaults HEAD to init.defaultBranch (typically `main`) which
    # leaves HEAD dangling for any fixture cloned from `master`.
    git -C "$fresh" symbolic-ref HEAD "refs/heads/$branch"

    # The default fast-import pack is large and unoptimized; gc compacts it.
    git -C "$fresh" gc --aggressive --prune=now >/dev/null 2>&1

    local tip
    tip="$(git -C "$fresh" rev-parse HEAD)"
    local commits
    commits="$(git -C "$fresh" rev-list --count HEAD)"

    # Tarball layout: top-level directory matches the bare-repo dir name so
    # `tar xzf $name.tar.gz -C dest` produces `dest/$name/`.
    #
    # Determinism (so re-running vendor.sh against the same upstream tip
    # produces a byte-identical tarball and doesn't dirty the working tree):
    #   * `touch -t 200001010000.00` zeroes file mtimes in the tar headers.
    #     macOS's bsdtar lacks `--mtime`, so we do it on the filesystem.
    #   * `--uid 0 --gid 0` zeroes ownership in the tar headers.
    #   * `gzip -n` strips the original-filename + mtime header from the
    #     gzip wrapper.
    find "$fresh" -exec touch -t 200001010000.00 {} +
    (cd "$SCRATCH" && TZ=UTC tar --uid 0 --gid 0 -cf - "$name" | gzip -n -9 > "$out")

    local size
    size="$(du -h "$out" | cut -f1)"
    echo "    upstream=$branch  rewritten_tip=$tip  commits=$commits  size=$size"
    printf "%s\t%s\t%s\n" "$name" "$tip" "$commits" >> "$SCRATCH/manifest.tsv"
}

> "$SCRATCH/manifest.tsv"
for entry in "${fixtures[@]}"; do
    IFS='|' read -r url name depth branch <<< "$entry"
    vendor_one "$url" "$name" "$depth" "$branch"
done

echo
echo "Manifest (rewritten tip OIDs — paste into realworld_repos.toml):"
column -t -s $'\t' "$SCRATCH/manifest.tsv"
echo
echo "Total fixture size:"
du -ch "$SCRIPT_DIR"/*.tar.gz | tail -1
