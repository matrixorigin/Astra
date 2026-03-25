FROM rust:1.88-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cd rust && cargo build --release -p mo-agent-runtime --bin mo-agent-server && cargo build --release -p mo-agent-cli --bin mo-agent && cargo build --release -p mo-admin-cli --bin mo-admin

FROM debian:bookworm-slim

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 appuser
COPY --from=builder /app/rust/target/release/mo-agent-server /usr/local/bin/mo-agent-server
COPY --from=builder /app/rust/target/release/mo-agent /usr/local/bin/mo-agent
COPY --from=builder /app/rust/target/release/mo-admin /usr/local/bin/mo-admin
RUN chown -R appuser:appuser /app
USER appuser

EXPOSE 8000
ENV RUST_API_ADDR=0.0.0.0:8000

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:8000/health >/dev/null || exit 1

CMD ["mo-agent-server"]
