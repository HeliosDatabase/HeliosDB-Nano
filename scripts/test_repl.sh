#!/bin/bash
# Test script for HeliosDB Lite REPL

echo "Testing HeliosDB Lite REPL..."
echo

# Build the binary
echo "Building heliosdb-nano..."
cargo build --release --bin heliosdb-nano 2>&1 | tail -5

if [ ! -f "target/release/heliosdb-nano" ]; then
    echo "ERROR: Build failed"
    exit 1
fi

echo
echo "Binary built successfully!"
echo

# Test help command
echo "Running: heliosdb-nano --help"
./target/release/heliosdb-nano --help
echo

# Test REPL startup (interactive mode - will need manual testing)
echo "To test REPL interactively, run:"
echo "  ./target/release/heliosdb-nano repl --memory"
echo
echo "Or with a data directory:"
echo "  ./target/release/heliosdb-nano repl -d ./test-db"
echo

echo "Test script complete!"
