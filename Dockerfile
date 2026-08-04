# syntax=docker/dockerfile:1.7
FROM rust:1.92-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config python3-dev clang \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release -p rlean --bin rlean \
    && cp /src/target/release/rlean /tmp/rlean

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates python3 python3-numpy libpython3.11 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /tmp/rlean /usr/local/bin/rlean
WORKDIR /strategy

# Verglas and market-data services are node services. A run container has no
# result volume: every durable backtest result is committed through this API.
ENV VERGLAS_ENDPOINT=http://host.docker.internal:8334
ENTRYPOINT ["/usr/local/bin/rlean"]
