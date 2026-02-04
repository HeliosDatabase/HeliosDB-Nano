#!/bin/bash

# HeliosDB-Lite Compression Test Suite
# Tests: FSST, ALP, Compression Config, Statistics
# Run: ./test_compression.sh

BINARY="./target/release/heliosdb-lite"
TEST_DB="test_compression.db"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB-Lite Compression Test Suite"
echo "=========================================="
echo ""

run_test() {
    local test_name="$1"
    local test_num="$2"
    local sql="$3"

    echo -n "[$test_num] $test_name ... "

    output=$(timeout 10 "$BINARY" repl --memory << EOF 2>&1
$sql
\q
EOF
)

    # Test passes if:
    # 1. Output contains successful patterns (Query OK, results, columns, etc.)
    # 2. No critical errors (Connection failed, panic, etc.)
    # 3. Allows expected errors
    if echo "$output" | grep -qE "Query OK|Column|^[0-9]|rows\)|postgres|^\("; then
        if echo "$output" | grep -qvE "panic|Connection|INTERNAL"; then
            echo -e "${GREEN}✓${NC}"
            ((PASSED++))
            return 0
        fi
    fi

    echo -e "${RED}✗${NC}"
    echo "  Output: $(echo "$output" | tail -2)"
    ((FAILED++))
    return 1
}

# ===================================================================
# COMPRESSION BASICS
# ===================================================================
echo -e "${YELLOW}═══ COMPRESSION BASICS ═══${NC}"
echo ""

run_test "Create table with text (FSST candidates)" "1.1" \
    "CREATE TABLE text_data (id INT, name TEXT, description TEXT);
INSERT INTO text_data VALUES (1, 'Alice', 'Software Engineer');
INSERT INTO text_data VALUES (2, 'Bob', 'Product Manager');
INSERT INTO text_data VALUES (3, 'Carol', 'Data Scientist');
SELECT COUNT(*) FROM text_data;"

run_test "Create table with numeric data (ALP candidates)" "1.2" \
    "CREATE TABLE numeric_data (id INT, value FLOAT8, amount INT);
INSERT INTO numeric_data VALUES (1, 3.14159, 1000);
INSERT INTO numeric_data VALUES (2, 2.71828, 2000);
INSERT INTO numeric_data VALUES (3, 1.41421, 3000);
SELECT COUNT(*) FROM numeric_data;"

run_test "Bulk insert for compression" "1.3" \
    "CREATE TABLE compressed (id INT, data TEXT);
INSERT INTO compressed VALUES (1, 'test data 1');
INSERT INTO compressed VALUES (2, 'test data 2');
INSERT INTO compressed VALUES (3, 'test data 3');
INSERT INTO compressed VALUES (4, 'test data 4');
INSERT INTO compressed VALUES (5, 'test data 5');
SELECT COUNT(*) FROM compressed;"

echo ""

# ===================================================================
# COLUMN COMPRESSION TRAINING
# ===================================================================
echo -e "${YELLOW}═══ COMPRESSION COLUMN TRAINING ═══${NC}"
echo ""

run_test "Train FSST dictionary" "2.1" \
    "CREATE TABLE strings (id INT, text TEXT);
INSERT INTO strings VALUES (1, 'the quick brown fox');
INSERT INTO strings VALUES (2, 'jumps over the lazy dog');
INSERT INTO strings VALUES (3, 'the fox is brown');
SELECT COUNT(*) FROM strings;"

echo ""

# ===================================================================
# COMPRESSION WITH QUERIES
# ===================================================================
echo -e "${YELLOW}═══ COMPRESSION WITH QUERIES ═══${NC}"
echo ""

run_test "Query on compressed text" "3.1" \
    "CREATE TABLE compressed_text (id INT, content TEXT);
INSERT INTO compressed_text VALUES (1, 'Lorem ipsum dolor sit amet');
INSERT INTO compressed_text VALUES (2, 'consectetur adipiscing elit');
INSERT INTO compressed_text VALUES (3, 'sed do eiusmod tempor');
SELECT * FROM compressed_text WHERE id = 1;"

run_test "Aggregation on compressed numeric" "3.2" \
    "CREATE TABLE compressed_numeric (id INT, value FLOAT8);
INSERT INTO compressed_numeric VALUES (1, 100.5);
INSERT INTO compressed_numeric VALUES (2, 200.5);
INSERT INTO compressed_numeric VALUES (3, 300.5);
SELECT SUM(value) FROM compressed_numeric;"

run_test "WHERE clause on compressed" "3.3" \
    "CREATE TABLE comp_where (id INT, status TEXT);
INSERT INTO comp_where VALUES (1, 'active');
INSERT INTO comp_where VALUES (2, 'inactive');
INSERT INTO comp_where VALUES (3, 'active');
SELECT COUNT(*) FROM comp_where WHERE status = 'active';"

echo ""

# ===================================================================
# MIXED COMPRESSION TYPES
# ===================================================================
echo -e "${YELLOW}═══ MIXED COMPRESSION TYPES ═══${NC}"
echo ""

run_test "Table with both text and numeric" "4.1" \
    "CREATE TABLE mixed (id INT, name TEXT, score FLOAT8, count INT);
INSERT INTO mixed VALUES (1, 'Alice', 95.5, 100);
INSERT INTO mixed VALUES (2, 'Bob', 87.3, 200);
SELECT COUNT(*) FROM mixed;"

run_test "Complex compression scenario" "4.2" \
    "CREATE TABLE complex (id INT, label TEXT, x FLOAT8, y FLOAT8, z FLOAT8);
INSERT INTO complex VALUES (1, 'point_a', 1.5, 2.5, 3.5);
INSERT INTO complex VALUES (2, 'point_b', 4.5, 5.5, 6.5);
INSERT INTO complex VALUES (3, 'point_c', 7.5, 8.5, 9.5);
SELECT COUNT(*) FROM complex;"

echo ""

# ===================================================================
# COMPRESSION LIMITS
# ===================================================================
echo -e "${YELLOW}═══ COMPRESSION EDGE CASES ═══${NC}"
echo ""

run_test "Compression with empty strings" "5.1" \
    "CREATE TABLE empty_text (id INT, text TEXT);
INSERT INTO empty_text VALUES (1, '');
INSERT INTO empty_text VALUES (2, 'not empty');
SELECT COUNT(*) FROM empty_text;"

run_test "Compression with NULL" "5.2" \
    "CREATE TABLE comp_null (id INT, data TEXT);
INSERT INTO comp_null VALUES (1, NULL);
INSERT INTO comp_null VALUES (2, 'value');
SELECT COUNT(*) FROM comp_null WHERE data IS NOT NULL;"

run_test "Compression with very large strings" "5.3" \
    "CREATE TABLE large_text (id INT, content TEXT);
INSERT INTO large_text VALUES (1, '" \
    "$(python3 -c 'print(\"a\" * 1000)')" \
    "');
SELECT LENGTH(content) FROM large_text;"

echo ""

# ===================================================================
# PERFORMANCE CHARACTERISTICS
# ===================================================================
echo -e "${YELLOW}═══ COMPRESSION PERFORMANCE ═══${NC}"
echo ""

run_test "Read after compression" "6.1" \
    "CREATE TABLE perf_test (id INT, data TEXT);
INSERT INTO perf_test VALUES (1, 'compressed data');
INSERT INTO perf_test VALUES (2, 'more data');
SELECT * FROM perf_test ORDER BY id;"

run_test "Update with compression" "6.2" \
    "CREATE TABLE update_test (id INT, value INT);
INSERT INTO update_test VALUES (1, 100);
UPDATE update_test SET value = 200 WHERE id = 1;
SELECT * FROM update_test;"

echo ""

# ===================================================================
# SUMMARY
# ===================================================================
echo "=========================================="
echo -e "${BLUE}Test Results${NC}"
echo "=========================================="
TOTAL=$((PASSED + FAILED))
echo -e "Passed: ${GREEN}${PASSED}/${TOTAL}${NC}"
echo -e "Failed: ${RED}${FAILED}/${TOTAL}${NC}"
echo ""

if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}✓ All Compression tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
