#!/bin/bash
# Run the cache lock bug reproduction test
#
# This script:
# 1. Builds the test binaries
# 2. Starts the slow origin server
# 3. Starts the Pingora proxy
# 4. Runs the test client
# 5. Cleans up

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Configuration
ORIGIN_PORT=${ORIGIN_PORT:-8000}
PROXY_PORT=${PROXY_PORT:-6148}
ORIGIN_DELAY=${ORIGIN_DELAY:-5}
NUM_READERS=${NUM_READERS:-3}
DISCONNECT_DELAY_MS=${DISCONNECT_DELAY_MS:-500}
THRESHOLD_MS=${THRESHOLD_MS:-2000}

cleanup() {
    echo "Cleaning up..."
    pkill -f "slow-origin" 2>/dev/null || true
    pkill -f "cache-lock-bug-repro.*proxy" 2>/dev/null || true
    exit
}
trap cleanup EXIT INT TERM

echo "=== Pingora Cache Lock Bug Reproduction ==="
echo ""

# Build
echo "Building..."
cargo build --release 2>&1 | grep -E "(Compiling cache-lock|Finished|error)" || true
echo ""

# Start origin server
echo "Starting slow origin server (port $ORIGIN_PORT, delay ${ORIGIN_DELAY}s)..."
RUST_LOG=info ./target/release/slow-origin --port $ORIGIN_PORT --delay $ORIGIN_DELAY &
ORIGIN_PID=$!
sleep 1

# Start proxy server
echo "Starting Pingora proxy (port $PROXY_PORT, origin port $ORIGIN_PORT)..."
RUST_LOG=info ./target/release/proxy --port $PROXY_PORT --origin-port $ORIGIN_PORT &
PROXY_PID=$!
sleep 2

# Run test
echo ""
echo "Running test client..."
echo ""
RUST_LOG=info ./target/release/test-client \
    --proxy-port $PROXY_PORT \
    --num-readers $NUM_READERS \
    --disconnect-delay-ms $DISCONNECT_DELAY_MS \
    --threshold-ms $THRESHOLD_MS \
    --origin-delay $ORIGIN_DELAY

echo ""
echo "Test complete."
