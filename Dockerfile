ARG RUST_VERSION=1.94-bookworm
ARG CARGO_REGISTRY=sparse+https://mirrors.ustc.edu.cn/crates.io-index/
ARG DEBIAN_MIRROR=mirrors.aliyun.com

FROM rust:${RUST_VERSION} AS builder

ARG CARGO_REGISTRY
ARG DEBIAN_MIRROR

WORKDIR /app

RUN set -eux; \
    if [ -n "${CARGO_REGISTRY}" ]; then \
        mkdir -p "${CARGO_HOME}"; \
        printf '[source.crates-io]\nreplace-with = "mirror"\n[source.mirror]\nregistry = "%s"\n' "${CARGO_REGISTRY}" > "${CARGO_HOME}/config.toml"; \
    fi

RUN set -eux; \
    if [ -n "${DEBIAN_MIRROR}" ]; then \
        sed -i "s|deb.debian.org|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources 2>/dev/null || true; \
        sed -i "s|deb.debian.org|${DEBIAN_MIRROR}|g" /etc/apt/sources.list 2>/dev/null || true; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates curl; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app/rust

# Copy manifests first so registry dependencies can be compiled and cached
# independently from normal source edits.
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/crates/astra-admin/Cargo.toml crates/astra-admin/Cargo.toml
COPY rust/crates/astra-cli/Cargo.toml crates/astra-cli/Cargo.toml
COPY rust/crates/astra-config/Cargo.toml crates/astra-config/Cargo.toml
COPY rust/crates/astra-credentials/Cargo.toml crates/astra-credentials/Cargo.toml
COPY rust/crates/astra-edge/Cargo.toml crates/astra-edge/Cargo.toml
COPY rust/crates/astra-harness/Cargo.toml crates/astra-harness/Cargo.toml
COPY rust/crates/astra-logging/Cargo.toml crates/astra-logging/Cargo.toml
COPY rust/crates/astra-mcp/Cargo.toml crates/astra-mcp/Cargo.toml
COPY rust/crates/astra-messaging/Cargo.toml crates/astra-messaging/Cargo.toml
COPY rust/crates/astra-pipeline/Cargo.toml crates/astra-pipeline/Cargo.toml
COPY rust/crates/astra-plan/Cargo.toml crates/astra-plan/Cargo.toml
COPY rust/crates/astra-prompts/Cargo.toml crates/astra-prompts/Cargo.toml
COPY rust/crates/astra-runtime-env/Cargo.toml crates/astra-runtime-env/Cargo.toml
COPY rust/crates/astra-sandbox/Cargo.toml crates/astra-sandbox/Cargo.toml
COPY rust/crates/astra-server-types/Cargo.toml crates/astra-server-types/Cargo.toml
COPY rust/crates/astra-skills/Cargo.toml crates/astra-skills/Cargo.toml
COPY rust/crates/astra-test-harness/Cargo.toml crates/astra-test-harness/Cargo.toml
COPY rust/crates/astra-text-utils/Cargo.toml crates/astra-text-utils/Cargo.toml
COPY rust/crates/astra-thin-client/Cargo.toml crates/astra-thin-client/Cargo.toml
COPY rust/crates/astra-tools/Cargo.toml crates/astra-tools/Cargo.toml
COPY rust/crates/astra-turn-core/Cargo.toml crates/astra-turn-core/Cargo.toml
COPY rust/crates/astra-turn-types/Cargo.toml crates/astra-turn-types/Cargo.toml
COPY rust/crates/core/Cargo.toml crates/core/Cargo.toml
COPY rust/crates/runtime/Cargo.toml crates/runtime/Cargo.toml
COPY rust/crates/services/Cargo.toml crates/services/Cargo.toml

RUN set -eux; \
    for manifest in crates/*/Cargo.toml; do \
        crate_dir="$(dirname "${manifest}")"; \
        mkdir -p "${crate_dir}/src/bin"; \
        printf 'pub fn __astra_docker_cache_placeholder() {}\n' > "${crate_dir}/src/lib.rs"; \
        printf 'fn main() {}\n' > "${crate_dir}/src/main.rs"; \
    done; \
    printf 'fn main() {}\n' > crates/astra-cli/src/bin/mock_mcp_server.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release -p astra-runtime --bin astra-server || true; \
    cargo build --release -p astra-cli --bin astra || true; \
    cargo build --release -p astra-admin-cli --bin astra-admin || true

COPY rust/ ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release -p astra-runtime --bin astra-server && \
    cargo build --release -p astra-cli --bin astra && \
    cargo build --release -p astra-admin-cli --bin astra-admin

FROM debian:bookworm-slim

ARG DEBIAN_MIRROR

WORKDIR /app
RUN set -eux; \
    if [ -n "${DEBIAN_MIRROR}" ]; then \
        sed -i "s|deb.debian.org|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources 2>/dev/null || true; \
        sed -i "s|deb.debian.org|${DEBIAN_MIRROR}|g" /etc/apt/sources.list 2>/dev/null || true; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl; \
    rm -rf /var/lib/apt/lists/*; \
    groupadd -r appgroup; \
    useradd --system --create-home --home-dir /home/appuser --shell /usr/sbin/nologin -g appgroup appuser
COPY --from=builder /app/rust/target/release/astra-server /usr/local/bin/astra-server
COPY --from=builder /app/rust/target/release/astra /usr/local/bin/astra
COPY --from=builder /app/rust/target/release/astra-admin /usr/local/bin/astra-admin
# WORKDIR writable for appgroup; K8s runAsUser overrides should add supplementalGroups: [appgroup GID].
# Prefer mounted volumes for real data rather than writing to /app at runtime.
RUN chown root:appgroup /app && chmod 1770 /app
USER appuser

EXPOSE 6789
ENV HOME=/home/appuser
ENV ASTRA_API_HOST=0.0.0.0
ENV ASTRA_API_PORT=6789

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:6789/health >/dev/null || exit 1

CMD ["astra-server"]
