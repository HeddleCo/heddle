# apt repo hosting + GPG signing-key strategy — decision doc

**Status:** Spike complete. **Recommendation: host the apt channel as a
git-backed repo `HeddleCo/apt-heddle` (pool + signed index in git, served
as static files via GitHub Pages at `apt.heddle.sh`), GPG-signed in
heddle's `release.yml` with an Ed25519 key whose primary is generated
offline and whose signing-only subkey lives in a GitHub Actions secret,
distributed through a `heddle-archive-keyring` package and installed with
the modern `signed-by=` recipe.**
**Issue:** [HeddleCo/heddle#328](https://github.com/HeddleCo/heddle/issues/328)
**Composes with:** [#346](https://github.com/HeddleCo/heddle/issues/346)
(merged — shared PR-posting substrate) and its impl #547.
**Unblocks:** #234 (apt channel impl) — which stays blocked by **both**
#328 (this) and #547/#346.

## Question

#234 wants `apt install heddle` for Debian/Ubuntu users. Two substrate
decisions sit under it, and #346 deliberately split them:

- **#346 owns the PR-posting / publish wiring** — the `publish-manifests`
  job in `release.yml` plus the `.github/actions/publish-manifest`
  composite action (impl tracked in #547).
- **#328 (this doc) owns the apt HOSTING substrate + GPG** — *where* the
  pool and `Release` index live and are served, *which* GPG key signs the
  index and *where that private key lives*, and the *stable domain* users
  point `apt` at.

#346 §"Composition with #328" frames #234 as taking exactly one of two
branches, *decided here*:

- **git-backed apt repo** → #234 reuses #547's composite action to post a
  `.deb` + reindex **PR** to that repo (same shape as Homebrew/Scoop).
- **push-API host** (packagecloud/cloudsmith) → #234 uses a
  channel-specific **push step** in the same `publish-manifests` job slot
  (no PR to open).

This doc decides the hosting platform (and therefore the branch), the GPG
strategy, and the domain. It is **design-only** — no `release.yml` edit,
no repo/bucket/key creation, no #234 body edit. The proposed YAML below
is a sketch.

## What `release.yml` already produces (the inputs to a `.deb`)

Read against the current pipeline:

- Native matrix build of six targets (`release.yml:186-204`). The two
  Linux legs — `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
  (`release.yml:193-198`) — are the inputs a `.deb` is built from: each
  emits a `heddle-<tag>-<target>.tar.gz` containing the `heddle` binary +
  `README.md`/`LICENSE`/`NOTICE` (`release.yml:239-251`), a one-line
  `.sha256`, a cosign `.sig`, and a Fulcio `.pem`.
- The `release` job aggregates a sorted `SHA256SUMS` and publishes the
  GitHub Release, `draft`/`prerelease` keyed off
  `validate-tag.outputs.kind` (`release.yml:310-341`).
- Deterministic asset URL:
  `https://github.com/HeddleCo/heddle/releases/download/<tag>/heddle-<tag>-<target>.tar.gz`
  (`RELEASING.md:152`), with the filename + `SHA256SUMS` layout a declared
  downstream contract (`RELEASING.md:137-145`) that explicitly names "apt
  `.deb` metadata" as a consumer.

So the bytes a `.deb` needs already exist; #328's job is to decide where
the *packaged + signed apt index* lives and who holds the signing key.

> **Cross-cutting risk to flag for #234/#347 (not a #328 decision):** the
> two Linux legs build on `ubuntu-24.04` / `ubuntu-24.04-arm`
> (`release.yml:194-197`), i.e. **glibc 2.39**. #234's AC targets Debian
> 12 (glibc 2.36) and Ubuntu 22.04 (glibc 2.35). A binary dynamically
> linked against 2.39 fails on those with `GLIBC_2.3x not found` — it will
> `apt install` cleanly and then crash at runtime. The hosting/GPG
> decision is independent of this, but #234 must pin the Linux release
> build to an older glibc floor (build on `ubuntu-22.04`, or use
> `cargo-zigbuild`/a musl target). Surfaced here so it isn't discovered
> after the channel is "done".

## Decision 1 — Hosting platform

**Chosen: a git-backed repo `HeddleCo/apt-heddle` holding the pool +
GPG-signed `Packages`/`Release` index, served as static files via GitHub
Pages behind `apt.heddle.sh`.**

Options compared. Axes that decide it: **signing control** (do WE hold the
private key, or does a third party custody it?), **cost at low volume**,
**ops burden / new credential classes**, **arm64+amd64**, and **which #346
branch it implies**.

| Option | How it works | Signing control | Cost (low volume) | New credential / infra | arm64+amd64 | #346 branch |
|---|---|---|---|---|---|---|
| **git-backed `apt-heddle` + Pages** *(chosen)* | `aptly`/`reprepro` builds pool+index **in `release.yml`**, GPG-signs `Release`, posts a PR to `apt-heddle`; Pages serves the merged tree. | **Full — key lives only in heddle CI + offline primary.** | **Free** (Pages). | Reuses #346's GitHub App (+ one GPG secret). **No new vendor/billing.** | Yes — `Architectures: amd64 arm64`, both `.deb`s in the pool. | **git-backed → reuse #547 composite action (PR).** |
| S3 + CloudFront | `aptly`/`reprepro` builds+signs locally, `aws s3 sync` to a bucket, CloudFront for HTTPS + custom domain. | **Full — key in heddle CI.** | Low single-digit $/mo after CF free tier; S3 storage trivial. | AWS account + bucket policy + CloudFront distro + ACM cert + an `AWS_*` deploy credential (ideally OIDC role). | Yes. | push/sync step (NOT the composite-action PR — it's a sync, not a PR). |
| packagecloud.io | `package_cloud push …`; platform generates+signs the index. | **Weak — platform custodies a per-repo signing key** (you fetch its public key); your identity is the platform's key. | Free tier 2 GB storage / 10 GB bandwidth; **Starter $89/mo** beyond. | New SaaS account + push token. | Yes. | push-API → push step. |
| cloudsmith.io | `cloudsmith push deb …`; managed index + signing. | **Partial — BYO GPG supported, but the private key (or its passphrase) is uploaded to Cloudsmith's infra.** | **OSS tier free: 50 GB storage / 200 GB bandwidth** (requires README attribution). | New SaaS account + push token + uploaded key. | Yes. | push-API → push step. |

**Why git-backed wins for heddle specifically:**

1. **One shared publish mechanism, not a divergent integration.** #346
   built the composite-action PR substrate *expressly* so all three
   channels (Homebrew/Scoop/apt) converge on one mechanism. A git-backed
   apt repo reuses it verbatim — #234 becomes "build `.deb` + reindex +
   sign, then post the PR" exactly parallel to the other two. A push-API
   host instead forks a second, channel-specific integration. Fewer
   moving parts, one place to maintain.
2. **We hold the GPG key, fully.** For a security-adjacent, signature-
   centric project (cosign keyless on every binary, signed-by keyrings),
   ceding the apt index signing identity to a SaaS — whether the
   platform's own key (packagecloud) or our key uploaded into their infra
   (cloudsmith) — is the wrong trade. Git-backed keeps the private key in
   exactly one trusted place: heddle's release CI (Decision 2), with the
   primary generated offline.
3. **Zero new vendor / billing relationship.** It reuses #346's GitHub
   App credential class and adds only one GPG secret — no new SaaS
   account, no new dollar line, no third-party uptime in the install path.
   Cloudsmith's OSS tier is genuinely generous (50 GB / 200 GB) and is the
   strongest SaaS fallback, but it still puts a third party in the trust +
   availability path for a thing we can host for free.
4. **Human merge gate, same as Homebrew/Scoop.** The auto-update lands as
   a PR a human merges; `apt-heddle`'s own CI can lint the index before it
   goes live. Matches #346 Decision 3's posture.

**Decoupling that de-risks the one real downside.** The git repo is the
**authoring substrate** (canonical pool + signed index); the **serving
layer** is separable. Recommend GitHub Pages now (free, zero new
credentials); if traffic ever outgrows Pages' soft bandwidth ceiling,
swap the serving layer to S3+CloudFront **without changing the authoring
model or the `apt.heddle.sh` URL** — the repo stays the source of truth.

**Downside acknowledged + mitigation:** committing `.deb` binaries to git
grows the repo over time (~2 arches × ~10–20 MB per release). Mitigation:
the reindex step prunes the pool to the last *N* releases (`aptly`
supports retention), keeping `apt-heddle` well under GitHub's repo soft
limits; older versions remain available as GitHub Release assets on
heddle itself regardless. Document the retention N at impl time.

**Signing happens in heddle's `release.yml`, not in `apt-heddle`.** The
`.deb` build + `aptly`/`reprepro` index generation + `Release` signing run
in heddle's release pipeline (the only place the GPG secret lives). The PR
to `apt-heddle` carries an **already-signed** pool+index, so `apt-heddle`
needs **no secrets at all** — it's a dumb static-content repo. This keeps
the signing key in one trust context (alongside cosign + the crates.io
token) and off the serving repo entirely.

## Decision 2 — GPG signing-key strategy (least privilege)

The apt `Release` file must be detached-signed (`Release.gpg`, and/or an
inline-signed `InRelease`) with a project GPG key that subscribers pin via
`signed-by=`. This is security-sensitive; each sub-decision is justified.

### Key type/size — **Ed25519** (sign-only), rsa4096 as the compat fallback

- **Ed25519**: small, fast, modern; verified by `gpgv` on every distro in
  #234's AC set — Debian 12 (bookworm, gpg 2.2+) and Ubuntu 22.04/24.04 all
  support Ed25519 `Release` signatures. Matches the project's modern
  posture (cosign keyless, `signed-by` keyrings).
- The only clients that can't verify Ed25519 are pre-2018, EOL releases
  (Debian 9, Ubuntu 16.04) — outside the stated target set and outside a
  pre-0.3 project's support window.
- **rsa4096 is the documented fallback** if the support matrix ever has to
  reach those older clients; the per-release signing cost difference is
  negligible. Decision is reversible at key-generation time.

### Key structure — **offline primary, signing-only subkey in CI**

This is the load-bearing security decision.

- Generate the **primary key (certify capability) offline**, on a
  non-CI machine. The primary **never** goes to CI.
- Create a **signing-only subkey**. Export **only the subkey's** private
  material (`gpg --export-secret-subkeys`) for CI.
- **Blast radius if the CI secret leaks:** an attacker can sign packages
  until the subkey is revoked — but **cannot** certify a new identity as
  us, cannot alter the key's identity, and we retain the offline primary
  to **revoke the subkey and mint a replacement** without users having to
  re-trust a new primary. The trust anchor users pinned survives the
  incident.

This is the least-privilege middle ground the brief asked for: a real
stored secret (apt signing, unlike cosign, can't be keyless), but bounded
to a revocable, replaceable subkey.

### Where the private subkey lives — **GitHub Actions secret** (mirrors crates.io)

| Option | Custody | Verdict |
|---|---|---|
| **GitHub secret holding the signing subkey** *(chosen)* | ASCII-armored signing **subkey** in `HEDDLE_APT_GPG_PRIVATE_KEY` (+ `HEDDLE_APT_GPG_PASSPHRASE` if the subkey is passphrase-protected), in **heddle** repo secrets. | **Chosen.** Same trust context + rotation ergonomics as the existing `CRATES_IO_API_KEY` (`RELEASING.md:266-288`); no new infra; subkey-only structure already bounds the blast radius. |
| KMS / cloud HSM | Key material non-extractable; signing is an API call. | **Future hardening, not now.** KMS emits raw RSA/ECDSA, **not** OpenPGP — needs a PGP-over-KMS shim (sequoia/`gpg`-KMS backend) + a cloud account + IAM. Disproportionate for a pre-0.3 OSS CLI; revisit if the threat model escalates. |
| Private key committed / broadly shared | — | **Rejected** outright. Never commit the private key; never widen its reach. |

**Publish path (least privilege at the step level).** The signing step
imports the subkey into an **ephemeral `GNUPGHOME`**, signs `Release`,
then the runner is discarded. The GPG secret is exposed to **only** that
one job/step. Per #346 Decision 2, that job keeps `GITHUB_TOKEN` at
`contents: read` — the cross-repo write to `apt-heddle` uses the GitHub
App installation token, never `GITHUB_TOKEN`. So the credentials are
split by purpose: **App token** = cross-repo PR write; **GPG subkey** =
index signature; neither can do the other's job.

### Rotation story

Two cases, and the keyring-package distribution (Decision 3) is what makes
both tractable:

1. **Subkey rotation (routine, or on CI-secret compromise).** Revoke the
   old subkey with the offline primary, mint a new signing subkey, update
   the single `HEDDLE_APT_GPG_PRIVATE_KEY` secret — **no workflow edit**,
   mirroring the crates.io rotation precedent ("update the secret; no
   workflow change needed", `RELEASING.md:287-288`). Because the
   **primary is unchanged**, subscribers' pinned keyring (which contains
   the primary + its subkeys) still chains to the new subkey **once they
   receive the updated keyring** — shipped through the keyring package
   (below). Runtime installation tokens and the subkey's own short life
   mean a leaked *runtime* artifact dies fast.
2. **Primary rotation (catastrophic — primary compromised, or algorithm
   migration).** The hard case: subscribers pinned the **old primary** in
   `signed-by=`. Use the standard Debian archive-key playbook: ship the
   new primary **inside the `heddle-archive-keyring` package**, run a
   **dual-sign overlap window** (sign `Release` with **both** old and new
   keys), push the keyring-package update **signed by the still-trusted
   old key**, let subscribers `apt upgrade` into the new trust anchor,
   then drop the old key after the overlap. Manual `signed-by=` installers
   (who fetched a bare key file) must re-fetch on a primary rotation —
   called out in the install docs.

## Decision 3 — Repo domain + install UX

### Domain — **`apt.heddle.sh`**

A stable, branded apt URL CNAME'd to GitHub Pages. It is **decoupled from
the serving backend** (Decision 1): Pages today, S3+CloudFront later if
needed, with the user-facing URL unchanged. Rejected alternative:
platform-provided URLs (`packages.packagecloud.io/heddleco/…`,
`dl.cloudsmith.io/…`) — they bind the public install recipe to a vendor we
chose not to depend on, and a later migration breaks every user's
`sources.list`.

> **Human infra to-do:** secure/confirm the `heddle.sh` apex and create
> the `apt.heddle.sh` DNS record (CNAME → `heddleco.github.io`) + the Pages
> custom-domain + TLS. `apt.heddle.sh` is used here as the project's
> presumed domain (it is the example #346 §Composition already used); if
> the owned domain differs, substitute it everywhere — the design is
> domain-agnostic.

### Install recipe — modern `signed-by=` keyring (NOT `apt-key add`)

`apt-key` is deprecated/removed on modern apt. The recipe pins heddle's
key to **its own keyring** and scopes trust to heddle's source via
`signed-by=` (so the key can sign **only** heddle's repo, not every apt
source on the machine).

**Recommended — keyring-package path (self-updating trust anchor):**

```bash
# 1. Install heddle's signing key into its own keyring (dearmored/binary).
curl -fsSL https://apt.heddle.sh/heddle-archive-keyring.gpg \
  | sudo tee /usr/share/keyrings/heddle-archive-keyring.gpg > /dev/null

# 2. Register the source, pinned to that keyring + this machine's arch.
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/heddle-archive-keyring.gpg] https://apt.heddle.sh stable main" \
  | sudo tee /etc/apt/sources.list.d/heddle.list > /dev/null

# 3. Install. The heddle-archive-keyring package then keeps the key
#    current across future `apt upgrade`s (see rotation, Decision 2).
sudo apt update && sudo apt install heddle
```

**Modern deb822 alternative** (`/etc/apt/sources.list.d/heddle.sources`):

```
Types: deb
URIs: https://apt.heddle.sh
Suites: stable
Components: main
Architectures: amd64 arm64
Signed-By: /usr/share/keyrings/heddle-archive-keyring.gpg
```

- **Suite layout:** a single rolling `stable main` suite carrying both
  `amd64` + `arm64` (the binaries are glibc-dynamic, not codename-
  specific — subject to the glibc-floor fix flagged above), rather than
  per-codename suites. Simpler for users and for the reindex step;
  revisit only if a future `.deb` gains a distro-specific dependency.
- **`heddle-archive-keyring.gpg`** published at the repo root is the
  **dearmored public key** (the primary + current signing subkey). The
  same key is wrapped in the `heddle-archive-keyring` `.deb` so the trust
  anchor self-updates; the bare file serves manual installers.

## Decision 4 — Recommendation + follow-up shape

**Recommendation (summary):**

1. **Hosting:** git-backed `HeddleCo/apt-heddle` (pool + signed index in
   git), served via **GitHub Pages at `apt.heddle.sh`**. Signing +
   indexing happen in heddle's `release.yml`; `apt-heddle` holds no
   secrets.
2. **GPG:** **Ed25519** (rsa4096 fallback), **offline primary +
   signing-only subkey** exported to a **GitHub Actions secret**
   (`HEDDLE_APT_GPG_PRIVATE_KEY`), ephemeral `GNUPGHOME` at sign time,
   `heddle-archive-keyring` package as the rotation/distribution vehicle.
3. **Domain + UX:** `apt.heddle.sh`; modern `signed-by=` keyring recipe
   (+ deb822 form); no `apt-key add`.

**Which #346 composition branch #234 takes:**
**the git-backed branch** — #234 reuses **#547's composite action** to
post the `.deb` + reindexed, signed pool to `apt-heddle` as a PR, in the
same `publish-manifests` matrix slot as Homebrew/Scoop (#346 §"Composition
with #328", first bullet). **Not** the push-API push-step branch.

**Proposed `publish-manifests` apt leg (SKETCH — not wired here).** Slots
into #346's matrix (`distribution-manifest-substrate-spike.md:289-335`):

```yaml
# add to the publish-manifests matrix once #547 lands:
- channel: apt
  target-repo: HeddleCo/apt-heddle
  manifest-path: .              # whole pool+index tree, not a single file
  renderer: scripts/build-apt-pool.sh   # OWNED BY #234

# apt-only steps inside the leg, before the shared composite action:
- name: Build .deb (amd64 + arm64) from release artifacts   # OWNED BY #234
  run: bash scripts/build-deb.sh "${{ needs.validate-tag.outputs.tag }}" dist/
- name: Import signing subkey into ephemeral GNUPGHOME
  env:
    GPG_KEY: ${{ secrets.HEDDLE_APT_GPG_PRIVATE_KEY }}
  run: |
    export GNUPGHOME="$(mktemp -d)"
    printf '%s' "$GPG_KEY" | gpg --batch --import
- name: Reindex + sign Release (aptly/reprepro)              # OWNED BY #234
  run: bash scripts/build-apt-pool.sh "${{ needs.validate-tag.outputs.tag }}"
  # → then the shared ./.github/actions/publish-manifest posts the PR,
  #   using the App token already minted in the job (Decision 2 of #346).
```

**Proposed #234 residual scope (a PROPOSAL for the orchestrator/user to
confirm — NOT filed, #234 body untouched):**

- Build `.deb` for `amd64` + `arm64` from the existing
  `*-unknown-linux-gnu` release archives (**after** resolving the
  glibc-floor risk above).
- Generate the pool + `Packages`/`Release` index with `aptly`/`reprepro`;
  GPG-sign `Release` (Ed25519 subkey, ephemeral `GNUPGHOME`).
- Add the `apt` matrix leg reusing **#547's composite action** to PR the
  signed tree to `apt-heddle`.
- Build + publish the `heddle-archive-keyring` package; publish the
  dearmored public key at the repo root.
- README/docs: the `signed-by=` install recipe above (modern form only).
- #234 stays **blocked by both #328 (this) and #547/#346**.

**Human infra to-dos (flagged, NOT actioned here):**

1. **DNS/Pages:** secure `heddle.sh`; create `apt.heddle.sh` CNAME →
   `heddleco.github.io` + Pages custom-domain + TLS.
2. **Repo:** create `HeddleCo/apt-heddle`; **install the #346 "Heddle
   Release Publisher" GitHub App on it** (per #346 Decision 2 — the App's
   installation set is the access boundary), `Contents: write` +
   `Pull requests: write`.
3. **Key generation:** offline-generate the Ed25519 **primary**, create the
   signing **subkey**, export the subkey →
   `HEDDLE_APT_GPG_PRIVATE_KEY` (+ passphrase secret) in **heddle** repo
   secrets; publish the dearmored public key + `heddle-archive-keyring`
   package.
4. **glibc floor:** decide the Linux release-build glibc floor (build on
   `ubuntu-22.04` / `cargo-zigbuild` / musl) — a #234/#347 impl item, not a
   #328 call, but a hard prerequisite for a *working* `apt install heddle`.

## Why this is design-only / what was NOT done

- No `release.yml` edit; the matrix leg + steps above are **sketches**.
- No repos/buckets created (`apt-heddle` is proposed), no GPG key
  generated (secret names are proposed), no DNS configured.
- **#234's body is untouched**; the residual scope is a proposal for the
  orchestrator to confirm before any filing. **No new sub-issues filed.**
- **Doc-only change** — no `cargo build`/test applies. `heddle doctor
  docs`, if it lints `docs/design/`, was not run for this prose design
  doc.

## Pointers

- Shared publish substrate this composes with:
  `docs/design/distribution-manifest-substrate-spike.md` (§"Composition
  with #328" :205-224; matrix sketch :289-335; App-token model :131-187).
- Release pipeline: `.github/workflows/release.yml` (Linux build legs
  :193-198; runner glibc source :194-197; aggregate/publish :294-341).
- Release contract + secret/rotation precedent: `RELEASING.md`
  (artifact contract naming apt :137-145; verify recipe :147-171;
  crates.io secret wiring + "update the secret; no workflow change"
  rotation :266-288).
- Vendor facts (signing custody + low-volume cost), retrieved 2026-06-06:
  - Cloudsmith OSS tier 50 GB storage / 200 GB bandwidth (attribution
    required); BYO GPG key supported but the private key/passphrase is
    held in Cloudsmith's infra —
    `help.cloudsmith.io/docs/open-source-hosting-policy`,
    `docs.cloudsmith.com/supply-chain-security/signing-keys`.
  - packagecloud free 2 GB / 10 GB, Starter $89/mo; platform-custodied
    per-repo signing key — `packagecloud.io/pricing`.
- Issues: #234 (apt impl, blocked by this + #547/#346), #346 (substrate
  spike), #547 (substrate impl), #347 (arm64 build leg / glibc-floor
  neighbor).
