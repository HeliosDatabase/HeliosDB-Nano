#!/bin/bash
# DB2 SQL PL Dialect Test Script
# Tests IBM DB2 procedural language features
# Run with: ./test_db2pl.sh


SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELIOS_CLI="${SCRIPT_DIR}/../../target/release/heliosdb-nano"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "======================================"
echo "DB2 SQL PL Dialect Test Suite"
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
echo "--- DB2 Date/Time Functions ---"
echo ""

# CURRENT TIMESTAMP
run_test "CURRENT_TIMESTAMP" \
    "SELECT CURRENT_TIMESTAMP;" \
    "ok"

# CURRENT DATE
run_test "CURRENT_DATE" \
    "SELECT CURRENT_DATE;" \
    "ok"

# CURRENT TIME
run_test "CURRENT_TIME" \
    "SELECT CURRENT_TIME;" \
    "ok"

echo ""
echo "--- DB2 CREATE FUNCTION ---"
echo ""

# CREATE FUNCTION with DB2 style
run_test "CREATE FUNCTION DB2 style" \
    "CREATE FUNCTION calc_tax(amount INTEGER) RETURNS INTEGER LANGUAGE SQL AS 'SELECT amount * 21 / 100';" \
    "ok"

# CREATE OR REPLACE FUNCTION
run_test "CREATE OR REPLACE FUNCTION" \
    "CREATE OR REPLACE FUNCTION get_vat(amount INTEGER) RETURNS INTEGER LANGUAGE SQL AS 'SELECT amount * 19 / 100';" \
    "ok"

echo ""
echo "--- DROP FUNCTION Tests ---"
echo ""

# DROP FUNCTION
run_test "DROP FUNCTION DB2 style" \
    "DROP FUNCTION IF EXISTS calc_tax;" \
    "ok"

echo ""
echo "--- CALL Tests ---"
echo ""

# CALL procedure
run_test "CALL procedure DB2 style" \
    "CALL process_batch(100);" \
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
