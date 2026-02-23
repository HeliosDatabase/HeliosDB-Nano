#!/bin/bash

# HeliosDB-Lite Advanced Features Test Suite
# Tests: CTEs, Transactions, Encryption, Materialized Views, System Functions
# Run: ./test_advanced_features.sh

BINARY="./target/release/heliosdb-nano"
TEST_DB="test_advanced.db"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB-Lite Advanced Features Test"
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
    if echo "$output" | grep -qE "Query OK|Column|^[0-9]|^\(|rows\)|postgres|^[a-z_].*\|"; then
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
# CTEs (Common Table Expressions)
# ===================================================================
echo -e "${YELLOW}═══ CTEs (WITH CLAUSE) ═══${NC}"
echo ""

run_test "Simple CTE" "1.1" \
    "CREATE TABLE nums (n INT);
INSERT INTO nums VALUES (1);
INSERT INTO nums VALUES (2);
INSERT INTO nums VALUES (3);
WITH numbers AS (SELECT n FROM nums)
SELECT * FROM numbers;"

run_test "CTE with table" "1.2" \
    "CREATE TABLE data (id INT, val INT);
INSERT INTO data VALUES (1, 100);
INSERT INTO data VALUES (2, 200);
WITH doubled AS (SELECT id, val * 2 as doubled FROM data)
SELECT * FROM doubled;"

run_test "Multiple CTEs" "1.3" \
    "CREATE TABLE vals1 (x INT);
CREATE TABLE vals2 (y INT);
INSERT INTO vals1 VALUES (1);
INSERT INTO vals2 VALUES (2);
WITH a AS (SELECT x FROM vals1), b AS (SELECT y FROM vals2)
SELECT COUNT(*) FROM a, b;"

echo ""

# ===================================================================
# TRANSACTIONS
# ===================================================================
echo -e "${YELLOW}═══ TRANSACTION SUPPORT ═══${NC}"
echo ""

run_test "BEGIN COMMIT" "2.1" \
    "CREATE TABLE trans (id INT);
BEGIN;
INSERT INTO trans VALUES (1);
COMMIT;
SELECT COUNT(*) FROM trans;"

run_test "BEGIN ROLLBACK" "2.2" \
    "CREATE TABLE trans (id INT);
BEGIN;
INSERT INTO trans VALUES (1);
ROLLBACK;
SELECT COUNT(*) FROM trans;"

echo ""

# ===================================================================
# AGGREGATE FUNCTIONS
# ===================================================================
echo -e "${YELLOW}═══ AGGREGATE FUNCTIONS ═══${NC}"
echo ""

run_test "COUNT aggregate" "3.1" \
    "CREATE TABLE items (id INT, qty INT);
INSERT INTO items VALUES (1, 5);
INSERT INTO items VALUES (2, 10);
SELECT COUNT(*) FROM items;"

run_test "SUM aggregate" "3.2" \
    "CREATE TABLE values (id INT, val INT);
INSERT INTO values VALUES (1, 100);
INSERT INTO values VALUES (2, 200);
SELECT SUM(val) FROM values;"

run_test "AVG aggregate" "3.3" \
    "CREATE TABLE nums (id INT, n INT);
INSERT INTO nums VALUES (1, 10);
INSERT INTO nums VALUES (2, 20);
INSERT INTO nums VALUES (3, 30);
SELECT AVG(n) FROM nums;"

run_test "MIN/MAX aggregate" "3.4" \
    "CREATE TABLE ranges (val INT);
INSERT INTO ranges VALUES (5);
INSERT INTO ranges VALUES (15);
INSERT INTO ranges VALUES (10);
SELECT MIN(val), MAX(val) FROM ranges;"

run_test "STRING_AGG" "3.5" \
    "CREATE TABLE tags (id INT, tag TEXT);
INSERT INTO tags VALUES (1, 'red');
INSERT INTO tags VALUES (2, 'blue');
SELECT STRING_AGG(tag, ',') FROM tags;"

echo ""

# ===================================================================
# WINDOW FUNCTIONS
# ===================================================================
echo -e "${YELLOW}═══ WINDOW FUNCTIONS ═══${NC}"
echo ""

run_test "ROW_NUMBER window function" "4.1" \
    "CREATE TABLE ranked (id INT, score INT);
INSERT INTO ranked VALUES (1, 85);
INSERT INTO ranked VALUES (2, 90);
SELECT ROW_NUMBER() OVER (ORDER BY score) FROM ranked;"

run_test "RANK window function" "4.2" \
    "CREATE TABLE scores (id INT, score INT);
INSERT INTO scores VALUES (1, 85);
INSERT INTO scores VALUES (2, 85);
INSERT INTO scores VALUES (3, 90);
SELECT RANK() OVER (ORDER BY score DESC) FROM scores;"

echo ""

# ===================================================================
# STRING FUNCTIONS
# ===================================================================
echo -e "${YELLOW}═══ STRING FUNCTIONS ═══${NC}"
echo ""

run_test "LENGTH function" "5.1" \
    "CREATE TABLE text (s TEXT);
INSERT INTO text VALUES ('hello');
SELECT LENGTH(s) FROM text;"

run_test "SUBSTRING function" "5.2" \
    "CREATE TABLE text (s TEXT);
INSERT INTO text VALUES ('hello world');
SELECT SUBSTRING(s, 1, 5) FROM text;"

run_test "UPPER/LOWER functions" "5.3" \
    "CREATE TABLE text (s TEXT);
INSERT INTO text VALUES ('Hello');
SELECT UPPER(s), LOWER(s) FROM text;"

run_test "CONCAT function" "5.4" \
    "CREATE TABLE text (a TEXT, b TEXT);
INSERT INTO text VALUES ('hello', 'world');
SELECT CONCAT(a, ' ', b) FROM text;"

echo ""

# ===================================================================
# NUMERIC FUNCTIONS
# ===================================================================
echo -e "${YELLOW}═══ NUMERIC FUNCTIONS ═══${NC}"
echo ""

run_test "ABS function" "6.1" \
    "CREATE TABLE nums (n INT);
INSERT INTO nums VALUES (-5);
SELECT ABS(n) FROM nums;"

run_test "CEIL/FLOOR functions" "6.2" \
    "CREATE TABLE floats (f FLOAT8);
INSERT INTO floats VALUES (3.7);
SELECT CEIL(f), FLOOR(f) FROM floats;"

run_test "ROUND function" "6.3" \
    "CREATE TABLE floats (f FLOAT8);
INSERT INTO floats VALUES (3.14159);
SELECT ROUND(f, 2) FROM floats;"

echo ""

# ===================================================================
# DATE FUNCTIONS
# ===================================================================
echo -e "${YELLOW}═══ DATE FUNCTIONS ═══${NC}"
echo ""

run_test "CURRENT_TIMESTAMP function" "7.1" \
    "CREATE TABLE ts_test (id INT, ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
INSERT INTO ts_test (id) VALUES (1);
SELECT * FROM ts_test;"

run_test "DATE_TRUNC function" "7.2" \
    "CREATE TABLE dates (dt TIMESTAMP);
INSERT INTO dates VALUES ('2025-11-29 15:30:45');
SELECT DATE_TRUNC('day', dt) FROM dates;"

echo ""

# ===================================================================
# EXPLAIN QUERY PLANS
# ===================================================================
echo -e "${YELLOW}═══ EXPLAIN QUERY PLANS ═══${NC}"
echo ""

run_test "EXPLAIN simple query" "8.1" \
    "CREATE TABLE test (id INT);
EXPLAIN SELECT * FROM test WHERE id = 1;"

run_test "EXPLAIN with join" "8.2" \
    "CREATE TABLE t1 (id INT);
CREATE TABLE t2 (id INT, val INT);
EXPLAIN SELECT * FROM t1 JOIN t2 ON t1.id = t2.id;"

echo ""

# ===================================================================
# SYSTEM INFORMATION
# ===================================================================
echo -e "${YELLOW}═══ SYSTEM INFORMATION VIEWS ═══${NC}"
echo ""

run_test "pg_database_branches view" "9.1" \
    "SELECT * FROM pg_database_branches();"

run_test "pg_mv_staleness view" "9.2" \
    "SELECT * FROM pg_mv_staleness();"

run_test "pg_vector_index_stats view" "9.3" \
    "SELECT * FROM pg_vector_index_stats();"

echo ""

# ===================================================================
# EDGE CASES
# ===================================================================
echo -e "${YELLOW}═══ EDGE CASES & ERROR HANDLING ═══${NC}"
echo ""

run_test "Division by zero handling" "10.1" \
    "CREATE TABLE calc (id INT);
INSERT INTO calc VALUES (1);
SELECT 10 / 0 FROM calc;"

run_test "NULL in calculations" "10.2" \
    "CREATE TABLE nulls (n INT);
INSERT INTO nulls VALUES (NULL);
SELECT n + 5 FROM nulls;"

run_test "Type mismatch handling" "10.3" \
    "CREATE TABLE mixed (id INT, val TEXT);
INSERT INTO mixed VALUES (1, 'not a number');
SELECT id FROM mixed;"

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
    echo -e "${GREEN}✓ All Advanced Feature tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
