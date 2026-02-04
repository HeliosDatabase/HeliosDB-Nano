#!/bin/bash
# Security audit script for HeliosDB Lite
# Runs comprehensive security checks

set -e

echo "========================================="
echo "HeliosDB Lite Security Audit"
echo "========================================="
echo ""

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Track overall status
OVERALL_STATUS=0

# Function to print status
print_status() {
    if [ $1 -eq 0 ]; then
        echo -e "${GREEN}✓ $2${NC}"
    else
        echo -e "${RED}✗ $2${NC}"
        OVERALL_STATUS=1
    fi
}

# 1. Dependency Audit
echo "1. Running cargo audit (dependency vulnerabilities)..."
if cargo audit --deny warnings 2>/dev/null; then
    print_status 0 "No known vulnerabilities in dependencies"
else
    print_status 1 "Vulnerabilities found in dependencies"
fi
echo ""

# 2. Supply Chain Security
echo "2. Running cargo deny (supply chain security)..."
if cargo deny check 2>/dev/null; then
    print_status 0 "Supply chain security checks passed"
else
    print_status 1 "Supply chain security issues found"
fi
echo ""

# 3. Unsafe Code Detection
echo "3. Checking for unsafe code..."
UNSAFE_COUNT=$(grep -r "unsafe" src/ --include="*.rs" | grep -v "//" | wc -l || echo "0")
if [ "$UNSAFE_COUNT" -eq 0 ]; then
    print_status 0 "No unsafe blocks found in production code"
else
    print_status 1 "Found $UNSAFE_COUNT unsafe blocks"
fi
echo ""

# 4. Unwrap Detection
echo "4. Checking for unwrap() calls..."
UNWRAP_COUNT=$(grep -r "\.unwrap()" src/ --include="*.rs" | grep -v "//" | wc -l || echo "0")
if [ "$UNWRAP_COUNT" -eq 0 ]; then
    print_status 0 "No unwrap() calls found in production code"
else
    print_status 1 "Found $UNWRAP_COUNT unwrap() calls"
fi
echo ""

# 5. Panic Detection
echo "5. Checking for panic!() calls..."
PANIC_COUNT=$(grep -r "panic!" src/ --include="*.rs" | grep -v "//" | wc -l || echo "0")
if [ "$PANIC_COUNT" -eq 0 ]; then
    print_status 0 "No panic!() calls found in production code"
else
    print_status 1 "Found $PANIC_COUNT panic!() calls"
fi
echo ""

# 6. Clippy Security Lints
echo "6. Running Clippy with security lints..."
if cargo clippy --all-targets --all-features -- \
    -W clippy::unwrap_used \
    -W clippy::expect_used \
    -W clippy::panic \
    -W clippy::integer_arithmetic \
    -W clippy::indexing_slicing 2>&1 | grep -q "warning\|error"; then
    print_status 1 "Clippy security warnings found"
else
    print_status 0 "No Clippy security warnings"
fi
echo ""

# 7. Test Coverage
echo "7. Running security tests..."
if cargo test --test sql_injection_tests --test resource_exhaustion_tests --test crypto_tests 2>&1 | tail -1 | grep -q "test result: ok"; then
    print_status 0 "Security tests passed"
else
    print_status 1 "Security tests failed"
fi
echo ""

# 8. Code Complexity Check
echo "8. Checking code complexity..."
echo "   (Manual review recommended for functions with cyclomatic complexity > 15)"
echo ""

# Final Summary
echo "========================================="
echo "Security Audit Summary"
echo "========================================="

if [ $OVERALL_STATUS -eq 0 ]; then
    echo -e "${GREEN}All security checks passed!${NC}"
    echo ""
    echo "Security Grade: A+ (10/10)"
    exit 0
else
    echo -e "${YELLOW}Some security checks failed or have warnings.${NC}"
    echo "Review the output above for details."
    echo ""
    echo "Security Grade: Needs attention"
    exit 1
fi
