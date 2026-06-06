# Distribution manifest substrate — decision doc

**Status:** Spike complete. **Recommendation: adopt a single reusable
"publish manifest to external repo" composite action, driven by a new
`publish-manifests` job in `release.yml`, authenticated with a
HeddleCo-owned GitHub App installation token.**
**Issue:** [HeddleCo/heddle#346](https://github.com/HeddleCo/heddle/issues/346)
**Blocks:** #232 (Homebrew), #233 (Scoop), #234 (apt — also blocked by #328).

## Question

When a stable `vX.Y.Z` tag publishes a GitHub Release, how should heddle
post a formula/manifest-update PR to each external package repo so that
the three packaging channels — Homebrew (#232), Scoop (#233), apt (#234)
— consume **one** shared mechanism instead of three divergent ones?

This is a substrate decision, not an implementation. The deliverable is
this doc. No production change to `release.yml`, and no external repos
are created here (see the guardrails in #346).

## TL;DR

- **Mechanism:** add one `publish-manifests` job to `release.yml`,
  gated on `needs.validate-tag.outputs.kind == 'stable'` (release.yml:328),
  that runs a `strategy.matrix` over channels. Each matrix leg renders
  its channel manifest, then calls a shared composite action that checks
  out the target repo and opens a PR via
  `peter-evans/create-pull-request`. Centralized rendering, one place to
  maintain, idempotent PRs.
- **Token:** a HeddleCo-owned **GitHub App** installed *only* on the
  tap/bucket repos, with `Contents: write` + `Pull requests: write` and
  nothing else. The job mints a 1-hour installation token at runtime via
  `actions/create-github-app-token`. The only stored secrets are the App
  ID + private key, in heddle's repo secrets — same shape as the existing
  `CRATES_IO_API_KEY` wiring (RELEASING.md:266-288). A **fine-grained PAT
  scoped to the two repos** is the documented bootstrap fallback. A
  classic/org-wide PAT is explicitly rejected.
- **Repos:** create `HeddleCo/homebrew-heddle` (tap) and
  `HeddleCo/scoop-heddle` (bucket). The apt *hosting substrate + GPG* is
  #328's call, not this spike's; this spike owns only the PR-posting
  wiring and shows how the two compose for #234.
- **Shared interface:** a composite action
  `.github/actions/publish-manifest` parameterized by `(target-repo,
  manifest-path, rendered-file, token, …)`. #232/#233/#234 each supply
  **only** their channel-specific manifest renderer + one matrix entry.

## What `release.yml` produces today (the inputs to this substrate)

Read against the current pipeline (`.github/workflows/release.yml`) and
its contract (`RELEASING.md`):

- **Trigger:** strict-semver `vX.Y.Z` tag push, or `workflow_dispatch`
  for RC dry-runs (release.yml:31-42). `validate-tag` is the single trust
  gate and emits `tag`, `tag_sha`, and `kind` (stable|prerelease)
  outputs (release.yml:62-67).
- **Build matrix:** six targets (release.yml:186-204). Each leg stages an
  archive plus a one-line `.sha256`, a cosign `.sig`, and a Fulcio `.pem`
  (release.yml:239-265), then uploads them as a per-target artifact
  (release.yml:282-292).
- **Release job:** downloads every build artifact (release.yml:305-308),
  flattens them and concatenates the per-archive `.sha256` files into a
  sorted aggregate `SHA256SUMS` (release.yml:310-316), then publishes the
  GitHub Release — `draft`/`prerelease` keyed off
  `validate-tag.outputs.kind` (release.yml:320-341).
- **Asset URL shape (deterministic from the tag):**
  `https://github.com/HeddleCo/heddle/releases/download/<tag>/heddle-<tag>-<target>.<ext>`
  (RELEASING.md:152-158). The asset filenames + `SHA256SUMS` layout are a
  declared downstream contract (RELEASING.md:137-145).

So the two facts a rendered manifest needs — **artifact URL** and
**sha256** — are both already produced: the URL is templatable from the
tag, and the sha256 of each `<target>` archive is one line of
`SHA256SUMS`. The substrate's job is to get those into a PR on the right
repo with the least privilege possible.

## Decision 1 — Auto-update mechanism

**Chosen: a new `publish-manifests` job in `release.yml` that renders each
manifest and opens a PR with `peter-evans/create-pull-request`, one
matrix leg per channel.**

Options compared:

| Option | How it works | Why not chosen |
|---|---|---|
| **(A) Centralized create-PR job** *(chosen)* | A job in heddle's `release.yml` renders the manifest and uses `peter-evans/create-pull-request` against a checkout of the target repo. | — |
| (B) `repository_dispatch` to each target repo | heddle fires a dispatch event carrying `{tag, version, sha256s}`; each tap repo runs its *own* workflow to render + open a PR with its in-repo `GITHUB_TOKEN`. | Does **not** avoid the cross-repo token — sending a `repository_dispatch` to another repo *also* needs a write-scoped token to that repo. It just adds a second workflow per channel to maintain and splits the SHA-flow across two repos. More surface, no security win. |
| (C) Hand-rolled `gh pr create` in a checkout step | A `run:` step clones the target, commits, `gh pr create`. | Re-implements branch naming, idempotent-update, and PR-body handling that `peter-evans/create-pull-request` already does (it updates an existing PR for the same branch rather than stacking duplicates on re-runs). More bespoke shell to keep correct across three channels. |

**Why (A):** rendering logic stays in one repo (heddle), the release job
already holds the version + artifacts, and `create-pull-request` is
idempotent on re-run (re-running a release updates the open PR instead of
opening a second). The cross-repo token is required by (A) and (B)
alike, so we pick the simpler topology and spend the effort minimizing
the token (Decision 2).

**Where it hooks in:** a new job

```yaml
publish-manifests:
  needs: [validate-tag, build, release]
  if: needs.validate-tag.outputs.kind == 'stable'   # release.yml:328
  runs-on: ubuntu-24.04
  permissions:
    contents: read            # this job does NOT need the workflow's
                              # contents:write — cross-repo writes come
                              # from the App token, not GITHUB_TOKEN
```

Two correctness/security points baked into that header:

1. **`if: kind == 'stable'`** — prerelease/draft dry-runs
   (`workflow_dispatch` RC runs) must **not** post manifest PRs to the
   public tap/bucket. We reuse the existing `kind` discriminator
   (release.yml:328-329) rather than inventing a new gate.
2. **`permissions: contents: read`** — the workflow's top-level
   `GITHUB_TOKEN` is `contents: write` + `id-token: write`
   (release.yml:44-46) for the *same* repo. The manifest job deliberately
   does the cross-repo write with the App token instead, so we never
   broaden `GITHUB_TOKEN` to reach other repos.

**SHA + URL flow into the manifest:** the job re-uses the
download-artifact + flatten pattern already in the release job
(release.yml:305-316) to obtain `SHA256SUMS` locally (preferred over
fetching from the just-published Release, which can race visibility).
The per-channel renderer reads each `<target>` line out of `SHA256SUMS`
and templates it alongside the deterministic URL
(`…/releases/download/<tag>/heddle-<tag>-<target>.<ext>`,
RELEASING.md:152) into the channel manifest.

## Decision 2 — Token / permission model (least privilege)

**Chosen: a HeddleCo-owned GitHub App installation token, scoped to the
tap/bucket repos only. Fine-grained PAT scoped to the same two repos is
the documented bootstrap fallback. Classic / org-wide PAT is rejected.**

The default `GITHUB_TOKEN` is scoped to the workflow's *own* repo and
**cannot** write to `HeddleCo/homebrew-heddle` or `HeddleCo/scoop-heddle`.
A cross-repo credential is therefore unavoidable; the only question is
how narrowly it can be scoped.

| Option | Scope | Identity | Expiry | Verdict |
|---|---|---|---|---|
| Classic PAT | every repo the granting user can touch | a person | up to no-expiry | **Rejected.** Org-wide blast radius; one leaked token writes everywhere. Contradicts least-privilege. |
| Fine-grained PAT, repo-scoped | only the named tap/bucket repos, `Contents: write` + `Pull requests: write` | a person | ≤ 1 year, manual | **Fallback.** Correct scope, but tied to a human account (lost if they leave the org) and forces annual manual rotation. |
| **GitHub App installation token** *(chosen)* | only repos the App is *installed* on, with exactly the App's declared permissions | the org-owned App (a bot) | **1 hour**, minted per run | **Chosen.** Narrowest workable scope; org-owned not person-owned; runtime token auto-expires so a leaked token's window is ~1h. |

**The recommended setup:**

- Create a GitHub App under the HeddleCo org (e.g. "Heddle Release
  Publisher"). Permissions: **`Contents: write`** + **`Pull requests:
  write`**, nothing else. No webhook.
- **Install it on only** `homebrew-heddle` and `scoop-heddle` (and, if
  #328 picks a git-backed apt repo, on that repo too). Installation scope
  *is* the access boundary — the App cannot touch any repo it isn't
  installed on, including heddle itself.
- Stored secrets in heddle: `HEDDLE_RELEASE_APP_ID` +
  `HEDDLE_RELEASE_APP_PRIVATE_KEY`. The job mints the short-lived token
  at runtime:

  ```yaml
  - uses: actions/create-github-app-token@v1
    id: app-token
    with:
      app-id: ${{ secrets.HEDDLE_RELEASE_APP_ID }}
      private-key: ${{ secrets.HEDDLE_RELEASE_APP_PRIVATE_KEY }}
      owner: HeddleCo
      repositories: homebrew-heddle,scoop-heddle
  # → use steps.app-token.outputs.token downstream
  ```

- **Rotation story:** regenerate the App's private key in the App
  settings and update the single `HEDDLE_RELEASE_APP_PRIVATE_KEY` secret —
  no workflow edit needed. This mirrors the crates.io rotation story
  already documented (RELEASING.md:287-288: "update the secret; no
  workflow change is needed"). Installation tokens additionally
  self-expire in 1h, so even without rotation a leaked *runtime* token
  dies fast.

**Why not the fine-grained PAT as primary:** it grants under a human's
identity (org access disappears when that person leaves), caps at a
1-year lifetime that forces a manual annual rotation, and a leaked PAT
lives until someone revokes it. The App is org-owned, its runtime tokens
are ephemeral, and its scope is the explicit installation set. The PAT
remains a perfectly fine **bootstrap** before the App exists — same
scope, faster to stand up — which is why it's the fallback, not a
co-equal.

## Decision 3 — External-repo strategy

**Chosen: create `HeddleCo/homebrew-heddle` and `HeddleCo/scoop-heddle`.
apt's hosting + GPG is #328; this spike owns only the PR-posting wiring
and states how they compose.**

| Repo | Holds | Why this name | Write access |
|---|---|---|---|
| `HeddleCo/homebrew-heddle` | `Formula/heddle.rb` | The `homebrew-` prefix is **mandatory** by Homebrew tap convention: `brew tap heddleco/heddle` resolves to `github.com/HeddleCo/homebrew-heddle`. | Release App + org admins only. Auto-update lands as a **PR**, merged by a human (so the tap's own CI runs first). |
| `HeddleCo/scoop-heddle` | `bucket/heddle.json` | `scoop bucket add heddle https://github.com/HeddleCo/scoop-heddle`; Scoop reads manifests from `bucket/`. | Same as above. |

Auto-updates post a **PR, not a direct push** — this keeps a human merge
gate and lets the target repo's own CI (formula audit, manifest lint)
run before the channel goes live. It also matches the ACs in #232/#233
("auto-updates … via posting a PR").

**Composition with #328 (apt), stated explicitly:**

- This spike (#346) owns the **PR-posting / publish wiring**. #328 owns
  the **apt hosting substrate** — platform (S3+CloudFront vs packagecloud
  vs cloudsmith), the GPG signing key + its storage/rotation, and the
  repo domain (e.g. `apt.heddle.sh`).
- The two compose at #234 along one of two branches, decided by #328:
  - **If #328 chooses a git-backed apt repo** (a `HeddleCo/apt-heddle`
    holding the pool + `Packages`/`Release` index synced to the host):
    #234 reuses **this spike's composite action** to post the `.deb` +
    reindex PR to that repo, exactly like Homebrew/Scoop. Install the
    Release App on that repo too.
  - **If #328 chooses a push-API host** (packagecloud/cloudsmith):
    there's no PR to open. #234 swaps the create-PR action for a
    channel-specific `push` step **in the same `publish-manifests` job
    slot** (same matrix shape, different leaf action). The shared
    interface (Decision 4) is intentionally general enough to allow this.
- Either way, #234 = #328's hosting decision **+** the GPG-signed index
  **+** this spike's job slot. #234 stays blocked by **both** #328 and
  #346.

## Decision 4 — Shared interface

**Chosen: a composite action `.github/actions/publish-manifest`, invoked
once per channel from the `publish-manifests` matrix. Channels supply
only a manifest renderer + a matrix entry.**

Composite action vs reusable workflow (`workflow_call`): a composite
action runs as steps inside the existing `publish-manifests` job, so the
App-token mint (Decision 2) happens once in the job and is passed in as
an input — cleaner than a reusable workflow that would need its own
`secrets:` plumbing and its own runner per channel. The channels differ
only in three values (target repo, in-repo manifest path, rendered
file), which is exactly a matrix.

**Division of ownership:**

- **Shared (this spike's wiring):** check out `target-repo` with the
  token, copy the rendered manifest into place, open/refresh the PR,
  surface the PR URL. Channel-agnostic.
- **Per channel (#232/#233/#234 own):** the *renderer* that turns
  `(version, per-target {url, sha256})` into the manifest text — a
  `Formula/heddle.rb` for Homebrew, a `bucket/heddle.json` for Scoop —
  plus one matrix entry.

### Proposed composite action — parameter surface

```yaml
# .github/actions/publish-manifest/action.yml   (PROPOSED — not wired here)
name: publish-manifest
description: Open/refresh a manifest-update PR on an external package repo.
inputs:
  target-repo:    { required: true,  description: "owner/name, e.g. HeddleCo/homebrew-heddle" }
  manifest-path:  { required: true,  description: "path WITHIN target-repo, e.g. Formula/heddle.rb" }
  rendered-file:  { required: true,  description: "path in THIS workspace to the rendered manifest" }
  token:          { required: true,  description: "App installation token (Contents+PR write on target-repo)" }
  tag:            { required: true,  description: "release tag, e.g. v0.3.0" }
  channel:        { required: true,  description: "label for branch/PR text, e.g. homebrew" }
  pr-title:       { required: false, default: "${{ inputs.channel }}: heddle ${{ inputs.tag }}" }
  pr-body:        { required: false, default: "Automated manifest update for heddle ${{ inputs.tag }}." }
  commit-message: { required: false, default: "${{ inputs.channel }}: heddle ${{ inputs.tag }}" }
outputs:
  pull-request-url:    { value: "${{ steps.cpr.outputs.pull-request-url }}" }
  pull-request-number: { value: "${{ steps.cpr.outputs.pull-request-number }}" }
runs:
  using: composite
  steps:
    - uses: actions/checkout@v4
      with: { repository: ${{ inputs.target-repo }}, token: ${{ inputs.token }}, path: __target }
    - shell: bash
      run: install -D "${{ inputs.rendered-file }}" "__target/${{ inputs.manifest-path }}"
    - id: cpr
      uses: peter-evans/create-pull-request@v6
      with:
        path: __target
        token: ${{ inputs.token }}
        branch: heddle-release/${{ inputs.channel }}-${{ inputs.tag }}
        title: ${{ inputs.pr-title }}
        body: ${{ inputs.pr-body }}
        commit-message: ${{ inputs.commit-message }}
```

### Proposed caller in `release.yml` (PROPOSED — not wired here)

```yaml
publish-manifests:
  needs: [validate-tag, build, release]
  if: needs.validate-tag.outputs.kind == 'stable'
  runs-on: ubuntu-24.04
  permissions: { contents: read }
  strategy:
    fail-fast: false
    matrix:
      include:
        - channel: homebrew
          target-repo: HeddleCo/homebrew-heddle
          manifest-path: Formula/heddle.rb
          renderer: scripts/render-homebrew-formula.sh   # OWNED BY #232
        - channel: scoop
          target-repo: HeddleCo/scoop-heddle
          manifest-path: bucket/heddle.json
          renderer: scripts/render-scoop-manifest.sh      # OWNED BY #233
  steps:
    - uses: actions/checkout@v4
      with: { ref: ${{ needs.validate-tag.outputs.tag_sha }} }   # SHA pin, release.yml:303
    - uses: actions/download-artifact@v4
      with: { path: staging }
    - name: Flatten SHA256SUMS                         # reuse release.yml:310-316
      run: |
        set -euo pipefail
        mkdir -p dist
        find staging -type f -name '*.sha256' -exec cp -t dist {} +
        ( cd dist && cat *.sha256 | sort > SHA256SUMS )
    - name: Render manifest                             # channel-specific renderer
      run: bash "${{ matrix.renderer }}" "${{ needs.validate-tag.outputs.tag }}" dist/SHA256SUMS > rendered.manifest
    - uses: actions/create-github-app-token@v1
      id: app-token
      with:
        app-id: ${{ secrets.HEDDLE_RELEASE_APP_ID }}
        private-key: ${{ secrets.HEDDLE_RELEASE_APP_PRIVATE_KEY }}
        owner: HeddleCo
        repositories: ${{ matrix.target-repo == 'HeddleCo/homebrew-heddle' && 'homebrew-heddle' || 'scoop-heddle' }}
    - uses: ./.github/actions/publish-manifest
      with:
        target-repo: ${{ matrix.target-repo }}
        manifest-path: ${{ matrix.manifest-path }}
        rendered-file: rendered.manifest
        token: ${{ steps.app-token.outputs.token }}
        tag: ${{ needs.validate-tag.outputs.tag }}
        channel: ${{ matrix.channel }}
```

(The `repositories:` ternary is shown literally for clarity; the impl can
carry the bare repo name as a matrix field instead.)

## Decision 5 — Recommendation + follow-up shape

**Recommendation (summary):**

1. **Mechanism:** a `publish-manifests` matrix job in `release.yml`,
   gated `kind == 'stable'`, rendering per channel then opening a PR via a
   shared composite action + `peter-evans/create-pull-request`.
2. **Token:** HeddleCo GitHub App installed only on the tap/bucket repos,
   `Contents: write` + `Pull requests: write`, runtime 1-hour token via
   `actions/create-github-app-token`; secrets `HEDDLE_RELEASE_APP_ID` +
   `HEDDLE_RELEASE_APP_PRIVATE_KEY`. Fine-grained repo-scoped PAT as
   bootstrap fallback. No classic/org PAT.
3. **Repos:** create `HeddleCo/homebrew-heddle` + `HeddleCo/scoop-heddle`.
4. **Shared interface:** composite action `.github/actions/publish-manifest`
   with the parameter surface above.

**Proposed per-channel consumption (a PROPOSAL for orchestrator/user to
confirm — NOT filed here):**

- **#232 (Homebrew)** — residual scope becomes: write
  `scripts/render-homebrew-formula.sh` (emits `Formula/heddle.rb` with the
  macOS arm64+intel and Linux blocks, each `sha256` pulled from
  `SHA256SUMS`, plus the cosign-verify install incantation,
  RELEASING.md:165-170) + add the `homebrew` matrix entry + tap README.
  Drop its "create the repo" / "add the release-step" ACs — now owned by
  #346.
- **#233 (Scoop)** — residual: write `scripts/render-scoop-manifest.sh`
  (emits `bucket/heddle.json` for x64 + arm64, optional `autoupdate`/
  `checkver`) + add the `scoop` matrix entry + bucket README. Stays
  blocked by **#347** (the arm64 build leg) for its arm64 coverage.
- **#234 (apt)** — residual: `.deb` build from the release artifacts +
  GPG-signed index, then **either** (git-backed host) add an `apt` matrix
  entry reusing the composite action, **or** (push-API host) a
  channel-specific push step in the same job slot. Stays blocked by
  **both #328** (hosting + GPG) **and #346**.

**Impl-time follow-ups to flag (not part of any channel's renderer):**

- The pipeline-contract asserter (`scripts/check-release-pipeline.sh`,
  RELEASING.md:191-212) and the `RELEASING.md` artifact-contract section
  should grow a clause for the `publish-manifests` job when it lands, so
  the new credentialed cross-repo job is statically asserted like the
  rest of the pipeline.

## Why this is design-only / what was NOT done

- No change to `release.yml`; the job + action above are *sketches* in
  this doc, not wired.
- No external repos created; `homebrew-heddle`/`scoop-heddle` are
  proposed, not stood up.
- No GitHub App created; the secret names are proposed.
- #232/#233/#234 bodies untouched; the consumption shapes above are
  proposals for the orchestrator to confirm before any filing.

**Doc-only change** — no `cargo build`/test applies. (`heddle doctor
docs`, if it lints `docs/design/`, was not run for this prose design
doc.)

## Pointers

- Current release pipeline: `.github/workflows/release.yml`
  (matrix build 178-292; release/aggregate 294-342; `kind` gate 328-329).
- Release contract + verify recipe + token-rotation precedent:
  `RELEASING.md` (artifact contract 111-145; cosign verify 165-170;
  crates.io token wiring + rotation 266-288; pipeline-contract check
  191-212).
- Pipeline-contract asserter: `scripts/check-release-pipeline.sh`.
- Blocked impls: #232 (Homebrew), #233 (Scoop), #234 (apt).
- Sibling spike (apt hosting + GPG): #328. Build-matrix arm64 dep: #347.
