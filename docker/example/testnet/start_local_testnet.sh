#!/usr/bin/env bash

# Ported and modified from https://github.com/eth-act/lighthouse/blob/optional-proofs/scripts/local_testnet/start_local_testnet.sh

# Requires `docker`, `kurtosis`, `yq`

set -Eeuo pipefail

SCRIPT_DIR="$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
ENCLAVE_NAME=local-testnet
NETWORK_PARAMS_FILE=$SCRIPT_DIR/network_params.yaml
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
        echo "   -b: whether to build Lighthouse docker image    default: $BUILD_IMAGE"
        echo "   -n: kurtosis network params file path           default: $NETWORK_PARAMS_FILE"
        echo "   -k: keeping enclave to allow starting the testnet without destroying the existing one"
        echo "   -h: this help"
        exit
        ;;
  esac
done

LH_BRANCH=optional-proofs-gloas
LH_IMAGE_NAME=$(yq eval ".participants[1].cl_image" $NETWORK_PARAMS_FILE)

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

if [ "$BUILD_IMAGE" = true ]; then
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
  kurtosis enclave rm -f $ENCLAVE_NAME 2>/dev/null || true
fi

kurtosis run --enclave $ENCLAVE_NAME github.com/ethpandaops/ethereum-package@$ETHEREUM_PKG_VERSION --args-file $NETWORK_PARAMS_FILE

# Only the second participant, through zkboost, feeds the fourth EL from now on.
kurtosis service stop $ENCLAVE_NAME cl-4-lighthouse-geth

echo "Started!"
