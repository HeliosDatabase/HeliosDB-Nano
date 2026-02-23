#!/bin/bash
# PL/pgSQL Dialect Test Script
# Tests PostgreSQL procedural language features
# Run with: ./test_plpgsql.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELIOS_CLI="${SCRIPT_DIR}/../../target/release/heliosdb-nano"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "======================================"
echo "PL/pgSQL Dialect Test Suite"
echo "======================================"
echo ""

# Check if binary exists
if [ ! -f "$HELIOS_CLI" ]; then
    echo -e "${YELLOW}Building release binary...${NC}"
    cd "$SCRIPT_DIR/../.." && cargo build --release
fi

# Test counter
TESTS_PASSED=0
TESTS_FAILED=0

# Function to run a test
run_test() {
    local name="$1"
    local sql="$2"
    local expected_result="$3"  # "ok" for success, "error" for expected failure

    echo -n "Testing: $name... "

    result=$(echo "$sql" | "$HELIOS_CLI" repl --memory 2>&1 | tail -5)

    if [[ "$expected_result" == "ok" ]]; then
        if echo "$result" | grep -q "Query OK\|row"; then
            echo -e "${GREEN}PASSED${NC}"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            echo -e "${RED}FAILED${NC}"
            echo "  SQL: $sql"
            echo "  Result: $result"
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    else
        if echo "$result" | grep -q "ERROR"; then
            echo -e "${GREEN}PASSED (expected error)${NC}"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            echo -e "${RED}FAILED (expected error but got success)${NC}"
            echo "  SQL: $sql"
            echo "  Result: $result"
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    fi
}

echo ""
echo "--- CREATE FUNCTION Tests ---"
echo ""

# Basic CREATE FUNCTION
run_test "CREATE FUNCTION with RETURNS" \
    "CREATE FUNCTION add_numbers(a INTEGER, b INTEGER) RETURNS INTEGER LANGUAGE SQL AS 'SELECT a + b';" \
    "ok"

# CREATE OR REPLACE FUNCTION
run_test "CREATE OR REPLACE FUNCTION" \
    "CREATE OR REPLACE FUNCTION multiply(x INTEGER, y INTEGER) RETURNS INTEGER LANGUAGE SQL AS 'SELECT x * y';" \
    "ok"

# CREATE FUNCTION with no parameters
run_test "CREATE FUNCTION no params" \
    "CREATE FUNCTION get_one() RETURNS INTEGER LANGUAGE SQL AS 'SELECT 1';" \
    "ok"

# CREATE FUNCTION returning TEXT
run_test "CREATE FUNCTION returning TEXT" \
    "CREATE FUNCTION hello_world() RETURNS TEXT LANGUAGE SQL AS 'SELECT hello';" \
    "ok"

# CREATE FUNCTION with OUT parameter
run_test "CREATE FUNCTION with OUT param" \
    "CREATE FUNCTION get_result(IN x INTEGER, OUT result INTEGER) LANGUAGE SQL AS 'SELECT x * 2';" \
    "ok"

# CREATE FUNCTION with INOUT parameter
run_test "CREATE FUNCTION with INOUT param" \
    "CREATE FUNCTION double_value(INOUT val INTEGER) LANGUAGE SQL AS 'SELECT val * 2';" \
    "ok"

echo ""
echo "--- DROP FUNCTION Tests ---"
echo ""

# DROP FUNCTION
run_test "DROP FUNCTION" \
    "DROP FUNCTION add_numbers;" \
    "ok"

# DROP FUNCTION IF EXISTS
run_test "DROP FUNCTION IF EXISTS" \
    "DROP FUNCTION IF EXISTS nonexistent_function;" \
    "ok"

echo ""
echo "--- CALL Statement Tests ---"
echo ""

# CALL procedure
run_test "CALL procedure with args" \
    "CALL my_procedure(1, 2);" \
    "ok"

# CALL procedure with no args
run_test "CALL procedure no args" \
    "CALL my_procedure();" \
    "ok"

echo ""
echo "--- Date/Time Functions (PL/pgSQL style) ---"
echo ""

# NOW()
run_test "NOW() function" \
    "SELECT NOW();" \
    "ok"

# CURRENT_TIMESTAMP
run_test "CURRENT_TIMESTAMP" \
    "SELECT CURRENT_TIMESTAMP;" \
    "ok"

# CURRENT_DATE
run_test "CURRENT_DATE" \
    "SELECT CURRENT_DATE;" \
    "ok"

# CURRENT_TIME
run_test "CURRENT_TIME" \
    "SELECT CURRENT_TIME;" \
    "ok"

echo ""
echo "======================================"
echo "Test Summary"
echo "======================================"
echo -e "Passed: ${GREEN}$TESTS_PASSED${NC}"
echo -e "Failed: ${RED}$TESTS_FAILED${NC}"
echo "Total: $((TESTS_PASSED + TESTS_FAILED))"
echo ""

if [ $TESTS_FAILED -gt 0 ]; then
    exit 1
fi
