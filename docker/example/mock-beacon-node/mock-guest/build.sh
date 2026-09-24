#!/usr/bin/env bash

# Compiles the mock guest for every zkVM with the Ere compiler, and writes its verifying key with the
# keygen command of the Ere server, as the compile workflow of ere-guests does.

# Requires `docker`

set -Eeuo pipefail

MOCK_GUEST_DIR="$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
ERE_VERSION=$(sed -nE 's|.*eth-act/ere", tag = "v([0-9.]+)".*|\1|p' "$MOCK_GUEST_DIR/Cargo.toml" | head -n 1)

for ZKVM in openvm sp1 zisk; do
    ERE_RUSTFLAGS=""
    if [[ $ZKVM == zisk ]]; then
        ERE_RUSTFLAGS="-C target-feature=+unaligned-scalar-mem"
    fi

    docker run --rm \
        -e ERE_RUSTFLAGS="$ERE_RUSTFLAGS" \
        -v "$MOCK_GUEST_DIR:/mock-guest" \
        --tmpfs /mock-guest/target:exec \
        "ghcr.io/eth-act/ere/ere-compiler-$ZKVM:$ERE_VERSION" \
        --compiler-kind rust-customized \
        --guest-dir /mock-guest \
        --output-dir /mock-guest \
        --elf-name "$ZKVM.elf" \
        -- \
        --features "$ZKVM" \
        --ignore-rust-version

    docker run --rm \
        -v "$MOCK_GUEST_DIR:/mock-guest" \
        "ghcr.io/eth-act/ere/ere-server-$ZKVM:$ERE_VERSION" \
        --elf-path "/mock-guest/$ZKVM.elf" \
        keygen \
        --program-vk-path "/mock-guest/$ZKVM.vk"
done
