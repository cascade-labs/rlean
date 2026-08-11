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

# One Verglas gateway advertises the catalog and routes bounded Arrow queries
# and writes to its isolated query/write roles. rlean never receives object
# store credentials and never embeds an Iceberg query engine.
ENV VERGLAS_ENDPOINT=http://host.docker.internal:8334
ENTRYPOINT ["/usr/local/bin/rlean"]
