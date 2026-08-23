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
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libpython3.11 \
        python3 \
        python3-numpy \
        python3-pandas \
        python3-scipy \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /tmp/rlean /usr/local/bin/rlean
WORKDIR /strategy

# Verglas is optional. Baking an endpoint in would make every container read as
# "configured", so a node without a gateway would fail the preflight instead of
# running uncached. The host injects VERGLAS_* only when one is configured.
ENTRYPOINT ["/usr/local/bin/rlean"]
