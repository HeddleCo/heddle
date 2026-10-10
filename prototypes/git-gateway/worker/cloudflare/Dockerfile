# SPDX-License-Identifier: Apache-2.0
# Python-free Cloudflare native host. Build context: repository root.
# No policy, secret, native source, local executable or catalog is baked in.
FROM rust:1.98.0-trixie AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
COPY docs/ docs/
RUN cargo build --locked --release -p heddle-git-projection --example gateway_host

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates git libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && test -x "$(git --exec-path)/git-http-backend" \
    && mkdir -p /app /config \
    && chmod 0555 /app /config
COPY --chmod=0444 LICENSE /app/LICENSE
COPY --from=build --chmod=0555 /build/target/release/examples/gateway_host /usr/local/bin/gateway_host
ENV HOME=/nonexistent TMPDIR=/tmp
WORKDIR /app
USER 65532:65532
# One Rust ingress; no Python proxy or second loopback HTTP hop.
# Current disclosure authority is required on every request, including cache hits.
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/gateway_host"]
