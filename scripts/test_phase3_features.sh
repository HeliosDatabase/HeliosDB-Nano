#!/bin/bash

# HeliosDB-Lite Phase 3 Feature Test Script
# Tests all Phase 3 features that can be tested from REPL
# Run: ./test_phase3_features.sh

set -e

BINARY="./target/release/heliosdb-nano"
DB_FILE="heliosdb_test.db"

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Test counter
PASSED=0
FAILED=0

# Clean up any existing test database
rm -f "$DB_FILE"*

echo "=========================================="
echo "HeliosDB-Lite Phase 3 Feature Test Suite"
echo "=========================================="
echo ""

# Helper function to run a test
run_test() {
    local test_name="$1"
    local sql_commands="$2"
    local expected_pattern="$3"

    echo -n "Testing: $test_name ... "

    # Run REPL with SQL commands
    output=$(timeout 10 "$BINARY" repl << EOF
$sql_commands
\q
EOF
)

    # Check if expected pattern is in output
    if echo "$output" | grep -q "$expected_pattern"; then
        echo -e "${GREEN}✓ PASSED${NC}"
        ((PASSED++))
    else
        echo -e "${RED}✗ FAILED${NC}"
        echo "  Expected pattern: $expected_pattern"
        echo "  Output snippet: $(echo "$output" | head -3)"
        ((FAILED++))
    fi
}

# ===================================================================
# Test Group 1: System Views
# ===================================================================
echo -e "${YELLOW}Test Group 1: System Views${NC}"
echo "-----------------------------------"

run_test "System View: pg_database_branches" \
    "SELECT * FROM pg_database_branches();" \
    "branch_name"

run_test "System View: pg_mv_staleness" \
    "SELECT * FROM pg_mv_staleness();" \
    "view_name"

run_test "System View: pg_vector_index_stats" \
    "SELECT * FROM pg_vector_index_stats();" \
    "index_name"

echo ""

# ===================================================================
# Test Group 2: Basic Time-Travel with AS OF NOW
# ===================================================================
echo -e "${YELLOW}Test Group 2: Time-Travel Queries (AS OF NOW)${NC}"
echo "-----------------------------------"

run_test "AS OF NOW with CREATE and INSERT" \
    "CREATE TABLE products (id INT, name TEXT, price INT);
INSERT INTO products VALUES (1, 'Widget', 100);
INSERT INTO products VALUES (2, 'Gadget', 200);
SELECT * FROM products AS OF NOW;" \
    "Widget"

run_test "AS OF NOW returns same as current" \
    "CREATE TABLE inventory (id INT, stock INT);
INSERT INTO inventory VALUES (1, 50);
SELECT id FROM inventory AS OF NOW;" \
    "Query OK"

echo ""

# ===================================================================
# Test Group 3: Time-Travel with AS OF TIMESTAMP
# ===================================================================
echo -e "${YELLOW}Test Group 3: Time-Travel Queries (AS OF TIMESTAMP)${NC}"
echo "-----------------------------------"

run_test "AS OF TIMESTAMP with recent timestamp" \
    "CREATE TABLE orders (id INT, amount INT);
INSERT INTO orders VALUES (1, 100);
INSERT INTO orders VALUES (2, 200);
SELECT * FROM orders AS OF TIMESTAMP '2025-11-28 09:00:00';" \
    "orders"

run_test "AS OF TIMESTAMP with historical timestamp" \
    "CREATE TABLE sales (id INT, total INT);
INSERT INTO sales VALUES (1, 500);
SELECT * FROM sales AS OF TIMESTAMP '2025-11-01 00:00:00';" \
    "Query OK"

echo ""

# ===================================================================
# Test Group 4: Time-Travel with AS OF TRANSACTION
# ===================================================================
echo -e "${YELLOW}Test Group 4: Time-Travel Queries (AS OF TRANSACTION)${NC}"
echo "-----------------------------------"

run_test "AS OF TRANSACTION ID" \
    "CREATE TABLE accounts (id INT, balance INT);
INSERT INTO accounts VALUES (1, 1000);
INSERT INTO accounts VALUES (2, 2000);
SELECT * FROM accounts AS OF TRANSACTION 1;" \
    "accounts"

run_test "AS OF TRANSACTION with multiple inserts" \
    "CREATE TABLE logs (id INT, message TEXT);
INSERT INTO logs VALUES (1, 'First');
INSERT INTO logs VALUES (2, 'Second');
SELECT * FROM logs AS OF TRANSACTION 1;" \
    "Query OK"

echo ""

# ===================================================================
# Test Group 5: Time-Travel with AS OF SCN
# ===================================================================
echo -e "${YELLOW}Test Group 5: Time-Travel Queries (AS OF SCN)${NC}"
echo "-----------------------------------"

run_test "AS OF SCN (System Change Number)" \
    "CREATE TABLE metrics (id INT, value INT);
INSERT INTO metrics VALUES (1, 42);
SELECT * FROM metrics AS OF SCN 100;" \
    "Query OK"

run_test "AS OF SCN with numeric ID" \
    "CREATE TABLE events (id INT, type TEXT);
INSERT INTO events VALUES (1, 'startup');
SELECT * FROM events AS OF SCN 500;" \
    "Query OK"

echo ""

# ===================================================================
# Test Group 6: Complex Time-Travel Scenarios
# ===================================================================
echo -e "${YELLOW}Test Group 6: Complex Time-Travel Scenarios${NC}"
echo "-----------------------------------"

run_test "Multiple inserts with time-travel" \
    "CREATE TABLE users (id INT, name TEXT);
INSERT INTO users VALUES (1, 'Alice');
INSERT INTO users VALUES (2, 'Bob');
INSERT INTO users VALUES (3, 'Charlie');
SELECT COUNT(*) FROM users AS OF NOW;" \
    "Query OK"

run_test "Time-travel with WHERE clause" \
    "CREATE TABLE data (id INT, value TEXT);
INSERT INTO data VALUES (1, 'A');
INSERT INTO data VALUES (2, 'B');
SELECT * FROM data AS OF TIMESTAMP '2025-11-28 00:00:00' WHERE id = 1;" \
    "Query OK"

echo ""

# ===================================================================
# Test Group 7: REPL Meta-Commands
# ===================================================================
echo -e "${YELLOW}Test Group 7: REPL Meta-Commands${NC}"
echo "-----------------------------------"

run_test "Help command (\\h)" \
    "\\h" \
    "Meta Commands"

run_test "List system views (\\dS)" \
    "\\dS" \
    "System"

run_test "Branch list command (\\branches)" \
    "\\branches" \
    "Database Branches"

run_test "Snapshots command (\\snapshots)" \
    "\\snapshots" \
    "Time-Travel"

echo ""

# ===================================================================
# Test Group 8: Basic SQL Still Works
# ===================================================================
echo -e "${YELLOW}Test Group 8: Basic SQL Operations (Regression Tests)${NC}"
echo "-----------------------------------"

run_test "Basic CREATE TABLE" \
    "CREATE TABLE test (id INT, value TEXT);
\d test" \
    "id"

run_test "Basic INSERT" \
    "CREATE TABLE t1 (id INT);
INSERT INTO t1 VALUES (1);
INSERT INTO t1 VALUES (2);
SELECT COUNT(*) FROM t1;" \
    "Query OK"

run_test "Basic SELECT" \
    "CREATE TABLE t2 (name TEXT);
INSERT INTO t2 VALUES ('Alice');
SELECT * FROM t2;" \
    "Alice"

run_test "DROP TABLE" \
    "CREATE TABLE t3 (id INT);
DROP TABLE t3;" \
    "Query OK"

echo ""

# ===================================================================
# Summary
# ===================================================================
echo "=========================================="
echo "Test Results Summary"
echo "=========================================="
echo -e "Passed: ${GREEN}${PASSED}${NC}"
echo -e "Failed: ${RED}${FAILED}${NC}"
echo "Total:  $((PASSED + FAILED))"
echo ""

if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}All tests passed!${NC}"
    exit 0
else
    echo -e "${RED}Some tests failed.${NC}"
    exit 1
fi
