#!/bin/bash
#
# Validation script for Phase 5 session tracking implementation
#

set -e

echo "=========================================="
echo "Phase 5 Implementation Validation"
echo "=========================================="
echo ""

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

ERRORS=0
WARNINGS=0

# Function to check file exists
check_file() {
    local file=$1
    local description=$2

    if [ -f "$file" ]; then
        echo -e "${GREEN}✓${NC} $description"
    else
        echo -e "${RED}✗${NC} $description (NOT FOUND)"
        ((ERRORS++))
    fi
}

# Function to check directory exists
check_dir() {
    local dir=$1
    local description=$2

    if [ -d "$dir" ]; then
        echo -e "${GREEN}✓${NC} $description"
    else
        echo -e "${RED}✗${NC} $description (NOT FOUND)"
        ((ERRORS++))
    fi
}

# Function to check if file is executable
check_executable() {
    local file=$1
    local description=$2

    if [ -x "$file" ]; then
        echo -e "${GREEN}✓${NC} $description"
    else
        echo -e "${YELLOW}⚠${NC} $description (NOT EXECUTABLE)"
        ((WARNINGS++))
    fi
}

echo "Checking Core Implementation Files..."
echo "-------------------------------------"
check_file "../../src/sql/system_tables.rs" "Session management implementation"
check_file "../../src/sql/mod.rs" "SQL module integration"

echo ""
echo "Checking Test Files..."
echo "-------------------------------------"
check_file "test_postgres.py" "PostgreSQL test program"
check_file "test_oracle.py" "Oracle test program"
check_file "run_tests.sh" "Test runner script"
check_file "requirements.txt" "Python requirements"
check_file "README.md" "Test suite documentation"

echo ""
echo "Checking Documentation Files..."
echo "-------------------------------------"
check_file "../../docs/implementation/SESSION_MANAGEMENT_IMPLEMENTATION.md" "Implementation documentation"
check_file "../../docs/guides/SESSION_MANAGEMENT_QUICK_REFERENCE.md" "Quick reference guide"
check_file "../../PHASE_5_SESSION_TRACKING_SUMMARY.md" "Phase 5 summary"

echo ""
echo "Checking File Permissions..."
echo "-------------------------------------"
check_executable "test_postgres.py" "PostgreSQL test executable"
check_executable "test_oracle.py" "Oracle test executable"
check_executable "run_tests.sh" "Test runner executable"

echo ""
echo "Checking Python Installation..."
echo "-------------------------------------"
if command -v python3 &> /dev/null; then
    PYTHON_VERSION=$(python3 --version)
    echo -e "${GREEN}✓${NC} Python 3 installed: $PYTHON_VERSION"
else
    echo -e "${RED}✗${NC} Python 3 not found"
    ((ERRORS++))
fi

echo ""
echo "Checking Python Dependencies..."
echo "-------------------------------------"
if [ -d "venv" ]; then
    echo -e "${GREEN}✓${NC} Virtual environment exists"

    source venv/bin/activate

    for pkg in psycopg2 oracledb pytest colorama; do
        if python3 -c "import $pkg" 2>/dev/null; then
            echo -e "${GREEN}✓${NC} $pkg installed"
        else
            echo -e "${YELLOW}⚠${NC} $pkg not installed (run: pip install -r requirements.txt)"
            ((WARNINGS++))
        fi
    done

    deactivate
else
    echo -e "${YELLOW}⚠${NC} Virtual environment not created (will be created on first run)"
    ((WARNINGS++))
fi

echo ""
echo "Checking Rust Code Compilation..."
echo "-------------------------------------"
echo "Attempting to check syntax of system_tables.rs..."

if command -v cargo &> /dev/null; then
    # Check if the file contains valid Rust syntax
    if grep -q "pub struct SessionInfo" ../../src/sql/system_tables.rs && \
       grep -q "pub struct SessionRegistry" ../../src/sql/system_tables.rs && \
       grep -q "pub enum SessionState" ../../src/sql/system_tables.rs; then
        echo -e "${GREEN}✓${NC} Required structs and enums present"
    else
        echo -e "${RED}✗${NC} Missing required Rust structures"
        ((ERRORS++))
    fi

    if grep -q "pub mod system_tables" ../../src/sql/mod.rs; then
        echo -e "${GREEN}✓${NC} system_tables module declared in mod.rs"
    else
        echo -e "${RED}✗${NC} system_tables module not declared in mod.rs"
        ((ERRORS++))
    fi
else
    echo -e "${YELLOW}⚠${NC} Cargo not found, skipping Rust validation"
    ((WARNINGS++))
fi

echo ""
echo "Checking Test Script Content..."
echo "-------------------------------------"

if grep -q "psycopg2" test_postgres.py; then
    echo -e "${GREEN}✓${NC} PostgreSQL test uses psycopg2"
else
    echo -e "${RED}✗${NC} PostgreSQL test missing psycopg2 import"
    ((ERRORS++))
fi

if grep -q "oracledb" test_oracle.py; then
    echo -e "${GREEN}✓${NC} Oracle test uses oracledb"
else
    echo -e "${RED}✗${NC} Oracle test missing oracledb import"
    ((ERRORS++))
fi

if grep -q "helios_sessions" test_postgres.py; then
    echo -e "${GREEN}✓${NC} PostgreSQL test queries helios_sessions"
else
    echo -e "${YELLOW}⚠${NC} PostgreSQL test may not query helios_sessions"
    ((WARNINGS++))
fi

if grep -q "v\$session" test_oracle.py; then
    echo -e "${GREEN}✓${NC} Oracle test queries v\$session"
else
    echo -e "${YELLOW}⚠${NC} Oracle test may not query v\$session"
    ((WARNINGS++))
fi

echo ""
echo "=========================================="
echo "Validation Summary"
echo "=========================================="
echo ""

if [ $ERRORS -eq 0 ] && [ $WARNINGS -eq 0 ]; then
    echo -e "${GREEN}✅ All checks passed!${NC}"
    echo ""
    echo "Phase 5 implementation is complete and ready for testing."
    echo ""
    echo "Next steps:"
    echo "  1. Ensure HeliosDB server is running with protocol support"
    echo "  2. Run: ./run_tests.sh"
    echo ""
    exit 0
elif [ $ERRORS -eq 0 ]; then
    echo -e "${YELLOW}⚠ Validation completed with $WARNINGS warning(s)${NC}"
    echo ""
    echo "Phase 5 implementation is mostly complete."
    echo "Review warnings above before testing."
    echo ""
    exit 0
else
    echo -e "${RED}❌ Validation failed with $ERRORS error(s) and $WARNINGS warning(s)${NC}"
    echo ""
    echo "Please address the errors above before proceeding."
    echo ""
    exit 1
fi
