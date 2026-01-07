#!/bin/bash

set -euo pipefail


WORKSPACE=$(mktemp -d)
trap "rm -rf $WORKSPACE" EXIT

echo "Cloning ZisK repository..."
git clone --depth 1 --branch ere/v0.15.0 https://github.com/han0110/zisk "$WORKSPACE"

TAG="zisk-distributed:v0.15.0-gpu"

echo "Building ZisK distributed prover image..."
docker build -f "$WORKSPACE/docker/Dockerfile" --build-arg CUDA_ARCH=${CUDA_ARCH:sm_120} -t "$TAG" "$WORKSPACE"

echo "Image $TAG built"
