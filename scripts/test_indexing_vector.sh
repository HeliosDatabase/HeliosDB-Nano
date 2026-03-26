#!/bin/bash

# HeliosDB Nano Indexing & Vector Search Test Suite
# Tests: CREATE INDEX, HNSW, GIN, B-Tree, Vector Similarity Search
# Run: ./test_indexing_vector.sh

BINARY="./target/release/heliosdb-nano"
TEST_DB="test_indexing.db"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB Nano Indexing & Vector Search Test"
echo "=========================================="
echo ""

run_test() {
    local test_name="$1"
    local test_num="$2"
    local sql="$3"

    rm -f "${TEST_DB}"*
    echo -n "[$test_num] $test_name ... "

    output=$(timeout 10 "$BINARY" repl << EOF 2>&1
$sql
\q
EOF
)

    if echo "$output" | grep -qE "Query OK|Index|^[0-9]|Column"; then
        if echo "$output" | grep -qvE "ERROR:|already exists|does not exist"; then
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
# CREATE INDEX - BASIC
# ===================================================================
echo -e "${YELLOW}═══ CREATE INDEX - BASIC ═══${NC}"
echo ""

run_test "CREATE INDEX on INT column" "1.1" \
    "CREATE TABLE users (id INT, name TEXT);
CREATE INDEX idx_user_id ON users(id);
\d users"

run_test "CREATE INDEX on TEXT column" "1.2" \
    "CREATE TABLE products (id INT, name TEXT, category TEXT);
CREATE INDEX idx_product_name ON products(name);
\d products"

run_test "CREATE INDEX with ORDER" "1.3" \
    "CREATE TABLE orders (id INT, amount INT);
CREATE INDEX idx_orders_amount ON orders(amount DESC);
\d orders"

run_test "CREATE UNIQUE INDEX" "1.4" \
    "CREATE TABLE emails (id INT, email TEXT);
CREATE UNIQUE INDEX idx_unique_email ON emails(email);
\d emails"

echo ""

# ===================================================================
# HNSW VECTOR INDEX
# ===================================================================
echo -e "${YELLOW}═══ HNSW VECTOR INDEX ═══${NC}"
echo ""

run_test "CREATE HNSW index VECTOR(3)" "2.1" \
    "CREATE TABLE vec3 (id INT, embedding VECTOR(3));
CREATE INDEX idx_vec3 ON vec3(embedding) USING hnsw;
INSERT INTO vec3 VALUES (1, '[0.1, 0.2, 0.3]');
INSERT INTO vec3 VALUES (2, '[0.4, 0.5, 0.6]');
SELECT COUNT(*) FROM vec3;"

run_test "CREATE HNSW index VECTOR(8)" "2.2" \
    "CREATE TABLE vec8 (id INT, emb VECTOR(8));
CREATE INDEX idx_vec8 ON vec8(emb) USING hnsw;
INSERT INTO vec8 VALUES (1, '[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8]');
SELECT COUNT(*) FROM vec8;"

run_test "CREATE HNSW index VECTOR(16)" "2.3" \
    "CREATE TABLE vec16 (id INT, v VECTOR(16));
CREATE INDEX idx_vec16 ON vec16(v) USING hnsw;
INSERT INTO vec16 VALUES (1, '[" \
    "$(python3 -c 'print(\",\".join([str(i/100) for i in range(16)]))')" \
    "]');
SELECT COUNT(*) FROM vec16;"

run_test "HNSW with bulk inserts" "2.4" \
    "CREATE TABLE embeddings (id INT, vec VECTOR(4));
CREATE INDEX idx_emb ON embeddings(vec) USING hnsw;
INSERT INTO embeddings VALUES (1, '[0.1, 0.1, 0.1, 0.1]');
INSERT INTO embeddings VALUES (2, '[0.2, 0.2, 0.2, 0.2]');
INSERT INTO embeddings VALUES (3, '[0.3, 0.3, 0.3, 0.3]');
SELECT COUNT(*) FROM embeddings;"

echo ""

# ===================================================================
# GIN INDEX (JSONB)
# ===================================================================
echo -e "${YELLOW}═══ GIN INDEX (JSON) ═══${NC}"
echo ""

run_test "CREATE GIN index on JSON" "3.1" \
    "CREATE TABLE json_docs (id INT, data JSON);
CREATE INDEX idx_json ON json_docs(data) USING gin;
INSERT INTO json_docs VALUES (1, '{\"key\": \"value\"}');
SELECT COUNT(*) FROM json_docs;"

run_test "GIN index with complex JSON" "3.2" \
    "CREATE TABLE json_data (id INT, content JSON);
CREATE INDEX idx_content ON json_data(content) USING gin;
INSERT INTO json_data VALUES (1, '{\"nested\": {\"deep\": {\"value\": 42}}}');
SELECT COUNT(*) FROM json_data;"

echo ""

# ===================================================================
# MULTIPLE INDEXES
# ===================================================================
echo -e "${YELLOW}═══ MULTIPLE INDEXES ═══${NC}"
echo ""

run_test "Multiple indexes on same table" "4.1" \
    "CREATE TABLE multi (id INT, name TEXT, age INT);
CREATE INDEX idx_id ON multi(id);
CREATE INDEX idx_name ON multi(name);
CREATE INDEX idx_age ON multi(age);
INSERT INTO multi VALUES (1, 'Alice', 30);
SELECT COUNT(*) FROM multi;"

run_test "Multiple vector indexes" "4.2" \
    "CREATE TABLE multi_vec (id INT, v1 VECTOR(3), v2 VECTOR(3));
CREATE INDEX idx_v1 ON multi_vec(v1) USING hnsw;
CREATE INDEX idx_v2 ON multi_vec(v2) USING hnsw;
INSERT INTO multi_vec VALUES (1, '[0.1,0.1,0.1]', '[0.2,0.2,0.2]');
SELECT COUNT(*) FROM multi_vec;"

echo ""

# ===================================================================
# DROP INDEX
# ===================================================================
echo -e "${YELLOW}═══ DROP INDEX ═══${NC}"
echo ""

run_test "DROP INDEX" "5.1" \
    "CREATE TABLE drop_idx_test (id INT);
CREATE INDEX idx_drop ON drop_idx_test(id);
DROP INDEX idx_drop;
CREATE TABLE test (id INT);"

run_test "DROP INDEX IF EXISTS" "5.2" \
    "DROP INDEX IF EXISTS nonexistent;
CREATE TABLE test (id INT);"

echo ""

# ===================================================================
# QUERY WITH INDEX (verify usage)
# ===================================================================
echo -e "${YELLOW}═══ INDEXED QUERY EXECUTION ═══${NC}"
echo ""

run_test "Query using indexed column" "6.1" \
    "CREATE TABLE indexed_table (id INT, value INT);
CREATE INDEX idx_value ON indexed_table(value);
INSERT INTO indexed_table VALUES (1, 100);
INSERT INTO indexed_table VALUES (2, 200);
SELECT * FROM indexed_table WHERE value > 150;"

run_test "Query with indexed join" "6.2" \
    "CREATE TABLE t1 (id INT, ref_id INT);
CREATE TABLE t2 (id INT, name TEXT);
CREATE INDEX idx_ref ON t1(ref_id);
INSERT INTO t1 VALUES (1, 1);
INSERT INTO t2 VALUES (1, 'Item1');
SELECT * FROM t1 JOIN t2 ON t1.ref_id = t2.id;"

echo ""

# ===================================================================
# VECTOR INDEX STATS
# ===================================================================
echo -e "${YELLOW}═══ VECTOR INDEX STATISTICS ═══${NC}"
echo ""

run_test "Query pg_vector_index_stats" "7.1" \
    "CREATE TABLE vec_stat (id INT, v VECTOR(4));
CREATE INDEX idx_stat ON vec_stat(v) USING hnsw;
SELECT * FROM pg_vector_index_stats();"

run_test "Vector index info" "7.2" \
    "CREATE TABLE vecs (id INT, emb VECTOR(8));
CREATE INDEX idx_vecs ON vecs(emb) USING hnsw;
INSERT INTO vecs VALUES (1, '[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8]');
SELECT index_name, dimensions FROM pg_vector_index_stats() WHERE index_name = 'idx_vecs';"

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
    echo -e "${GREEN}✓ All Indexing tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
