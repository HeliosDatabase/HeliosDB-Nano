#!/bin/bash
# T-SQL Dialect Test Script
# Tests SQL Server procedural language features
# Run with: ./test_tsql.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELIOS_CLI="${SCRIPT_DIR}/../../target/release/heliosdb-nano"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "======================================"
echo "T-SQL Dialect Test Suite"
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
echo "--- T-SQL Date/Time Functions ---"
echo ""

# GETDATE()
run_test "GETDATE() function" \
    "SELECT GETDATE();" \
    "ok"

# GETUTCDATE()
run_test "GETUTCDATE() function" \
    "SELECT GETUTCDATE();" \
    "ok"

# SYSDATETIME()
run_test "SYSDATETIME() function" \
    "SELECT SYSDATETIME();" \
    "ok"

echo ""
echo "--- T-SQL CREATE FUNCTION ---"
echo ""

# CREATE FUNCTION with T-SQL style
run_test "CREATE FUNCTION T-SQL style" \
    "CREATE FUNCTION dbo.GetTotal(a INTEGER, b INTEGER) RETURNS INTEGER LANGUAGE SQL AS 'SELECT a + b';" \
    "ok"

echo ""
echo "--- DROP FUNCTION Tests ---"
echo ""

# DROP FUNCTION
run_test "DROP FUNCTION T-SQL" \
    "DROP FUNCTION IF EXISTS dbo.GetTotal;" \
    "ok"

echo ""
echo "--- CALL/EXEC Tests ---"
echo ""

# CALL (used as EXEC in T-SQL)
run_test "CALL procedure" \
    "CALL stored_proc(1);" \
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
