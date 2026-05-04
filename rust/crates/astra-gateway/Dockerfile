# Astra Gateway — multi-stage Docker build
FROM rust:1.85-slim AS builder

WORKDIR /build
COPY rust/ rust/

# Build only the gateway binary (release)
RUN cd rust && cargo build -p astra-gateway --release \
    && strip target/release/astra-gateway

# ── Runtime image ──
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && groupadd -r gateway && useradd -r -g gateway -d /app gateway

COPY --from=builder /build/rust/target/release/astra-gateway /usr/local/bin/astra-gateway
COPY --from=builder /build/rust/target/release/astra /usr/local/bin/astra

WORKDIR /app
COPY rust/crates/astra-gateway/gateway.example.yaml /app/gateway.yaml
RUN chown -R gateway:gateway /app

USER gateway
EXPOSE 8088
ENV RUST_LOG=info,astra_gateway=debug

ENTRYPOINT ["astra-gateway"]
CMD ["--config", "/app/gateway.yaml"]
