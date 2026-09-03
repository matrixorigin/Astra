# Keep this aligned with rust-toolchain.toml. The cargo-chef and final build
# stages must use the same compiler or the cooked dependency artifacts cannot
# be reused. The digest pins the current multi-architecture image index.
ARG RUST_VERSION=1.97.0-bookworm
ARG CARGO_CHEF_VERSION=0.1.77
ARG IMAGE_REVISION=unknown
ARG IMAGE_SOURCE_DIRTY=true
# Optional build accelerators for restricted or regional networks. Public
# builds use the upstream Cargo and Debian sources by default.
ARG CARGO_REGISTRY
ARG DEBIAN_MIRROR

FROM rust:${RUST_VERSION}@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073 AS chef

ARG CARGO_CHEF_VERSION
ARG CARGO_REGISTRY
ARG DEBIAN_MIRROR

# Docker exposes standard proxy build arguments to RUN instructions without an
# ARG declaration. Do not redeclare or promote them to ENV: proxy URLs may
# contain credentials, and declared values can persist in build metadata.

WORKDIR /app

RUN set -eux; \
    if [ -n "${CARGO_REGISTRY}" ]; then \
        mkdir -p "${CARGO_HOME}"; \
        printf '[source.crates-io]\nreplace-with = "mirror"\n[source.mirror]\nregistry = "%s"\n' "${CARGO_REGISTRY}" > "${CARGO_HOME}/config.toml"; \
    fi

RUN set -eux; \
    if [ -n "${DEBIAN_MIRROR}" ]; then \
        mirror="${DEBIAN_MIRROR%/}"; \
        case "${mirror}" in http://*|https://*) ;; *) mirror="https://${mirror}" ;; esac; \
        find /etc/apt -type f \( -name 'sources.list' -o -name '*.sources' \) -print0 \
            | xargs -0 -r sed -i -E "s#https?://(deb.debian.org|security.debian.org)#${mirror}#g"; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates curl; \
    rm -rf /var/lib/apt/lists/*

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo install cargo-chef --version "${CARGO_CHEF_VERSION}" --locked

FROM chef AS planner

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

ARG IMAGE_REVISION
ARG IMAGE_SOURCE_DIRTY

WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
# Runtime image intentionally ships the API server plus the single public CLI.
# Test-only mock_mcp_server and the standalone astra-edge daemon are excluded.
RUN cargo chef cook --release --no-default-features --recipe-path recipe.json \
        -p astra-runtime --bin astra-server \
        -p astra-cli --bin astra

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN ASTRA_BUILD_SOURCE_GIT_SHA="${IMAGE_REVISION}" \
    ASTRA_BUILD_SOURCE_GIT_DIRTY="${IMAGE_SOURCE_DIRTY}" \
    cargo build --release --no-default-features \
        -p astra-runtime --bin astra-server \
        -p astra-cli --bin astra && \
    mkdir -p /out && \
    cp target/release/astra-server target/release/astra /out/

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

ARG IMAGE_VERSION=dev
ARG IMAGE_REVISION=unknown
ARG IMAGE_BRANCH=unknown

LABEL org.opencontainers.image.title="Astra" \
      org.opencontainers.image.description="Astra API server and CLI runtime image" \
      org.opencontainers.image.url="https://github.com/matrixorigin/Astra" \
      org.opencontainers.image.source="https://github.com/matrixorigin/Astra" \
      org.opencontainers.image.documentation="https://github.com/matrixorigin/Astra#readme" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="MatrixOrigin" \
      org.opencontainers.image.version="${IMAGE_VERSION}" \
      org.opencontainers.image.revision="${IMAGE_REVISION}" \
      org.opencontainers.image.ref.name="${IMAGE_BRANCH}"

ARG DEBIAN_MIRROR
# Standard proxy build arguments remain available to apt during RUN without
# being copied into the final image configuration.

WORKDIR /app
RUN set -eux; \
    replace_apt_sources() { \
        from_regex="$1"; \
        to_base="$(printf '%s' "$2" | sed 's/[&]/\\&/g')"; \
        find /etc/apt -type f \( -name 'sources.list' -o -name '*.sources' \) -print0 \
            | xargs -0 -r sed -i -E "s#${from_regex}#${to_base}#g"; \
    }; \
    if [ -n "${DEBIAN_MIRROR}" ]; then \
        mirror="${DEBIAN_MIRROR%/}"; \
        case "${mirror}" in http://*|https://*) ;; *) mirror="https://${mirror}" ;; esac; \
        mirror_host="${mirror#http://}"; \
        mirror_host="${mirror_host#https://}"; \
        mirror_host_regex="$(printf '%s' "${mirror_host}" | sed 's/[.[\*^$()+?{}|]/\\&/g')"; \
        bootstrap_mirror="${mirror}"; \
        case "${bootstrap_mirror}" in https://*) bootstrap_mirror="http://${bootstrap_mirror#https://}" ;; esac; \
        replace_apt_sources 'https?://(deb.debian.org|security.debian.org)' "${bootstrap_mirror}"; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates; \
    if [ -n "${DEBIAN_MIRROR}" ]; then \
        if [ "${mirror}" != "${bootstrap_mirror}" ]; then \
            replace_apt_sources "https?://${mirror_host_regex}" "${mirror}"; \
            apt-get update; \
        fi; \
    fi; \
    apt-get install -y --no-install-recommends curl libssl3; \
    rm -rf /var/lib/apt/lists/*; \
    groupadd --gid 10001 appgroup; \
    useradd --uid 10001 --gid 10001 --create-home --home-dir /home/appuser --shell /usr/sbin/nologin appuser
COPY --from=builder /out/astra-server /usr/local/bin/astra-server
COPY --from=builder /out/astra /usr/local/bin/astra
COPY LICENSE /usr/share/licenses/astra/LICENSE
# WORKDIR is writable by the fixed non-root runtime identity.
# Prefer mounted volumes for real data rather than writing to /app at runtime.
RUN chown root:appgroup /app && \
    chmod 0770 /app && \
    chmod 0444 /usr/share/licenses/astra/LICENSE
USER 10001:10001

EXPOSE 17001
ENV HOME=/home/appuser
ENV ASTRA_API_HOST=0.0.0.0
ENV ASTRA_API_PORT=17001

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:17001/health >/dev/null || exit 1

STOPSIGNAL SIGTERM
CMD ["astra-server"]
