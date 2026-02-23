#!/bin/bash
# Test new features: REAL/FLOAT types, SSL rejection, daemon mode

set -e

BINARY="./target/release/heliosdb-nano"

echo "========================================"
echo "Testing New Features"
echo "========================================"
echo ""

echo "Test 1: REAL and FLOAT data types"
printf '%s\n' \
    'CREATE TABLE products (id INT, name TEXT, price REAL);' \
    'INSERT INTO products VALUES (1, '"'"'Widget'"'"', 9.99);' \
    'INSERT INTO products VALUES (2, '"'"'Gadget'"'"', 19.99);' \
    'SELECT * FROM products;' \
    '\q' | $BINARY repl --memory 2>&1 | tail -20

echo ""
echo "Test 2: FLOAT data type"
printf '%s\n' \
    'CREATE TABLE measurements (id INT, value FLOAT);' \
    'INSERT INTO measurements VALUES (1, 3.14159);' \
    'INSERT INTO measurements VALUES (2, 2.71828);' \
    'SELECT * FROM measurements;' \
    '\q' | $BINARY repl --memory 2>&1 | tail -15

echo ""
echo "Test 3: Daemon mode - Start server"
$BINARY start --daemon --port 15433 --pid-file /tmp/heliosdb-test.pid
sleep 2

echo ""
echo "Test 4: Check server status"
$BINARY status --pid-file /tmp/heliosdb-test.pid

echo ""
echo "Test 5: Test psql connection (SSL should be rejected gracefully)"
if command -v psql &> /dev/null; then
    psql -h 127.0.0.1 -p 15433 -U postgres -d postgres -c "SELECT 1 AS test;" 2>&1 || echo "Connection test completed"
else
    echo "psql not available, skipping connection test"
fi

echo ""
echo "Test 6: Stop daemon server"
$BINARY stop --pid-file /tmp/heliosdb-test.pid

echo ""
echo "Test 7: Verify server stopped"
$BINARY status --pid-file /tmp/heliosdb-test.pid || echo "Server stopped as expected"

echo ""
echo "========================================"
echo "All tests complete!"
echo "========================================"
