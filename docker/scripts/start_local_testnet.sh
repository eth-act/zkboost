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
LH_BRANCH=optional-proofs

# Get options
while getopts "b:n:r:kh" flag; do
  case "${flag}" in
    b) BUILD_IMAGE=${OPTARG};;
    n) NETWORK_PARAMS_FILE=${OPTARG};;
    r) LH_BRANCH=${OPTARG};;
    k) KEEP_ENCLAVE=true;;
    h)
        echo "Start a local testnet with kurtosis."
        echo
        echo "usage: $0 <Options>"
        echo
        echo "Options:"
        echo "   -b: whether to build the custom Lighthouse image when used    default: $BUILD_IMAGE"
        echo "   -n: example network params file path                       default: $NETWORK_PARAMS_FILE"
        echo "   -r: eth-act/lighthouse branch to build from                default: $LH_BRANCH"
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

LH_IMAGE_NAME=lighthouse:eth-act-$LH_BRANCH

# The examples name the published ethpandaops image. A participant that names the local
# tag instead gets that branch built from source here.
if [ "$BUILD_IMAGE" = true ] &&
  LH_IMAGE_NAME="$LH_IMAGE_NAME" yq -e '[.participants[].cl_image == strenv(LH_IMAGE_NAME)] | any' "$NETWORK_PARAMS_FILE" > /dev/null; then
  echo "Building Lighthouse docker image ($LH_IMAGE_NAME) from eth-act/lighthouse@$LH_BRANCH."
  LH_SRC=$(mktemp -d)
  git clone --depth 1 --branch $LH_BRANCH https://github.com/eth-act/lighthouse "$LH_SRC"
  docker build --build-arg FEATURES=portable,ere-verifier -t "$LH_IMAGE_NAME" "$LH_SRC"
  rm -rf "$LH_SRC"
else
  echo "Using the Lighthouse images of $NETWORK_PARAMS_FILE."
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
