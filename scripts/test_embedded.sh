#!/bin/bash
# Comprehensive Embedded Mode Test Script

set -e

echo "========================================="
echo "HeliosDB Lite - Embedded Mode Test Suite"
echo "========================================="
echo ""

BINARY="./target/release/heliosdb-lite"

# Build if not exists
if [ ! -f "$BINARY" ]; then
    echo "Building release binary..."
    cargo build --release
fi

echo "Test 1: In-Memory Database - Basic SQL"
printf '%s\n' \
    'CREATE TABLE users (id INT, name TEXT, email TEXT);' \
    'INSERT INTO users VALUES (1, '"'"'Alice'"'"', '"'"'alice@example.com'"'"');' \
    'INSERT INTO users VALUES (2, '"'"'Bob'"'"', '"'"'bob@example.com'"'"');' \
    'SELECT * FROM users;' \
    '\q' | $BINARY repl 2>&1 | tail -20

echo ""
echo "Test 2: File-Based Database - Persistence"
rm -f /tmp/test_helios.db /tmp/test_helios.db.wal
printf '%s\n' \
    'CREATE TABLE products (id INT, name TEXT, price REAL);' \
    'INSERT INTO products VALUES (1, '"'"'Widget'"'"', 9.99);' \
    'SELECT * FROM products;' \
    '\q' | $BINARY repl /tmp/test_helios.db 2>&1 | tail -15

echo ""
echo "Test 3: Reopen Database - Check Persistence"
printf '%s\n' \
    'SELECT * FROM products;' \
    '\q' | $BINARY repl /tmp/test_helios.db 2>&1 | tail -10

echo ""
echo "Test 4: Branching"
printf '%s\n' \
    'CREATE TABLE test (id INT);' \
    'CREATE BRANCH feature1;' \
    '\branches' \
    '\q' | $BINARY repl 2>&1 | tail -20

echo ""
echo "Test 5: Time Travel"
printf '%s\n' \
    'CREATE TABLE orders (id INT, amount REAL);' \
    'INSERT INTO orders VALUES (1, 100.0);' \
    "SELECT * FROM orders AS OF TIMESTAMP '2025-01-01';" \
    '\q' | $BINARY repl 2>&1 | tail -15

echo ""
echo "Test 6: Meta Commands"
printf '%s\n' \
    'CREATE TABLE test1 (id INT);' \
    'CREATE TABLE test2 (name TEXT);' \
    '\d' \
    '\dt' \
    '\q' | $BINARY repl 2>&1 | tail -20

echo ""
echo "Test 7: Telemetry"
printf '%s\n' \
    '\telemetry' \
    '\q' | $BINARY repl 2>&1 | tail -15

echo ""
echo "Test 8: Profile Command"
printf '%s\n' \
    'CREATE TABLE perf_test (id INT, value TEXT);' \
    '\profile SELECT * FROM perf_test' \
    '\q' | $BINARY repl 2>&1 | tail -15

echo ""
echo "========================================="
echo "Embedded Mode Tests Complete"
echo "========================================="
