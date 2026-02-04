#!/bin/bash
# Test script for HeliosDB Lite REPL

echo "Testing HeliosDB Lite REPL..."
echo

# Build the binary
echo "Building heliosdb-lite..."
cargo build --release --bin heliosdb-lite 2>&1 | tail -5

if [ ! -f "target/release/heliosdb-lite" ]; then
    echo "ERROR: Build failed"
    exit 1
fi

echo
echo "Binary built successfully!"
echo

# Test help command
echo "Running: heliosdb-lite --help"
./target/release/heliosdb-lite --help
echo

# Test REPL startup (interactive mode - will need manual testing)
echo "To test REPL interactively, run:"
echo "  ./target/release/heliosdb-lite repl --memory"
echo
echo "Or with a data directory:"
echo "  ./target/release/heliosdb-lite repl -d ./test-db"
echo

echo "Test script complete!"
