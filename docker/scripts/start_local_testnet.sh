#!/usr/bin/env bash

# Ported and modified from https://github.com/eth-act/lighthouse/blob/optional-proofs/scripts/local_testnet/start_local_testnet.sh

# Requires `docker`, `kurtosis`, `yq`

set -Eeuo pipefail

SCRIPT_DIR="$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
ENCLAVE_NAME=${ENCLAVE_NAME:-local-testnet}
NETWORK_PARAMS_FILE=$SCRIPT_DIR/../example/testnet/network_params.yaml
ETHEREUM_PKG_VERSION=main

BUILD_IMAGE=true
KEEP_ENCLAVE=false

# Get options
while getopts "b:n:kh" flag; do
  case "${flag}" in
    b) BUILD_IMAGE=${OPTARG};;
    n) NETWORK_PARAMS_FILE=${OPTARG};;
    k) KEEP_ENCLAVE=true;;
    h)
        echo "Start a local testnet with kurtosis."
        echo
        echo "usage: $0 <Options>"
        echo
        echo "Options:"
        echo "   -b: whether to build the custom Lighthouse image when used    default: $BUILD_IMAGE"
        echo "   -n: example network params file path                       default: $NETWORK_PARAMS_FILE"
        echo "   -k: keeping enclave to allow starting the testnet without destroying the existing one"
        echo "   -h: this help"
        exit
        ;;
  esac
done

if ! command -v docker &> /dev/null; then
    echo "Docker is not installed. Please install Docker and try again."
    exit 1
fi

if ! command -v kurtosis &> /dev/null; then
    echo "kurtosis command not found. Please install kurtosis and try again."
    exit 1
fi

if ! command -v yq &> /dev/null; then
    echo "yq not found. Please install yq and try again."
    exit 1
fi

LH_BRANCH=optional-proofs-gloas
LH_IMAGE_NAME=$(yq eval '.participants[] | select(.cl_image == "lighthouse:eth-act-optional-proofs-gloas") | .cl_image' "$NETWORK_PARAMS_FILE")

if [ "$BUILD_IMAGE" = true ] && [ -n "$LH_IMAGE_NAME" ]; then
  # eth-act/lighthouse publishes no image of this branch.
  echo "Building Lighthouse docker image ($LH_IMAGE_NAME) from eth-act/lighthouse@$LH_BRANCH."
  LH_SRC=$(mktemp -d)
  git clone --depth 1 --branch $LH_BRANCH https://github.com/eth-act/lighthouse "$LH_SRC"
  # The prebuilt ERE verifier library carries its own Rust standard library.
  sed -i 's|^ENV CARGO_INCREMENTAL=1$|&\nENV RUSTFLAGS="-C link-arg=-Wl,--allow-multiple-definition"|' "$LH_SRC/Dockerfile"
  docker build --build-arg FEATURES=portable,ere-verifier -t "$LH_IMAGE_NAME" "$LH_SRC"
  rm -rf "$LH_SRC"
fi

if [ "$KEEP_ENCLAVE" = false ]; then
  # Stop local testnet
  kurtosis enclave rm -f "$ENCLAVE_NAME" 2>/dev/null || true
fi

kurtosis run --enclave "$ENCLAVE_NAME" "github.com/ethpandaops/ethereum-package@$ETHEREUM_PKG_VERSION" --args-file "$NETWORK_PARAMS_FILE"

# Initialize the dedicated Compose geth with the genesis of this enclave.
GENESIS_TMP_DIR=$(mktemp -d)
trap 'rm -rf "$GENESIS_TMP_DIR"' EXIT
kurtosis files download "$ENCLAVE_NAME" el_cl_genesis_data "$GENESIS_TMP_DIR/genesis"
cp "$GENESIS_TMP_DIR/genesis/genesis.json" "$SCRIPT_DIR/genesis-$ENCLAVE_NAME.json"

echo "Started!"
