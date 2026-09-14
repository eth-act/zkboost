FROM rust:1.94.1-bookworm AS builder

WORKDIR /app

# nasm and the native libraries build `proofman-starks-lib-c`, pulled by `proofman-fields` on x86_64
RUN apt-get update && apt-get install -y --no-install-recommends \
    libclang-dev \
    protobuf-compiler \
    libprotobuf-dev \
    nasm \
    nlohmann-json3-dev \
    libgmp-dev \
    libsodium-dev \
    libomp-dev \
    libopenmpi-dev \
    libssl-dev \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY . .

RUN --mount=type=cache,sharing=locked,target=/app/target/ \
    --mount=type=cache,sharing=locked,target=/usr/local/cargo/git/db \
    --mount=type=cache,sharing=locked,target=/usr/local/cargo/registry/ \
    cargo build --release --features otel --bin zkboost --bin mock-cl && \
    cp /app/target/release/zkboost /app/zkboost && \
    cp /app/target/release/mock-cl /app/mock-cl

FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 \
    ca-certificates \
    curl \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/zkboost /usr/local/bin/zkboost
COPY --from=builder /app/mock-cl /usr/local/bin/mock-cl

ENTRYPOINT ["/usr/local/bin/zkboost"]
