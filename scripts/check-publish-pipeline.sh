#!/usr/bin/env bash
# Asserter for the crates.io auto-publish pipeline contract (heddle#72).
#
# The publish workflow runs only on push-to-main, so we can't observe its
# artifacts in normal PR CI. Instead we statically verify that
# `.github/workflows/publish-crates.yml` declares the contract every
# downstream consumer (release-plz PRs, crates.io, eventual library
# consumers) relies on:
#
#   - push-to-main trigger (and NO workflow_dispatch — automation must
#     never be triggerable from outside main's history)
#   - validate-publish trust gate runs first, emits validated outputs
#   - publish job depends on validate-publish and reads its outputs
#   - publish job pins checkout to validate-publish's commit SHA, not
#     the mutable refs/heads/main (closes the TOCTOU window — same
#     reason heddle#56's release.yml pins to tag_sha)
#   - publish job reads CARGO_REGISTRY_TOKEN from secrets.* via env var
#   - explicit publishable-crates list (no auto-discovery — implicit
#     `publish = true` in Cargo.toml is too easy to misconfigure)
#
# This mirrors scripts/check-release-pipeline.sh (heddle#56) and shares
# the same PyYAML venv fallback. The two asserters are independently
# runnable but follow the same two-pass shape.

set -euo pipefail

WF=".github/workflows/publish-crates.yml"
fail=0

err() { echo "::error::$*" >&2; fail=1; }
ok()  { echo "ok: $*"; }

if [[ ! -f "$WF" ]]; then
  err "$WF does not exist"
  echo "::error::Publish pipeline not implemented. See heddle#72."
  exit 1
fi

# --- Smoke (grep) ---------------------------------------------------------

# Push-to-main trigger. Strict: the workflow must fire on main-branch
# pushes only, never on tag pushes (those are heddle#56's release.yml)
# and never via workflow_dispatch (automation must not be triggerable
# from outside main's history; a force-publish is a deliberate ops
# action that should run locally with the maintainer's own creds).
if grep -E "^\s*push:" "$WF" >/dev/null \
   && grep -E "branches:" "$WF" >/dev/null \
   && grep -E "['\"]?main['\"]?" "$WF" >/dev/null; then
  ok "push-to-main trigger present"
else
  err "missing push-to-main trigger in $WF"
fi

# Explicit anti-pattern: workflow_dispatch creates a path for an
# operator (or an attacker with workflow-dispatch rights) to publish
# from a non-main commit. The grep is intentionally loose so renames or
# inline comments mentioning workflow_dispatch in a NOTE block still
# match — the strict pass (below) then re-verifies via parsed YAML.
if grep -E "^\s*workflow_dispatch:" "$WF" >/dev/null; then
  err "$WF must NOT declare workflow_dispatch (force-publish is a deliberate ops action; run cargo publish locally)"
else
  ok "no workflow_dispatch trigger (anti-pattern correctly absent)"
fi

# Trust gate: validate-publish job must run before publish and emit
# validated outputs. The structural shape is checked here; the strict
# pass verifies the outputs exist on the job and that publish reads
# from them.
if grep -E "^\s*validate-publish:" "$WF" >/dev/null; then
  ok "validate-publish job present"
else
  err "missing validate-publish job in $WF"
fi

if grep -E "^\s*publish:" "$WF" >/dev/null; then
  ok "publish job present"
else
  err "missing publish job in $WF"
fi

if grep -E "needs:\s*validate-publish|needs:\s*\[validate-publish" "$WF" >/dev/null; then
  ok "publish job depends on validate-publish"
else
  err "publish job must declare 'needs: validate-publish' so credentialed publish is gated on it"
fi

# Token wiring. We assert the exact secret name and the env var name —
# any rename on either side would silently break authentication, and
# the workflow would either fail loud at first publish or (worse) drop
# the auth header entirely depending on cargo's behavior.
if grep -F 'secrets.CARGO_REGISTRY_TOKEN' "$WF" >/dev/null; then
  ok "publish step reads secrets.CARGO_REGISTRY_TOKEN"
else
  err "$WF must reference secrets.CARGO_REGISTRY_TOKEN"
fi

if grep -E '^\s*CARGO_REGISTRY_TOKEN:' "$WF" >/dev/null; then
  ok "CARGO_REGISTRY_TOKEN env var declared"
else
  err "$WF must expose the token as the CARGO_REGISTRY_TOKEN env var (cargo's documented name)"
fi

# Explicit crate list. Auto-discovery via `cargo metadata --workspace`
# would publish whatever's currently marked publishable in Cargo.toml,
# which is invisible at PR review time. An explicit list (env var or
# matrix) makes adding a publishable crate a one-line workflow edit
# reviewed in PR. We look for a PUBLISHABLE_CRATES marker.
if grep -E "PUBLISHABLE_CRATES" "$WF" >/dev/null; then
  ok "explicit PUBLISHABLE_CRATES list present"
else
  err "$WF must maintain an explicit PUBLISHABLE_CRATES list (no auto-discovery — see heddle#72 design)"
fi

# RELEASING.md must document the auto-publish flow alongside heddle#56's
# binary release docs.
if [[ ! -f RELEASING.md ]]; then
  err "RELEASING.md is missing"
else
  if grep -F 'publish-crates.yml' RELEASING.md >/dev/null; then
    ok "RELEASING.md documents publish-crates.yml"
  else
    err "RELEASING.md must document publish-crates.yml (the auto-publish flow)"
  fi
fi

# --- Strict structural checks (parsed YAML) -------------------------------
#
# The grep-based checks above answer "does the pipeline mention X
# anywhere?" — useful as a smoke screen, but blind to per-job
# structure. The strict checks below parse publish-crates.yml and
# verify:
#
#   - validate-publish declares the documented outputs (commit_sha,
#     to_publish, has_publishes). Without commit_sha, the SHA pin in
#     the publish job has nothing to reference.
#   - publish job's actions/checkout pins ref to the validated
#     commit_sha, not refs/heads/main. main is mutable: between
#     validate-publish reading HEAD and the publish job checking out,
#     a force-push could redirect the publish to attacker-controlled
#     code. Pinning the SHA closes that window.
#   - publish job's `if:` references has_publishes so the whole job is
#     skipped on no-op merges (the common case).
#   - the trigger really is push-only (no workflow_dispatch slipped in
#     past the grep via a different shape).

ensure_pyyaml() {
  # Echo the python interpreter to use (with PyYAML importable), or
  # return non-zero. Lifted from scripts/check-release-pipeline.sh —
  # same venv fallback so PEP 668 distros don't break the asserter.
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

errors = []
oks = []

# Trigger shape. PyYAML parses `on:` to the literal True (it's a YAML
# boolean keyword), so we look it up under both keys to be safe.
on = wf.get("on") or wf.get(True) or {}
if not isinstance(on, dict):
    errors.append("workflow `on:` must be a mapping (push:...)")
else:
    push = on.get("push") or {}
    if not isinstance(push, dict):
        errors.append("workflow must trigger on push (with branches: [main])")
    else:
        branches = push.get("branches") or []
        if isinstance(branches, str):
            branches = [branches]
        if "main" not in branches:
            errors.append(f"push trigger must include 'main' branch (got {branches!r})")
        else:
            oks.append("push trigger restricted to main branch")
        if "tags" in push:
            errors.append("push trigger must not include tags (tag releases live in release.yml — heddle#56)")
    if "workflow_dispatch" in on:
        errors.append("workflow_dispatch must not be declared (force-publish is a deliberate ops action)")
    else:
        oks.append("workflow_dispatch correctly absent")

jobs = wf.get("jobs", {}) or {}

vp = jobs.get("validate-publish")
if not isinstance(vp, dict):
    errors.append("validate-publish job missing or malformed")
else:
    outs = vp.get("outputs", {}) or {}
    for out_name in ("commit_sha", "to_publish", "has_publishes"):
        if out_name not in outs:
            errors.append(
                f"validate-publish must declare a '{out_name}' output "
                f"(downstream publish job reads from it)"
            )
        else:
            oks.append(f"validate-publish exports {out_name} output")

pub = jobs.get("publish")
if not isinstance(pub, dict):
    errors.append("publish job missing or malformed")
else:
    needs = pub.get("needs", [])
    if isinstance(needs, str):
        needs = [needs]
    if "validate-publish" not in needs:
        errors.append("publish job does not declare 'needs: validate-publish' (would skip the trust gate)")
    else:
        oks.append("publish job declares needs: validate-publish")

    if_clause = pub.get("if", "")
    if "needs.validate-publish.outputs.has_publishes" not in str(if_clause):
        errors.append(
            "publish job's `if:` must reference "
            "needs.validate-publish.outputs.has_publishes so no-op merges skip the job entirely"
        )
    else:
        oks.append("publish job gates execution on has_publishes")

    # Checkout step must pin to the commit_sha output, not the mutable
    # refs/heads/main. Acting on main after validate-publish reads its
    # HEAD would re-resolve to whatever main points at when checkout
    # runs — a window where a force-push would redirect the publish.
    SHA_REF_OK = "${{ needs.validate-publish.outputs.commit_sha }}"
    MAIN_REF_BAD = "refs/heads/main"
    steps = pub.get("steps", []) or []
    checkouts = [
        s for s in steps
        if isinstance(s, dict)
        and isinstance(s.get("uses"), str)
        and s.get("uses", "").startswith("actions/checkout@")
    ]
    if not checkouts:
        errors.append("publish job has no actions/checkout step — cannot verify SHA pin")
    for s in checkouts:
        ref = (s.get("with") or {}).get("ref", "")
        if not isinstance(ref, str):
            ref = str(ref)
        if SHA_REF_OK not in ref:
            errors.append(
                f"publish job checks out '{ref}' instead of needs.validate-publish.outputs.commit_sha — TOCTOU on mutable main ref"
            )
        elif MAIN_REF_BAD in ref:
            errors.append(
                f"publish job mixes refs/heads/main with commit_sha ('{ref}') — refs/heads/main is mutable; remove it"
            )
        else:
            oks.append("publish job pins checkout to validated commit_sha")

    # The token must reach cargo as the CARGO_REGISTRY_TOKEN env var
    # (cargo's documented name) AND it must originate from
    # secrets.CARGO_REGISTRY_TOKEN. Wiring it under a different env
    # name silently breaks authentication.
    job_env = pub.get("env", {}) or {}
    token_envs = []
    if "CARGO_REGISTRY_TOKEN" in job_env:
        token_envs.append(("job", job_env["CARGO_REGISTRY_TOKEN"]))
    for s in steps:
        if not isinstance(s, dict):
            continue
        step_env = s.get("env", {}) or {}
        if "CARGO_REGISTRY_TOKEN" in step_env:
            token_envs.append((s.get("name") or s.get("id") or "step", step_env["CARGO_REGISTRY_TOKEN"]))
    if not token_envs:
        errors.append(
            "publish job must expose CARGO_REGISTRY_TOKEN as an env var "
            "(at job or step scope) so cargo publish can authenticate"
        )
    else:
        valid = [t for t in token_envs if "secrets.CARGO_REGISTRY_TOKEN" in str(t[1])]
        if not valid:
            errors.append(
                "CARGO_REGISTRY_TOKEN env var must read from secrets.CARGO_REGISTRY_TOKEN "
                f"(got {token_envs!r})"
            )
        else:
            oks.append("CARGO_REGISTRY_TOKEN wired from secrets.CARGO_REGISTRY_TOKEN")

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
  echo "publish-pipeline check FAILED" >&2
  exit 1
fi
echo "publish-pipeline check passed"
