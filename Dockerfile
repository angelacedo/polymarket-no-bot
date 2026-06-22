# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/polymarket-no-bot /usr/local/bin/polymarket-no-bot
COPY config/docker.toml /app/config/docker.toml

RUN mkdir -p /app/data

ENV RUST_LOG=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/status || exit 1

CMD ["polymarket-no-bot", "run", "--config", "/app/config/docker.toml", "--mode", "paper"]
