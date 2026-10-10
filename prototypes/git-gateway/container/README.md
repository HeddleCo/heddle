# Gateway container preparation

Code-only packaging for the opt-in `gateway.host` entrypoint. **The image has not
been built or run.** Docker, Podman, Buildah and nerdctl were absent from the cloud
executor. Static and engine-command tests are not container or hosted proof.
No service, image registry, account, credential, fixture identity or remote resource
was created. The scripts never deploy, push, create networks, or provision tokens.

## Image

The multi-stage Dockerfile builds the locked `gateway_native` example with official
`rust:1.98.0-trixie`, then copies only that binary, the runtime Python modules and
LICENSE into `debian:trixie-slim`. Debian supplies Python, Git (including
`git-http-backend`), certificates and the C runtime dependencies. It does not reuse
the cloud-built binary. Official tag listings checked 2026-10-02:
[Rust](https://hub.docker.com/_/rust/) and
[Debian](https://hub.docker.com/_/debian).

The root build context is narrowed by `Dockerfile.dockerignore`; the root
`.dockerignore` is unchanged. Public Rust source/test fixtures are build inputs
only. Local native stores, demo directories, Git history, local build outputs,
policies and credentials are excluded. Python's demo module is not in the final
image. Application files are root-owned and not writable by UID/GID 65532. Runtime
configuration is never passed as a build argument or copied into an image layer.

A future operator with separately authorized container execution can build from
the repository root:

```sh
docker build -f prototypes/git-gateway/container/Dockerfile \
  -t heddle-git-gateway:local .
```

This explicit build may fetch official base images, Debian packages and the
public dependencies pinned by Cargo.lock. No build was attempted here. Rust is
version-pinned; base-image digests and Debian package snapshots are not pinned,
so this is not a bit-for-bit reproducible or supply-chain-attested image.

## External configuration and local run

Provide an existing configuration directory containing `host.json`, with paths
in that file referring to the **container's** filesystem. Use
`/usr/local/bin/gateway_native` for `binary`. Reader and service policies, catalog
configuration, published pins, bundles and any existing read credentials must
come from that operator-controlled directory or separately approved external
services. See [the host guide](../HOSTING.md) for the host schema. No working
credential or sample identity is supplied by this package.

The directory must be readable by container UID/GID 65532. Keep sensitive files
accessible only to the operator and that identity; the script does not change
ownership or permissions. Keep all needed files on the same mounted filesystem:
nested submounts are deliberately not included ([Docker bind-mount contract](https://docs.docker.com/engine/storage/bind-mounts/#recursive-mounts)). A local catalog may also need
ownership compatible with Git's normal repository trust checks; do not bypass
those checks globally.

```sh
prototypes/git-gateway/container/run.sh \
  /absolute/operator-config-directory heddle-git-gateway:local
```

The scripts require Docker with Linux container and cgroup support. They verify
that the image exists locally, resolve it to its local immutable ID, and use
`--pull=never`. A missing image or engine fails rather than installing, pulling or
building anything. Run is foreground and ephemeral (`--rm`), with:

- UID/GID 65532, a read-only root and read-only `/config` bind mount
- All capabilities dropped and `no-new-privileges`
- 1 GiB memory, no additional swap, one CPU quota and 64-PID cgroup ceiling
- 128 open files, 64-process user limit, 96 MiB per-file limit and no core dumps
- A 512 MiB `/tmp` tmpfs (`noexec,nosuid,nodev`, mode 0700) and 16 MiB `/dev/shm`
- No published ports, host networking, privileged mode or engine-socket mounts

The runtime's normal private bridge permits configured HTTPS reads. The Python
listener binds only to `127.0.0.1` **inside the container network namespace**.
Host `localhost` cannot reach it. A future approved TLS/service-identity sidecar
could share that namespace and connect over loopback; no sidecar, edge tunnel,
TLS termination or network ingress is configured or tested here. These controls
do not establish production authority, source retention, or hostile-input safety.
Tmpfs pages count against memory limits; peak resource usage is unmeasured.
See [Docker resource constraints](https://docs.docker.com/engine/containers/resource_constraints/)
for host support and memory/swap behavior.

## Validation

Static and script command-contract tests, runnable without Docker:

```sh
python3 -m unittest discover -s prototypes/git-gateway/container -v
```

These tests invoke the actual shell scripts with a clearly fake local engine,
checking limits, immutable image selection, read-only mounts, no published ports,
no automatic build/pull, offline smoke, and rejection of unsafe extra arguments.
They also check shell syntax, exact runtime source allowlist and build contract.

A future **explicit** smoke against an already-built local image:

```sh
prototypes/git-gateway/container/smoke.sh heddle-git-gateway:local
```

Smoke requires Linux cgroup v2. It disables all networking and does not mount
configuration, run the host listener, create fixtures or mint an identity. It
checks module imports, effective UID/GID, application immutability, read-only root,
tmpfs flags, capabilities, no-new-privileges, rlimits, cgroup quota values, the Git
backend and native executable's usage/error path. It does not stress-test quotas,
serve Git, authenticate a reader or verify any hosted service. The full clone,
revocation, service-policy, read-only filesystem and resource-pressure behavior
still needs a separately authorized container runtime test before deployment.

Current result: 10 static/command-contract tests passed. Runtime smoke and image
build are **not run**. Neither local engine-command fixtures nor static packaging
checks are evidence that Docker enforced these controls.
