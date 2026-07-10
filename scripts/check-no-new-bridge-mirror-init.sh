#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Grep-gate: forbid *new* Bridge Mirror creation outside git-projection
# residual/migration modules.
#
# Existing code may still *read* `.heddle/git` for migration, fsck, and tests.
# Creating a mirror (`init_mirror` / `init_mirror_with_guard` call sites, or
# bare `SleyRepository::init_bare` of the Bridge Mirror path) must stay inside
# `crates/git-projection/` until residuals fully replace the mirror.
#
# Prefer Raw Git Object Residuals (`ResidualStore`) for non-reconstructable
# Git object bytes. See CONTEXT.md and docs/adr/0042-retire-persistent-bridge-mirror.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ALLOW_PREFIX='crates/git-projection/'

violations=0

# init_mirror / init_mirror_with_guard *call sites* outside git-projection.
# Definition sites are included; both must remain in git-projection.
while IFS= read -r match; do
  [[ -z "$match" ]] && continue
  file="${match%%:*}"
  case "$file" in
    ${ALLOW_PREFIX}*) continue ;;
    scripts/check-no-new-bridge-mirror-init.sh) continue ;;
  esac
  echo "forbidden init_mirror outside crates/git-projection/: $match" >&2
  violations=$((violations + 1))
done < <(rg -n --glob '*.rs' --glob '!**/tests/**' '(\.|::)?init_mirror(_with_guard)?\s*\(' crates || true)

# Bare-init of a path that is clearly the Bridge Mirror (join("git") near
# heddle_dir) outside git-projection production code.
while IFS= read -r match; do
  [[ -z "$match" ]] && continue
  file="${match%%:*}"
  case "$file" in
    ${ALLOW_PREFIX}*) continue ;;
    scripts/check-no-new-bridge-mirror-init.sh) continue ;;
  esac
  echo "forbidden Bridge Mirror init_bare outside crates/git-projection/: $match" >&2
  violations=$((violations + 1))
done < <(rg -n --glob '*.rs' --glob '!**/tests/**' 'init_bare\([^\)]*join\("git"\)' crates || true)

if [[ "$violations" -gt 0 ]]; then
  echo "check-no-new-bridge-mirror-init: $violations violation(s)" >&2
  echo "New Bridge Mirror creation belongs only in crates/git-projection residual/migration code." >&2
  echo "Prefer ResidualStore for Raw Git Object Residuals." >&2
  exit 1
fi

echo "check-no-new-bridge-mirror-init: ok"
