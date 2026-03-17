#!/usr/bin/env bash
set -euo pipefail

# Download and verify a guest program from eth-act/ere-guests releases.
#
# Usage: ./download-ere-guest.sh --tag <release-tag> --guest <guest-name> --output-dir <dir>
# Example: ./download-ere-guest.sh --tag v0.6.0 --guest stateless-validator-reth-zisk --output-dir ./programs/

PUB_KEY="RWTsNA0kZFhw19A26aujYun4hv4RraCnEYDehrgEG6NnCjmjkr9/+KGy"

usage() {
    echo "Usage: $0 --tag <release-tag> --guest <guest-name> --output-dir <dir>"
    echo ""
    echo "Options:"
    echo "  --tag         Release tag (e.g. v0.6.0)"
    echo "  --guest       Guest program name (e.g. stateless-validator-reth-zisk)"
    echo "  --output-dir  Directory to save the downloaded program"
    exit 1
}

RELEASE_TAG=""
GUEST_NAME=""
OUTPUT_DIR=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)
            RELEASE_TAG="$2"
            shift 2
            ;;
        --guest)
            GUEST_NAME="$2"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        --help|-h)
            usage
            ;;
        *)
            echo "Error: unknown option '$1'"
            usage
            ;;
    esac
done

if [[ -z "${RELEASE_TAG}" || -z "${GUEST_NAME}" || -z "${OUTPUT_DIR}" ]]; then
    echo "Error: all flags are required."
    usage
fi

REPO="eth-act/ere-guests"
BASE_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}"
PROGRAM_URL="${BASE_URL}/${GUEST_NAME}"
SIG_URL="${BASE_URL}/${GUEST_NAME}.minisig"

mkdir -p "${OUTPUT_DIR}"

PROGRAM_PATH="${OUTPUT_DIR}/${GUEST_NAME}"
SIG_PATH="${OUTPUT_DIR}/${GUEST_NAME}.minisig"

echo "Downloading ${GUEST_NAME} from ${PROGRAM_URL}..."
curl -fSL -o "${PROGRAM_PATH}" "${PROGRAM_URL}"

echo "Downloading signature from ${SIG_URL}..."
curl -fSL -o "${SIG_PATH}" "${SIG_URL}"

echo "Verifying signature..."
if ! command -v minisign &> /dev/null; then
    echo "Error: minisign is not installed."
    echo "Install: https://jedisct1.github.io/minisign/"
    exit 1
fi

minisign -Vm "${PROGRAM_PATH}" -x "${SIG_PATH}" -P "${PUB_KEY}"

echo "Verified OK: ${PROGRAM_PATH}"

# Clean up signature file
rm -f "${SIG_PATH}"
