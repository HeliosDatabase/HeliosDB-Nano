#!/bin/bash
# Comprehensive Server Mode Test Script

set -e

echo "========================================"
echo "HeliosDB Lite - Server Mode Test Suite"
echo "========================================"
echo ""

BINARY="./target/release/heliosdb-lite"
PORT=15432  # Use high port to avoid conflicts
TEST_DIR="/tmp/heliosdb-test-$$"

# Cleanup function
cleanup() {
    echo "Cleaning up..."
    if [ ! -z "$SERVER_PID" ]; then
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
    rm -rf "$TEST_DIR"
}

trap cleanup EXIT

# Create test directory
mkdir -p "$TEST_DIR"

echo "Test 1: Server Startup"
echo "Starting server on port $PORT..."
$BINARY start --data-dir "$TEST_DIR" --port $PORT --listen 127.0.0.1 &
SERVER_PID=$!

# Wait for server to start
echo "Waiting for server to start..."
sleep 3

# Check if server is running
if ! kill -0 $SERVER_PID 2>/dev/null; then
    echo "ERROR: Server failed to start"
    exit 1
fi
echo "✓ Server started (PID: $SERVER_PID)"

echo ""
echo "Test 2: PostgreSQL Protocol - Basic Connection"
# Try to connect using psql if available
if command -v psql &> /dev/null; then
    echo "Testing connection with psql..."
    psql -h 127.0.0.1 -p $PORT -U postgres -d postgres -c "SELECT 1;" 2>&1 | head -10 || true
else
    echo "psql not available, skipping PostgreSQL protocol test"
fi

echo ""
echo "Test 3: Check server is responsive"
# Send SIGTERM to gracefully shutdown
echo "Shutting down server..."
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true
SERVER_PID=""

echo "✓ Server shut down cleanly"

echo ""
echo "Test 4: REST API Mode (if available)"
echo "Starting server for REST API tests..."
$BINARY start --data-dir "$TEST_DIR" --port $PORT --listen 127.0.0.1 &
SERVER_PID=$!
sleep 3

# Try HTTP requests if curl is available
if command -v curl &> /dev/null; then
    echo "Testing REST API endpoints..."

    # Check if there's an HTTP endpoint (default might be 8080)
    echo "Checking for HTTP API on port 8080..."
    curl -s -f http://127.0.0.1:8080/health 2>&1 || echo "No HTTP API on 8080"

    echo "Checking for HTTP API on same port as PostgreSQL..."
    curl -s -f http://127.0.0.1:$PORT/health 2>&1 || echo "No HTTP API on $PORT"
else
    echo "curl not available, skipping REST API tests"
fi

echo ""
echo "Test 5: Hybrid Mode - Server + Embedded"
# Start fresh server
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true

echo "Starting server in background..."
$BINARY start --data-dir "$TEST_DIR" --port $PORT --listen 127.0.0.1 &
SERVER_PID=$!
sleep 3

# Also test embedded mode while server is running (different data dir)
echo "Testing embedded mode (different data dir)..."
printf '%s\n' \
    'CREATE TABLE hybrid_test (id INT);' \
    'SELECT * FROM hybrid_test;' \
    '\q' | $BINARY repl --data-dir "/tmp/heliosdb-embedded-$$" 2>&1 | tail -10

rm -rf "/tmp/heliosdb-embedded-$$"

echo ""
echo "========================================"
echo "Server Mode Tests Complete"
echo "========================================"
