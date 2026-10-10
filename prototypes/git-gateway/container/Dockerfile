# SPDX-License-Identifier: Apache-2.0
# Build context: repository root. No local binary, configuration or fixture is copied.
FROM rust:1.98.0-trixie AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
COPY docs/ docs/
RUN cargo build --locked --release -p heddle-git-projection --example gateway_native

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates git python3 libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && test -x "$(git --exec-path)/git-http-backend" \
    && mkdir -p /app/gateway /config \
    && chmod 0555 /app /app/gateway /config
COPY --chmod=0444 LICENSE /app/LICENSE
COPY --from=build --chmod=0555 /build/target/release/examples/gateway_native /usr/local/bin/gateway_native
COPY --chmod=0444 prototypes/git-gateway/gateway/__init__.py \
    prototypes/git-gateway/gateway/artifacts.py \
    prototypes/git-gateway/gateway/auth.py \
    prototypes/git-gateway/gateway/bundle_transport.py \
    prototypes/git-gateway/gateway/child.py \
    prototypes/git-gateway/gateway/core.py \
    prototypes/git-gateway/gateway/host.py \
    prototypes/git-gateway/gateway/http.py \
    prototypes/git-gateway/gateway/native_bundle.py /app/gateway/
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PYTHONNOUSERSITE=1 \
    HOME=/nonexistent \
    TMPDIR=/tmp \
    GATEWAY_NATIVE=/usr/local/bin/gateway_native
WORKDIR /app
USER 65532:65532
# Intentionally no EXPOSE, credentials, policy defaults, fixture, or public listener.
ENTRYPOINT ["python3", "-m", "gateway.host", "/config/host.json"]
