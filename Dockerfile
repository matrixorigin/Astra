ARG RUST_VERSION=1.94-bookworm
ARG CARGO_CHEF_VERSION=0.1.77
ARG CARGO_REGISTRY=sparse+https://mirrors.ustc.edu.cn/crates.io-index/
ARG DEBIAN_MIRROR=https://mirrors.aliyun.com

FROM rust:${RUST_VERSION} AS chef

ARG CARGO_CHEF_VERSION
ARG CARGO_REGISTRY
ARG DEBIAN_MIRROR
ARG http_proxy
ARG https_proxy
ARG no_proxy
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG NO_PROXY

# Promote proxy build args to environment variables so apt, cargo, and git can
# consume them in this base stage and all stages derived from it.
ENV http_proxy=${http_proxy}
ENV https_proxy=${https_proxy}
ENV no_proxy=${no_proxy}
ENV HTTP_PROXY=${HTTP_PROXY}
ENV HTTPS_PROXY=${HTTPS_PROXY}
ENV NO_PROXY=${NO_PROXY}

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

WORKDIR /app/rust
COPY rust/ ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

WORKDIR /app/rust
COPY --from=planner /app/rust/recipe.json recipe.json
# Runtime image intentionally ships the API server plus the single public CLI.
# Test-only mock_mcp_server and the standalone astra-edge daemon are excluded.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo chef cook --release --no-default-features --recipe-path recipe.json \
        -p astra-runtime --bin astra-server \
        -p astra-cli --bin astra

COPY rust/ ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --no-default-features \
        -p astra-runtime --bin astra-server \
        -p astra-cli --bin astra && \
    mkdir -p /out && \
    cp target/release/astra-server target/release/astra /out/

FROM debian:bookworm-slim

ARG IMAGE_VERSION=dev
ARG IMAGE_REVISION=unknown
ARG IMAGE_BRANCH=unknown

LABEL org.opencontainers.image.title="Astra" \
      org.opencontainers.image.description="Astra API server and CLI runtime image" \
      org.opencontainers.image.source="https://github.com/matrixorigin/astra" \
      org.opencontainers.image.version="${IMAGE_VERSION}" \
      org.opencontainers.image.revision="${IMAGE_REVISION}" \
      org.opencontainers.image.ref.name="${IMAGE_BRANCH}"

ARG DEBIAN_MIRROR
ARG http_proxy
ARG https_proxy
ARG no_proxy
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG NO_PROXY

ENV http_proxy=${http_proxy}
ENV https_proxy=${https_proxy}
ENV no_proxy=${no_proxy}
ENV HTTP_PROXY=${HTTP_PROXY}
ENV HTTPS_PROXY=${HTTPS_PROXY}
ENV NO_PROXY=${NO_PROXY}

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
    groupadd -r appgroup; \
    useradd --system --create-home --home-dir /home/appuser --shell /usr/sbin/nologin -g appgroup appuser
COPY --from=builder /out/astra-server /usr/local/bin/astra-server
COPY --from=builder /out/astra /usr/local/bin/astra
# WORKDIR writable for appgroup; K8s runAsUser overrides should add supplementalGroups: [appgroup GID].
# Prefer mounted volumes for real data rather than writing to /app at runtime.
RUN chown root:appgroup /app && chmod 0770 /app
USER appuser

EXPOSE 17001
ENV HOME=/home/appuser
ENV ASTRA_API_HOST=0.0.0.0
ENV ASTRA_API_PORT=17001

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:17001/health >/dev/null || exit 1

STOPSIGNAL SIGTERM
CMD ["astra-server"]
