FROM rust:1.88-slim-bookworm AS builder
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    openssl \
    libssl-dev \
    pkg-config \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY . .

# Build the application for release
RUN cargo build --release

# ---- Runtime Stage ----
FROM debian:bookworm-slim
WORKDIR /app

# Copy the binary and programs
COPY --from=builder /app/target/release/zkboost-server /app/zkboost-server

# Expose port 3000 to the outside world (This is the port that the rust code currently listens on)
EXPOSE 3000
CMD ["/app/zkboost-server"]
