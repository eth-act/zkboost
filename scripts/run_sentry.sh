#!/usr/bin/env bash

# Start local testnet and run execution-witness-sentry
# Requires: docker, kurtosis, yq, cargo

set -Eeuo pipefail

SCRIPT_DIR="$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
ROOT_DIR="$SCRIPT_DIR/.."
ENCLAVE_NAME=local-testnet
NETWORK_PARAMS_FILE=$SCRIPT_DIR/network_params_simple.yaml
CONFIG_FILE=$ROOT_DIR/crates/execution-witness-sentry/config.toml

cleanup() {
    echo "Cleaning up..."
    kurtosis enclave rm -f $ENCLAVE_NAME 2>/dev/null || true
}

trap cleanup EXIT

# Check dependencies
for cmd in docker kurtosis yq cargo; do
    if ! command -v $cmd &> /dev/null; then
        echo "Error: $cmd is not installed"
        exit 1
    fi
done

# Remove existing enclave if present
kurtosis enclave rm -f $ENCLAVE_NAME 2>/dev/null || true

echo "Starting local testnet..."
kurtosis run --enclave $ENCLAVE_NAME github.com/ethpandaops/ethereum-package@main --args-file $NETWORK_PARAMS_FILE

echo ""
echo "Testnet started. Building and running execution-witness-sentry..."
echo ""

# Build the sentry
cargo build -p execution-witness-sentry --manifest-path "$ROOT_DIR/Cargo.toml"

# Run the sentry
cd "$ROOT_DIR/crates/execution-witness-sentry"
RUST_LOG=info cargo run --manifest-path "$ROOT_DIR/Cargo.toml" -p execution-witness-sentry -- "$CONFIG_FILE"
