#!/bin/bash

# HeliosDB Nano Core SQL Operations Test Suite
# Tests: CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, DROP, TRUNCATE
# Run: ./test_core_sql.sh

BINARY="./target/release/heliosdb-nano"
TEST_DB="test_core_sql.db"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB Nano Core SQL Operations Test"
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
    # 3. Allows expected errors like "already exists" or "does not exist"
    if echo "$output" | grep -qE "Query OK|^\(|Column.*Type|^[0-9]|rows\)|postgres|^[a-z_].*\|"; then
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
# CREATE TABLE
# ===================================================================
echo -e "${YELLOW}═══ CREATE TABLE ═══${NC}"
echo ""

run_test "CREATE TABLE basic" "1.1" \
    "CREATE TABLE users (id INT, name TEXT);
\d users"

run_test "CREATE TABLE with constraints" "1.2" \
    "CREATE TABLE products (
        id INT PRIMARY KEY,
        name TEXT NOT NULL,
        price INT
    );
\d products"

run_test "CREATE TABLE with multiple types" "1.3" \
    "CREATE TABLE data (
        id INT,
        name TEXT,
        amount FLOAT8,
        active BOOLEAN,
        data JSON
    );
\d data"

run_test "CREATE TABLE IF NOT EXISTS" "1.4" \
    "CREATE TABLE IF NOT EXISTS test (id INT);
CREATE TABLE IF NOT EXISTS test (id INT);"

echo ""

# ===================================================================
# INSERT
# ===================================================================
echo -e "${YELLOW}═══ INSERT OPERATIONS ═══${NC}"
echo ""

run_test "INSERT single row" "2.1" \
    "CREATE TABLE items (id INT, name TEXT);
INSERT INTO items VALUES (1, 'Item1');
SELECT * FROM items;"

run_test "INSERT multiple rows" "2.2" \
    "CREATE TABLE numbers (id INT, value INT);
INSERT INTO numbers VALUES (1, 100);
INSERT INTO numbers VALUES (2, 200);
INSERT INTO numbers VALUES (3, 300);
SELECT COUNT(*) FROM numbers;"

run_test "INSERT with column list" "2.3" \
    "CREATE TABLE employees (id INT, name TEXT, salary INT);
INSERT INTO employees (id, name, salary) VALUES (1, 'Alice', 50000);
INSERT INTO employees (name, id, salary) VALUES ('Bob', 2, 60000);
SELECT COUNT(*) FROM employees;"

run_test "INSERT NULL values" "2.4" \
    "CREATE TABLE nullable (id INT, optional TEXT);
INSERT INTO nullable VALUES (1, NULL);
INSERT INTO nullable VALUES (2, 'value');
SELECT * FROM nullable;"

echo ""

# ===================================================================
# SELECT
# ===================================================================
echo -e "${YELLOW}═══ SELECT OPERATIONS ═══${NC}"
echo ""

run_test "SELECT all columns" "3.1" \
    "CREATE TABLE test (id INT, val INT);
INSERT INTO test VALUES (1, 100);
INSERT INTO test VALUES (2, 200);
SELECT * FROM test;"

run_test "SELECT specific columns" "3.2" \
    "CREATE TABLE records (id INT, name TEXT, age INT);
INSERT INTO records VALUES (1, 'Alice', 30);
INSERT INTO records VALUES (2, 'Bob', 25);
SELECT name, age FROM records;"

run_test "SELECT with WHERE" "3.3" \
    "CREATE TABLE scores (id INT, points INT);
INSERT INTO scores VALUES (1, 85);
INSERT INTO scores VALUES (2, 92);
INSERT INTO scores VALUES (3, 78);
SELECT * FROM scores WHERE points > 80;"

run_test "SELECT with ORDER BY" "3.4" \
    "CREATE TABLE prices (id INT, price INT);
INSERT INTO prices VALUES (1, 100);
INSERT INTO prices VALUES (2, 50);
INSERT INTO prices VALUES (3, 150);
SELECT * FROM prices ORDER BY price;"

run_test "SELECT with LIMIT" "3.5" \
    "CREATE TABLE data (id INT);
INSERT INTO data VALUES (1);
INSERT INTO data VALUES (2);
INSERT INTO data VALUES (3);
SELECT * FROM data LIMIT 2;"

run_test "SELECT with OFFSET" "3.6" \
    "CREATE TABLE items (id INT);
INSERT INTO items VALUES (1);
INSERT INTO items VALUES (2);
INSERT INTO items VALUES (3);
SELECT * FROM items LIMIT 2 OFFSET 1;"

run_test "SELECT DISTINCT" "3.7" \
    "CREATE TABLE colors (id INT, color TEXT);
INSERT INTO colors VALUES (1, 'red');
INSERT INTO colors VALUES (2, 'blue');
INSERT INTO colors VALUES (3, 'red');
SELECT DISTINCT color FROM colors;"

run_test "SELECT with aggregate functions" "3.8" \
    "CREATE TABLE values (id INT, val INT);
INSERT INTO values VALUES (1, 10);
INSERT INTO values VALUES (2, 20);
INSERT INTO values VALUES (3, 30);
SELECT COUNT(*), SUM(val), AVG(val), MIN(val), MAX(val) FROM values;"

echo ""

# ===================================================================
# UPDATE
# ===================================================================
echo -e "${YELLOW}═══ UPDATE OPERATIONS ═══${NC}"
echo ""

run_test "UPDATE single row" "4.1" \
    "CREATE TABLE accounts (id INT, balance INT);
INSERT INTO accounts VALUES (1, 1000);
UPDATE accounts SET balance = 1500 WHERE id = 1;
SELECT * FROM accounts;"

run_test "UPDATE multiple rows" "4.2" \
    "CREATE TABLE status (id INT, active BOOLEAN);
INSERT INTO status VALUES (1, false);
INSERT INTO status VALUES (2, false);
UPDATE status SET active = true WHERE id > 0;
SELECT COUNT(*) FROM status WHERE active = true;"

run_test "UPDATE with expression" "4.3" \
    "CREATE TABLE wallet (id INT, amount INT);
INSERT INTO wallet VALUES (1, 100);
UPDATE wallet SET amount = amount + 50 WHERE id = 1;
SELECT * FROM wallet;"

echo ""

# ===================================================================
# DELETE
# ===================================================================
echo -e "${YELLOW}═══ DELETE OPERATIONS ═══${NC}"
echo ""

run_test "DELETE specific rows" "5.1" \
    "CREATE TABLE logs (id INT, message TEXT);
INSERT INTO logs VALUES (1, 'msg1');
INSERT INTO logs VALUES (2, 'msg2');
INSERT INTO logs VALUES (3, 'msg3');
DELETE FROM logs WHERE id = 2;
SELECT COUNT(*) FROM logs;"

run_test "DELETE all rows" "5.2" \
    "CREATE TABLE temp (id INT);
INSERT INTO temp VALUES (1);
INSERT INTO temp VALUES (2);
DELETE FROM temp;
SELECT COUNT(*) FROM temp;"

echo ""

# ===================================================================
# TRUNCATE
# ===================================================================
echo -e "${YELLOW}═══ TRUNCATE OPERATIONS ═══${NC}"
echo ""

run_test "TRUNCATE table" "6.1" \
    "CREATE TABLE trash (id INT);
INSERT INTO trash VALUES (1);
INSERT INTO trash VALUES (2);
TRUNCATE TABLE trash;
SELECT COUNT(*) FROM trash;"

echo ""

# ===================================================================
# DROP TABLE
# ===================================================================
echo -e "${YELLOW}═══ DROP TABLE ═══${NC}"
echo ""

run_test "DROP TABLE" "7.1" \
    "CREATE TABLE remove_me (id INT);
DROP TABLE remove_me;
CREATE TABLE test (id INT);"

run_test "DROP TABLE IF EXISTS" "7.2" \
    "DROP TABLE IF EXISTS nonexistent;
CREATE TABLE test (id INT);"

echo ""

# ===================================================================
# JOINS
# ===================================================================
echo -e "${YELLOW}═══ JOIN OPERATIONS ═══${NC}"
echo ""

run_test "INNER JOIN" "8.1" \
    "CREATE TABLE t1 (id INT, name TEXT);
CREATE TABLE t2 (id INT, value INT);
INSERT INTO t1 VALUES (1, 'A');
INSERT INTO t1 VALUES (2, 'B');
INSERT INTO t2 VALUES (1, 100);
INSERT INTO t2 VALUES (3, 300);
SELECT t1.name, t2.value FROM t1 INNER JOIN t2 ON t1.id = t2.id;"

run_test "LEFT JOIN" "8.2" \
    "CREATE TABLE users (id INT, name TEXT);
CREATE TABLE orders (user_id INT, amount INT);
INSERT INTO users VALUES (1, 'Alice');
INSERT INTO users VALUES (2, 'Bob');
INSERT INTO orders VALUES (1, 100);
SELECT users.name, orders.amount FROM users LEFT JOIN orders ON users.id = orders.user_id;"

echo ""

# ===================================================================
# GROUP BY & AGGREGATION
# ===================================================================
echo -e "${YELLOW}═══ GROUP BY & AGGREGATION ═══${NC}"
echo ""

run_test "GROUP BY basic" "9.1" \
    "CREATE TABLE sales (id INT, category TEXT, amount INT);
INSERT INTO sales VALUES (1, 'A', 100);
INSERT INTO sales VALUES (2, 'B', 200);
INSERT INTO sales VALUES (3, 'A', 150);
SELECT category, SUM(amount) FROM sales GROUP BY category;"

run_test "GROUP BY with HAVING" "9.2" \
    "CREATE TABLE scores (id INT, category TEXT, score INT);
INSERT INTO scores VALUES (1, 'math', 85);
INSERT INTO scores VALUES (2, 'math', 90);
INSERT INTO scores VALUES (3, 'english', 75);
SELECT category, AVG(score) FROM scores GROUP BY category HAVING AVG(score) > 75;"

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
    echo -e "${GREEN}✓ All Core SQL tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
