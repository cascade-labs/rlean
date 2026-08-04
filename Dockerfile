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

# Verglas is only the node-local S3 cache/data path. The SDK discovers the real
# Iceberg catalog from its admin endpoint and performs catalog/query work itself.
ENV VERGLAS_ENDPOINT=http://host.docker.internal:8334
ENV VERGLAS_S3_ENDPOINT=http://host.docker.internal:8333
ENTRYPOINT ["/usr/local/bin/rlean"]
