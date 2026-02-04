#!/bin/bash
#
# Protocol Integration Test Runner for HeliosDB-Lite
#
# This script runs comprehensive protocol tests for both PostgreSQL
# and Oracle wire protocol implementations.
#

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "=========================================="
echo "HeliosDB-Lite Protocol Integration Tests"
echo "=========================================="
echo ""

# Color codes for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Test results tracking
TESTS_PASSED=0
TESTS_FAILED=0

# Check Python installation
if ! command -v python3 &> /dev/null; then
    echo -e "${RED}Error: python3 is not installed${NC}"
    exit 1
fi

# Check if virtual environment exists
if [ ! -d "venv" ]; then
    echo -e "${YELLOW}Creating Python virtual environment...${NC}"
    python3 -m venv venv
fi

# Activate virtual environment
source venv/bin/activate

# Install requirements
echo -e "${YELLOW}Installing Python dependencies...${NC}"
pip install -q --upgrade pip
pip install -q -r requirements.txt
echo ""

# Function to run a test
run_test() {
    local test_name=$1
    local test_script=$2

    echo "=========================================="
    echo "Running: $test_name"
    echo "=========================================="
    echo ""

    if python3 "$test_script"; then
        echo ""
        echo -e "${GREEN}✅ $test_name PASSED${NC}"
        ((TESTS_PASSED++))
    else
        echo ""
        echo -e "${RED}❌ $test_name FAILED${NC}"
        ((TESTS_FAILED++))
    fi

    echo ""
}

# Check if HeliosDB server is running
echo "Checking if HeliosDB server is running..."
if ! nc -z localhost 5432 2>/dev/null && ! nc -z localhost 1521 2>/dev/null; then
    echo -e "${YELLOW}Warning: HeliosDB server may not be running${NC}"
    echo "Please ensure the server is started before running tests"
    echo ""
    read -p "Press Enter to continue anyway or Ctrl+C to abort..."
    echo ""
fi

# Run PostgreSQL protocol tests
run_test "PostgreSQL Protocol Test" "test_postgres.py"

# Run Oracle protocol tests
run_test "Oracle Protocol Test" "test_oracle.py"

# Summary
echo "=========================================="
echo "Test Summary"
echo "=========================================="
echo ""
echo -e "Tests Passed: ${GREEN}${TESTS_PASSED}${NC}"
echo -e "Tests Failed: ${RED}${TESTS_FAILED}${NC}"
echo ""

if [ $TESTS_FAILED -eq 0 ]; then
    echo -e "${GREEN}✅ All protocol tests completed successfully!${NC}"
    exit 0
else
    echo -e "${RED}❌ Some tests failed. Please review the output above.${NC}"
    exit 1
fi
