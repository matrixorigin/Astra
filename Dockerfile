FROM rust:1.88-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cd rust && cargo build --release -p astra-runtime --bin astra-server && cargo build --release -p astra-cli --bin astra && cargo build --release -p astra-admin-cli --bin astra-admin

FROM debian:bookworm-slim

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 appuser
COPY --from=builder /app/rust/target/release/astra-server /usr/local/bin/astra-server
COPY --from=builder /app/rust/target/release/astra /usr/local/bin/astra
COPY --from=builder /app/rust/target/release/astra-admin /usr/local/bin/astra-admin
RUN chown -R appuser:appuser /app
USER appuser

EXPOSE 8000
ENV RUST_API_ADDR=0.0.0.0:8000

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:8000/health >/dev/null || exit 1

CMD ["astra-server"]
