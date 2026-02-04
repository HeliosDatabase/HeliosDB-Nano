#!/bin/bash
# Run all procedural language dialect tests
# Run with: ./run_all_tests.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo ""
echo -e "${BLUE}============================================${NC}"
echo -e "${BLUE}Procedural Language Dialect Test Suite${NC}"
echo -e "${BLUE}============================================${NC}"
echo ""
echo "This suite tests HeliosDB's compatibility with:"
echo "  - PL/pgSQL (PostgreSQL)"
echo "  - T-SQL (SQL Server)"
echo "  - PL/SQL (Oracle)"
echo "  - DB2 SQL PL (IBM DB2)"
echo ""

# Make scripts executable
chmod +x "$SCRIPT_DIR"/*.sh

# Track overall results
TOTAL_PASSED=0
TOTAL_FAILED=0

run_test_suite() {
    local name="$1"
    local script="$2"

    echo ""
    echo -e "${YELLOW}Running $name tests...${NC}"
    echo ""

    if "$SCRIPT_DIR/$script"; then
        echo -e "${GREEN}$name tests completed successfully${NC}"
    else
        echo -e "${RED}$name tests had failures${NC}"
    fi
}

# Run all test suites
run_test_suite "PL/pgSQL" "test_plpgsql.sh"
run_test_suite "T-SQL" "test_tsql.sh"
run_test_suite "PL/SQL" "test_plsql.sh"
run_test_suite "DB2 SQL PL" "test_db2pl.sh"

echo ""
echo -e "${BLUE}============================================${NC}"
echo -e "${BLUE}All Tests Complete${NC}"
echo -e "${BLUE}============================================${NC}"
echo ""
