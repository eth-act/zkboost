#!/bin/bash

# Exit on error
set -e

# Configuration
ZKVM="${ZKVM:-sp1}"  # Default to sp1, can be overridden by environment variable
PORT=3001
SERVER_URL="http://localhost:$PORT"
PROGRAM_ID="basic-$ZKVM"
INPUT_FILE="test-fixture/basic/input/valid"  # Binary input file
FIXTURE_URL="https://raw.githubusercontent.com/han0110/zkboost/test-fixtures/basic.tar.gz"
FIXTURE_ARCHIVE="basic.tar.gz"
FIXTURE_DIR="test-fixture/basic"
CONFIG_FILE="test-fixture/basic/basic.toml"
PROOF_FILE="proof_response.json"
VERIFY_FILE="verify_request.json"

# Download and extract test fixtures if not already present
if [ ! -d "$FIXTURE_DIR" ]; then
    echo "Test fixture not found, downloading..."

    # Download if archive doesn't exist
    if [ ! -f "$FIXTURE_ARCHIVE" ]; then
        echo "Downloading $FIXTURE_URL..."
        curl -L -o "$FIXTURE_ARCHIVE" "$FIXTURE_URL"
        echo "Download complete"
    else
        echo "Archive already exists, skipping download"
    fi

    # Extract archive
    echo "Extracting $FIXTURE_ARCHIVE..."
    tar -xzf "$FIXTURE_ARCHIVE"
    echo "Test fixture extracted to $FIXTURE_DIR"
else
    echo "Test fixture already exists at $FIXTURE_DIR"
fi

# Pull image
echo "Pulling image ere-server-$ZKVM..."
docker image pull "ghcr.io/eth-act/ere/ere-server-$ZKVM:0.0.15-e602c18"

# Build the server
echo "========================================"
echo "Building zkboost server..."
cargo build --release --package zkboost-server
echo "Build complete"

# Configure zkVM in config file
echo "========================================"
echo "Configuring zkVM: $ZKVM"
if [ -f "$CONFIG_FILE" ]; then
    # Replace zkVM kind and program paths
    sed -i "s/kind = \"[^\"]*\"/kind = \"$ZKVM\"/" "$CONFIG_FILE"
    sed -i "s|program-id = \"basic-[^\"]*\"|program-id = \"basic-$ZKVM\"|" "$CONFIG_FILE"
    sed -i "s|program-path = \"./test-fixture/basic/elf/[^\"]*\"|program-path = \"./test-fixture/basic/elf/$ZKVM\"|" "$CONFIG_FILE"

    echo "Config updated for $ZKVM"
else
    echo "ERROR: Config file not found at $CONFIG_FILE"
    exit 1
fi

# Start the server in background
echo "========================================"
echo "Starting zkboost server..."
ERE_IMAGE_REGISTRY=ghcr.io/eth-act/ere RUST_LOG=info ./target/release/zkboost-server --config "$CONFIG_FILE" --port $PORT > zkboost.log 2>&1 &
SERVER_PID=$!
echo "Server started with PID: $SERVER_PID"

# Wait for server to be ready (port listening)
echo "Waiting for server to start on port $PORT..."
TIMEOUT=300
ELAPSED=0
until nc -z localhost $PORT 2>/dev/null; do
    if [ $ELAPSED -ge $TIMEOUT ]; then
        echo "ERROR: Server failed to start within ${TIMEOUT}s"
        kill $SERVER_PID 2>/dev/null || true
        exit 1
    fi
    sleep 1
    ELAPSED=$((ELAPSED + 1))
done
echo "Server is ready"

# Cleanup function to stop server on exit
cleanup() {
    echo ""
    echo "Stopping server (PID: $SERVER_PID)..."

    # Send SIGTERM for graceful shutdown
    kill $SERVER_PID 2>/dev/null || true

    # Wait for server to exit gracefully (up to 10 seconds)
    echo "Waiting for server to shutdown gracefully..."
    for i in {1..10}; do
        if ! kill -0 $SERVER_PID 2>/dev/null; then
            echo "Server stopped gracefully"
            break
        fi
        sleep 1
    done

    # Force kill if still running
    if kill -0 $SERVER_PID 2>/dev/null; then
        echo "Server still running, forcing shutdown..."
        kill -9 $SERVER_PID 2>/dev/null || true
    fi

    # Wait a bit more for Docker cleanup
    echo "Waiting for Docker containers to cleanup..."
    sleep 2
}
trap cleanup EXIT


# Helper function to make API calls with error handling
make_request() {
    local method=$1
    local endpoint=$2
    local data=$3
    local description=$4
    local data_file=$5

    echo "----------------------------------------"
    echo "$description..."
    
    if [ -n "$data_file" ]; then
        response=$(curl -s -X "$method" "$SERVER_URL/$endpoint" \
            -H "Content-Type: application/json" \
            -d "@$data_file")
    elif [ -n "$data" ]; then
        response=$(curl -s -X "$method" "$SERVER_URL/$endpoint" \
            -H "Content-Type: application/json" \
            -d "$data")
    else
        response=$(curl -s -X "$method" "$SERVER_URL/$endpoint")
    fi

    if [ $? -ne 0 ]; then
        echo "ERROR: Failed to make request to $endpoint"
        exit 1
    fi

    # Validate JSON response
    if ! echo "$response" | jq '.' > /dev/null 2>&1; then
        echo "ERROR: Invalid JSON response from $endpoint $response"
        exit 1
    fi

    # Handle endpoint-specific response processing
    case "$endpoint" in
        "prove")
            # Save the full response for later use
            echo "$response" > "$PROOF_FILE"
            # Get proof size (base64 string length to bytes) and proving time
            proof_base64=$(jq -r '.proof' "$PROOF_FILE")
            proof_size=$(echo "$proof_base64" | base64 -d | wc -c)
            echo "Proof size: $proof_size bytes"
            echo "Proving time: $(jq '.proving_time_milliseconds' "$PROOF_FILE")ms"
            echo "Proof generated successfully (full proof saved to $PROOF_FILE)"
            # Print first 32 characters of base64 proof
            echo "First 32 chars of proof (base64): $(echo "$proof_base64" | cut -c1-32)..."
            ;;
        "execute")
            # Display execution metrics
            # Duration is serialized as {secs: N, nanos: N}
            exec_secs=$(jq '.execution_duration.secs' <<< "$response")
            exec_nanos=$(jq '.execution_duration.nanos' <<< "$response")
            exec_ms=$((exec_secs * 1000 + exec_nanos / 1000000))
            echo "Execution time: ${exec_ms}ms"
            echo "Total cycles: $(jq '.total_num_cycles' <<< "$response")"
            ;;
        "verify")
            # Check verification result
            if [ "$(jq '.verified' <<< "$response")" = "true" ]; then
                echo "Verification successful"
            else
                echo "Verification failed: $(jq -r '.failure_reason' <<< "$response")"
                exit 1
            fi
            ;;
        *)
            # For other endpoints, print the response
            echo "Response:"
            echo "$response"
            ;;
    esac
}

echo "========================================"
echo "Starting workflow test..."
echo "========================================"

# Check if input file exists
if [ ! -f "$INPUT_FILE" ]; then
    echo "ERROR: Input file not found at $INPUT_FILE"
    exit 1
fi

# Encode input file to base64
INPUT_BASE64=$(base64 -w 0 "$INPUT_FILE")
echo "Input file encoded (size: $(wc -c < "$INPUT_FILE") bytes)"

# Step 1: Get server info
make_request "GET" "info" "" "Getting server information"

# Step 2: Execute the program with base64-encoded input
EXECUTE_DATA="{
    \"program_id\": \"$PROGRAM_ID\",
    \"input\": \"$INPUT_BASE64\"
}"
make_request "POST" "execute" "$EXECUTE_DATA" "Executing program"

# Step 3: Generate proof with base64-encoded input
PROVE_DATA="{
    \"program_id\": \"$PROGRAM_ID\",
    \"input\": \"$INPUT_BASE64\"
}"
make_request "POST" "prove" "$PROVE_DATA" "Generating proof"

# Step 4: Verify proof
# Create a temporary file for the verification request
if [ -f "$PROOF_FILE" ]; then
    # Create verification request file
    jq -c --arg program_id "$PROGRAM_ID" '{program_id: $program_id, proof: .proof}' "$PROOF_FILE" > "$VERIFY_FILE"
    make_request "POST" "verify" "" "Verifying proof" "$VERIFY_FILE"
    # Clean up temporary files
    rm "$VERIFY_FILE"
    rm "$PROOF_FILE"
else
    echo "Error: $PROOF_FILE not found"
    exit 1
fi

echo "========================================"
echo "Workflow test completed successfully!" 