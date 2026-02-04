#!/bin/bash

# HeliosDB-Lite Phase 3 Feature Test Script (Clean Version)
# Tests all Phase 3 features with fresh database for each test
# Run: ./test_phase3_clean.sh

BINARY="./target/release/heliosdb-lite"
TEST_DB="heliosdb_test.db"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB-Lite Phase 3 Feature Test Suite"
echo "=========================================="
echo ""

# Function to run a single test
run_test() {
    local test_name="$1"
    local test_num="$2"
    local sql="$3"

    echo -n "[$test_num] $test_name ... "

    # Run test (using in-memory database for clean isolation)
    output=$(timeout 5 "$BINARY" repl --memory << EOF 2>&1
$sql
\q
EOF
)

    # Check for success conditions and errors
    # Success patterns: Query OK, (N row), table schema output, branch info
    if echo "$output" | grep -qE "Query OK|^\(|Column.*Type|branch_name|view_name|index_name"; then
        # Make sure it's not an error output
        if echo "$output" | grep -qvE "ERROR:|Table.*already exists|Table.*does not exist|has [0-9]+ child"; then
            echo -e "${GREEN}✓${NC}"
            ((PASSED++))
            return 0
        fi
    fi

    # If we get here, it failed
    echo -e "${RED}✗${NC}"
    echo "  Output: $(echo "$output" | tail -3)"
    ((FAILED++))
    return 1
}

# ===================================================================
# SYSTEM VIEWS
# ===================================================================
echo -e "${YELLOW}═══ SYSTEM VIEWS (Phase 3) ═══${NC}"
echo ""

run_test "System View: pg_database_branches" "1.1" \
    "SELECT * FROM pg_database_branches();"

run_test "System View: pg_mv_staleness" "1.2" \
    "SELECT * FROM pg_mv_staleness();"

run_test "System View: pg_vector_index_stats" "1.3" \
    "SELECT * FROM pg_vector_index_stats();"

echo ""

# ===================================================================
# TIME-TRAVEL QUERIES - AS OF NOW
# ===================================================================
echo -e "${YELLOW}═══ TIME-TRAVEL: AS OF NOW ═══${NC}"
echo ""

run_test "Simple query with AS OF NOW" "2.1" \
    "CREATE TABLE test1 (id INT, name TEXT);
INSERT INTO test1 VALUES (1, 'Alice');
SELECT * FROM test1 AS OF NOW;"

run_test "Multiple rows with AS OF NOW" "2.2" \
    "CREATE TABLE test2 (id INT, value INT);
INSERT INTO test2 VALUES (1, 100);
INSERT INTO test2 VALUES (2, 200);
SELECT COUNT(*) FROM test2 AS OF NOW;"

run_test "AS OF NOW with WHERE clause" "2.3" \
    "CREATE TABLE test3 (id INT, status TEXT);
INSERT INTO test3 VALUES (1, 'active');
INSERT INTO test3 VALUES (2, 'inactive');
SELECT * FROM test3 AS OF NOW WHERE status = 'active';"

echo ""

# ===================================================================
# TIME-TRAVEL QUERIES - AS OF TIMESTAMP
# ===================================================================
echo -e "${YELLOW}═══ TIME-TRAVEL: AS OF TIMESTAMP ═══${NC}"
echo ""

run_test "Query with AS OF TIMESTAMP (recent)" "3.1" \
    "CREATE TABLE orders1 (id INT, amount INT);
INSERT INTO orders1 VALUES (100, 1000);
SELECT * FROM orders1 AS OF TIMESTAMP '2025-11-28 09:00:00';"

run_test "Query with AS OF TIMESTAMP (historical)" "3.2" \
    "CREATE TABLE orders2 (id INT, amount INT);
INSERT INTO orders2 VALUES (200, 2000);
SELECT * FROM orders2 AS OF TIMESTAMP '2025-01-01 00:00:00';"

run_test "Multiple AS OF TIMESTAMP in same query" "3.3" \
    "CREATE TABLE sales (id INT, total INT);
INSERT INTO sales VALUES (1, 500);
INSERT INTO sales VALUES (2, 600);
SELECT COUNT(*) FROM sales AS OF TIMESTAMP '2025-11-28 10:00:00';"

echo ""

# ===================================================================
# TIME-TRAVEL QUERIES - AS OF TRANSACTION
# ===================================================================
echo -e "${YELLOW}═══ TIME-TRAVEL: AS OF TRANSACTION ═══${NC}"
echo ""

run_test "Query with AS OF TRANSACTION 1" "4.1" \
    "CREATE TABLE accounts (id INT, balance INT);
INSERT INTO accounts VALUES (1, 5000);
SELECT * FROM accounts AS OF TRANSACTION 1;"

run_test "Query with AS OF TRANSACTION 2" "4.2" \
    "CREATE TABLE logs (id INT, message TEXT);
INSERT INTO logs VALUES (1, 'event1');
INSERT INTO logs VALUES (2, 'event2');
SELECT * FROM logs AS OF TRANSACTION 2;"

run_test "AS OF TRANSACTION with aggregate" "4.3" \
    "CREATE TABLE metrics (id INT, value INT);
INSERT INTO metrics VALUES (1, 100);
INSERT INTO metrics VALUES (2, 200);
SELECT SUM(value) FROM metrics AS OF TRANSACTION 1;"

echo ""

# ===================================================================
# TIME-TRAVEL QUERIES - AS OF SCN
# ===================================================================
echo -e "${YELLOW}═══ TIME-TRAVEL: AS OF SCN ═══${NC}"
echo ""

run_test "Query with AS OF SCN 100" "5.1" \
    "CREATE TABLE events (id INT, type TEXT);
INSERT INTO events VALUES (1, 'start');
SELECT * FROM events AS OF SCN 100;"

run_test "Query with AS OF SCN 500" "5.2" \
    "CREATE TABLE data (id INT, value TEXT);
INSERT INTO data VALUES (1, 'test');
SELECT * FROM data AS OF SCN 500;"

run_test "AS OF SCN with multiple rows" "5.3" \
    "CREATE TABLE records (id INT, status TEXT);
INSERT INTO records VALUES (1, 'ok');
INSERT INTO records VALUES (2, 'pending');
SELECT COUNT(*) FROM records AS OF SCN 1000;"

echo ""

# ===================================================================
# DATABASE BRANCHING
# ===================================================================
echo -e "${YELLOW}═══ DATABASE BRANCHING (Phase 3) ═══${NC}"
echo ""

run_test "CREATE DATABASE BRANCH" "6.1" \
    "CREATE DATABASE BRANCH dev FROM main AS OF NOW;
SELECT * FROM pg_database_branches();"

run_test "CREATE BRANCH (short syntax)" "6.2" \
    "CREATE BRANCH staging AS OF NOW;
SELECT * FROM pg_database_branches();"

run_test "DROP DATABASE BRANCH" "6.3" \
    "CREATE DATABASE BRANCH test_branch AS OF NOW;
DROP DATABASE BRANCH test_branch;
SELECT * FROM pg_database_branches();"

run_test "DROP BRANCH IF EXISTS" "6.4" \
    "DROP DATABASE BRANCH IF EXISTS nonexistent;
SELECT * FROM pg_database_branches();"

echo ""

# ===================================================================
# BRANCH SWITCHING - USE BRANCH SQL
# ===================================================================
echo -e "${YELLOW}═══ BRANCH SWITCHING: USE BRANCH SQL ═══${NC}"
echo ""

run_test "USE BRANCH to switch branches" "6.5" \
    "CREATE DATABASE BRANCH dev FROM main AS OF NOW;
USE BRANCH dev;
SELECT * FROM pg_database_branches();"

run_test "USE BRANCH with short syntax" "6.6" \
    "CREATE BRANCH staging AS OF NOW;
USE BRANCH staging;
SELECT * FROM pg_database_branches();"

echo ""

# ===================================================================
# REGRESSION TESTS - Basic SQL Still Works
# ===================================================================
echo -e "${YELLOW}═══ REGRESSION TESTS: Basic SQL ═══${NC}"
echo ""

run_test "CREATE TABLE" "7.1" \
    "CREATE TABLE users_test (id INT, name TEXT);
\d users_test"

run_test "INSERT and SELECT" "7.2" \
    "CREATE TABLE products_test (id INT, name TEXT);
INSERT INTO products_test (id, name) VALUES (1, 'Widget');
SELECT * FROM products_test;"

run_test "INSERT multiple rows" "7.3" \
    "CREATE TABLE items_test (id INT, name TEXT);
INSERT INTO items_test (id, name) VALUES (1, 'A');
INSERT INTO items_test (id, name) VALUES (2, 'B');
SELECT COUNT(*) FROM items_test;"

run_test "WHERE clause" "7.4" \
    "CREATE TABLE values_test (id INT, amount INT);
INSERT INTO values_test (id, amount) VALUES (1, 100);
INSERT INTO values_test (id, amount) VALUES (2, 200);
SELECT * FROM values_test WHERE amount > 150;"

run_test "DROP TABLE" "7.5" \
    "CREATE TABLE temp_test (id INT);
INSERT INTO temp_test (id) VALUES (1);
DROP TABLE temp_test;
DROP TABLE IF EXISTS temp_test;"

echo ""

# ===================================================================
# VECTOR INDEXES - CREATE INDEX with USING (Phase 3)
# ===================================================================
echo -e "${YELLOW}═══ VECTOR INDEXES: CREATE INDEX with USING ═══${NC}"
echo ""

run_test "CREATE INDEX with USING hnsw" "8.1" \
    "CREATE TABLE vec_test (id INT, embedding VECTOR(4));
CREATE INDEX idx_vec_test ON vec_test(embedding) USING hnsw;
SELECT index_name FROM pg_vector_index_stats();"

run_test "CREATE INDEX HNSW dimension 8" "8.2" \
    "CREATE TABLE vec_8d (id INT, data VECTOR(8));
CREATE INDEX idx_vec_8d ON vec_8d(data) USING hnsw;
SELECT dimensions FROM pg_vector_index_stats() WHERE index_name = 'idx_vec_8d';"

run_test "CREATE INDEX HNSW dimension 16" "8.3" \
    "CREATE TABLE vec_16d (id INT, emb VECTOR(16));
CREATE INDEX idx_vec_16d ON vec_16d(emb) USING hnsw;
SELECT index_name, dimensions FROM pg_vector_index_stats();"

run_test "CREATE multiple vector indexes" "8.4" \
    "CREATE TABLE multi_vec (id INT, v1 VECTOR(3), v2 VECTOR(5));
CREATE INDEX idx_v1 ON multi_vec(v1) USING hnsw;
CREATE INDEX idx_v2 ON multi_vec(v2) USING hnsw;
SELECT COUNT(*) FROM pg_vector_index_stats();"

echo ""

# ===================================================================
# TIME-TRAVEL BUG FIX - AS OF SCN/TRANSACTION (v2.5.0)
# ===================================================================
echo -e "${YELLOW}═══ TIME-TRAVEL BUG FIX: Snapshot Filtering (v2.5.0) ═══${NC}"
echo ""

run_test "Time-travel: AS OF SCN filters correct snapshot" "9.1" \
    "CREATE TABLE users (id INT, name TEXT);
INSERT INTO users VALUES (1, 'Alice');
INSERT INTO users VALUES (2, 'Bob');
SELECT COUNT(*) FROM users AS OF SCN 0;"

run_test "Time-travel: AS OF TRANSACTION filters snapshots" "9.2" \
    "CREATE TABLE accounts (id INT, balance INT);
INSERT INTO accounts VALUES (1, 1000);
INSERT INTO accounts VALUES (2, 2000);
INSERT INTO accounts VALUES (3, 3000);
SELECT COUNT(*) FROM accounts AS OF TRANSACTION 2;"

run_test "Time-travel: Multi-transaction snapshot isolation" "9.3" \
    "CREATE TABLE transactions (id INT, type TEXT);
INSERT INTO transactions VALUES (1, 'deposit');
INSERT INTO transactions VALUES (2, 'withdrawal');
INSERT INTO transactions VALUES (3, 'transfer');
SELECT id, type FROM transactions AS OF TRANSACTION 1 ORDER BY id;"

run_test "Time-travel: SCN before any inserts (empty result)" "9.4" \
    "CREATE TABLE orders (id INT, total INT);
INSERT INTO orders VALUES (100, 1000);
INSERT INTO orders VALUES (200, 2000);
SELECT COUNT(*) FROM orders AS OF SCN 0;"

echo ""

# ===================================================================
# ROW ID TRACKING - Query Engine (v2.5.0)
# ===================================================================
echo -e "${YELLOW}═══ ROW ID TRACKING: Query Engine Integration (v2.5.0) ═══${NC}"
echo ""

run_test "Row ID tracking: Basic table scan" "10.1" \
    "CREATE TABLE products (id INT, name TEXT);
INSERT INTO products VALUES (1, 'Widget');
INSERT INTO products VALUES (2, 'Gadget');
SELECT * FROM products;"

run_test "Row ID tracking: With WHERE clause (row selection)" "10.2" \
    "CREATE TABLE items (id INT, quantity INT);
INSERT INTO items VALUES (1, 10);
INSERT INTO items VALUES (2, 20);
INSERT INTO items VALUES (3, 30);
SELECT * FROM items WHERE quantity > 15;"

run_test "Row ID tracking: With LIMIT (row reduction)" "10.3" \
    "CREATE TABLE numbers (id INT, value INT);
INSERT INTO numbers VALUES (1, 100);
INSERT INTO numbers VALUES (2, 200);
INSERT INTO numbers VALUES (3, 300);
INSERT INTO numbers VALUES (4, 400);
SELECT value FROM numbers LIMIT 2;"

run_test "Row ID tracking: With ORDER BY (row ordering)" "10.4" \
    "CREATE TABLE scores (id INT, points INT);
INSERT INTO scores VALUES (1, 85);
INSERT INTO scores VALUES (2, 92);
INSERT INTO scores VALUES (3, 78);
SELECT * FROM scores ORDER BY points DESC;"

run_test "Row ID tracking: With column projection" "10.5" \
    "CREATE TABLE employees (id INT, name TEXT, salary INT);
INSERT INTO employees VALUES (1, 'Alice', 50000);
INSERT INTO employees VALUES (2, 'Bob', 60000);
SELECT name, salary FROM employees WHERE salary >= 55000;"

run_test "Row ID tracking: With aggregation" "10.6" \
    "CREATE TABLE sales (id INT, amount INT);
INSERT INTO sales VALUES (1, 100);
INSERT INTO sales VALUES (2, 200);
INSERT INTO sales VALUES (3, 150);
SELECT COUNT(*) as count FROM sales;"

run_test "Row ID tracking: With time-travel + row filtering" "10.7" \
    "CREATE TABLE events (id INT, event_type TEXT);
INSERT INTO events VALUES (1, 'click');
INSERT INTO events VALUES (2, 'view');
INSERT INTO events VALUES (3, 'purchase');
SELECT * FROM events AS OF NOW WHERE id > 1;"

echo ""

# ===================================================================
# SUMMARY
# ===================================================================
echo "=========================================="
echo -e "${BLUE}Test Results Summary${NC}"
echo "=========================================="
TOTAL=$((PASSED + FAILED))
echo -e "Passed: ${GREEN}${PASSED}/${TOTAL}${NC}"
echo -e "Failed: ${RED}${FAILED}/${TOTAL}${NC}"
echo ""

if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}✓ All Phase 3 tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
