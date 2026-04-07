FROM rust:1.91.0-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    libclang-dev \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY . .

RUN --mount=type=cache,sharing=locked,target=/app/target/ \
    --mount=type=cache,sharing=locked,target=/usr/local/cargo/git/db \
    --mount=type=cache,sharing=locked,target=/usr/local/cargo/registry/ \
    cargo build --release --features otel --bin zkboost --bin mock-zkattestor && \
    cp /app/target/release/zkboost /app/zkboost && \
    cp /app/target/release/mock-zkattestor /app/mock-zkattestor

FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 \
    ca-certificates \
    curl \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/zkboost /usr/local/bin/zkboost
COPY --from=builder /app/mock-zkattestor /usr/local/bin/mock-zkattestor

ENTRYPOINT ["/usr/local/bin/zkboost"]
