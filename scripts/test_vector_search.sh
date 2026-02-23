#!/bin/bash

# HeliosDB-Lite Vector Search Test Suite
# Tests: VECTOR columns, HNSW indexes, similarity search, KNN, PQ compression
# Run: ./test_vector_search.sh

BINARY="./target/release/heliosdb-nano"
TEST_DB="test_vector_search.db"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB-Lite Vector Search Test Suite"
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

    if echo "$output" | grep -qE "Query OK|Index|^[0-9]|Column|rows\)"; then
        if echo "$output" | grep -qvE "ERROR:|panic|Connection|INTERNAL"; then
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
# CREATE TABLE WITH VECTOR COLUMN
# ===================================================================
echo -e "${YELLOW}═══ CREATE TABLE WITH VECTOR COLUMN ═══${NC}"
echo ""

run_test "Create table with VECTOR(3)" "1.1" \
    "CREATE TABLE vectors3 (id INT, vec VECTOR(3));
\d vectors3"

run_test "Create table with VECTOR(8)" "1.2" \
    "CREATE TABLE vectors8 (id INT, embedding VECTOR(8));
\d vectors8"

run_test "Create table with VECTOR(128)" "1.3" \
    "CREATE TABLE embeddings (id INT, content TEXT, embedding VECTOR(128));
\d embeddings"

run_test "Create table with VECTOR(384)" "1.4" \
    "CREATE TABLE documents (id INT, title TEXT, embedding VECTOR(384));
\d documents"

run_test "Multiple VECTOR columns" "1.5" \
    "CREATE TABLE multi_vec (id INT, text_vec VECTOR(768), image_vec VECTOR(512));
\d multi_vec"

echo ""

# ===================================================================
# INSERT VECTORS
# ===================================================================
echo -e "${YELLOW}═══ INSERT VECTOR DATA ═══${NC}"
echo ""

run_test "Insert VECTOR(3)" "2.1" \
    "CREATE TABLE v3 (id INT, vec VECTOR(3));
INSERT INTO v3 VALUES (1, '[0.1, 0.2, 0.3]');
SELECT COUNT(*) FROM v3;"

run_test "Insert VECTOR(4)" "2.2" \
    "CREATE TABLE v4 (id INT, vec VECTOR(4));
INSERT INTO v4 VALUES (1, '[1.0, 0.0, 0.0, 0.0]');
INSERT INTO v4 VALUES (2, '[0.0, 1.0, 0.0, 0.0]');
INSERT INTO v4 VALUES (3, '[0.0, 0.0, 1.0, 0.0]');
SELECT COUNT(*) FROM v4;"

run_test "Insert normalized vector" "2.3" \
    "CREATE TABLE normalized (id INT, vec VECTOR(3));
INSERT INTO normalized VALUES (1, '[0.577, 0.577, 0.577]');
SELECT COUNT(*) FROM normalized;"

run_test "Insert with NULL vector" "2.4" \
    "CREATE TABLE nullable (id INT, vec VECTOR(3));
INSERT INTO nullable (id) VALUES (1);
SELECT COUNT(*) FROM nullable WHERE vec IS NULL;"

echo ""

# ===================================================================
# CREATE HNSW INDEX
# ===================================================================
echo -e "${YELLOW}═══ CREATE HNSW INDEX ═══${NC}"
echo ""

run_test "Create HNSW index on VECTOR(3)" "3.1" \
    "CREATE TABLE vec3 (id INT, v VECTOR(3));
CREATE INDEX idx_vec3 ON vec3(v) USING hnsw;
INSERT INTO vec3 VALUES (1, '[0.1, 0.2, 0.3]');
SELECT COUNT(*) FROM vec3;"

run_test "Create HNSW index on VECTOR(8)" "3.2" \
    "CREATE TABLE vec8 (id INT, v VECTOR(8));
CREATE INDEX idx_vec8 ON vec8(v) USING hnsw;
INSERT INTO vec8 VALUES (1, '[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8]');
SELECT COUNT(*) FROM vec8;"

run_test "Create HNSW index on VECTOR(128)" "3.3" \
    "CREATE TABLE vec128 (id INT, v VECTOR(128));
CREATE INDEX idx_vec128 ON vec128(v) USING hnsw;
\d vec128"

run_test "Multiple HNSW indexes" "3.4" \
    "CREATE TABLE multi (id INT, v1 VECTOR(4), v2 VECTOR(4));
CREATE INDEX idx_v1 ON multi(v1) USING hnsw;
CREATE INDEX idx_v2 ON multi(v2) USING hnsw;
\d multi"

echo ""

# ===================================================================
# COSINE SIMILARITY SEARCH
# ===================================================================
echo -e "${YELLOW}═══ COSINE SIMILARITY SEARCH (<=>) ═══${NC}"
echo ""

run_test "Cosine similarity basic" "4.1" \
    "CREATE TABLE vecs (id INT, v VECTOR(3));
INSERT INTO vecs VALUES (1, '[1.0, 0.0, 0.0]');
INSERT INTO vecs VALUES (2, '[0.707, 0.707, 0.0]');
INSERT INTO vecs VALUES (3, '[0.0, 1.0, 0.0]');
SELECT id, v <=> '[1.0, 0.0, 0.0]' AS distance FROM vecs ORDER BY distance;"

run_test "Cosine similarity with LIMIT" "4.2" \
    "CREATE TABLE docs (id INT, vec VECTOR(4));
INSERT INTO docs VALUES (1, '[1.0, 0.0, 0.0, 0.0]');
INSERT INTO docs VALUES (2, '[0.5, 0.5, 0.0, 0.0]');
INSERT INTO docs VALUES (3, '[0.0, 1.0, 0.0, 0.0]');
INSERT INTO docs VALUES (4, '[0.0, 0.0, 1.0, 0.0]');
SELECT id, vec <=> '[1.0, 0.0, 0.0, 0.0]' AS dist FROM docs ORDER BY dist LIMIT 2;"

run_test "Cosine similarity normalized vectors" "4.3" \
    "CREATE TABLE norm (id INT, v VECTOR(3));
INSERT INTO norm VALUES (1, '[0.577, 0.577, 0.577]');
INSERT INTO norm VALUES (2, '[1.0, 0.0, 0.0]');
SELECT id, v <=> '[0.577, 0.577, 0.577]' AS dist FROM norm ORDER BY dist;"

echo ""

# ===================================================================
# L2 DISTANCE SEARCH
# ===================================================================
echo -e "${YELLOW}═══ L2 DISTANCE SEARCH (<->) ═══${NC}"
echo ""

run_test "L2 distance basic" "5.1" \
    "CREATE TABLE points (id INT, vec VECTOR(2));
INSERT INTO points VALUES (1, '[1.0, 0.0]');
INSERT INTO points VALUES (2, '[0.0, 1.0]');
INSERT INTO points VALUES (3, '[1.0, 1.0]');
SELECT id, vec <-> '[0.0, 0.0]' AS distance FROM points ORDER BY distance;"

run_test "L2 distance with LIMIT" "5.2" \
    "CREATE TABLE coords (id INT, v VECTOR(3));
INSERT INTO coords VALUES (1, '[1.0, 0.0, 0.0]');
INSERT INTO coords VALUES (2, '[2.0, 0.0, 0.0]');
INSERT INTO coords VALUES (3, '[3.0, 0.0, 0.0]');
INSERT INTO coords VALUES (4, '[4.0, 0.0, 0.0]');
SELECT id, v <-> '[0.0, 0.0, 0.0]' AS dist FROM coords ORDER BY dist LIMIT 3;"

run_test "L2 distance nearest neighbor" "5.3" \
    "CREATE TABLE items (id INT, emb VECTOR(4));
INSERT INTO items VALUES (1, '[1.0, 2.0, 3.0, 4.0]');
INSERT INTO items VALUES (2, '[4.0, 5.0, 6.0, 7.0]');
INSERT INTO items VALUES (3, '[7.0, 8.0, 9.0, 10.0]');
SELECT id, emb <-> '[1.0, 2.0, 3.0, 4.0]' AS dist FROM items ORDER BY dist LIMIT 1;"

echo ""

# ===================================================================
# INNER PRODUCT SEARCH
# ===================================================================
echo -e "${YELLOW}═══ INNER PRODUCT SEARCH (<#>) ═══${NC}"
echo ""

run_test "Inner product basic" "6.1" \
    "CREATE TABLE vecs (id INT, v VECTOR(3));
INSERT INTO vecs VALUES (1, '[1.0, 2.0, 3.0]');
INSERT INTO vecs VALUES (2, '[4.0, 5.0, 6.0]');
SELECT id, v <#> '[1.0, 0.0, 0.0]' AS score FROM vecs ORDER BY score;"

run_test "Inner product with ORDER BY" "6.2" \
    "CREATE TABLE embeddings (id INT, vec VECTOR(4));
INSERT INTO embeddings VALUES (1, '[1.0, 0.0, 0.0, 0.0]');
INSERT INTO embeddings VALUES (2, '[0.0, 1.0, 0.0, 0.0]');
INSERT INTO embeddings VALUES (3, '[1.0, 1.0, 0.0, 0.0]');
SELECT id, vec <#> '[1.0, 1.0, 0.0, 0.0]' AS score FROM embeddings ORDER BY score LIMIT 2;"

echo ""

# ===================================================================
# KNN SEARCH
# ===================================================================
echo -e "${YELLOW}═══ KNN SEARCH ═══${NC}"
echo ""

run_test "KNN k=5 with L2 distance" "7.1" \
    "CREATE TABLE knn_test (id INT, vec VECTOR(4));
INSERT INTO knn_test VALUES (1, '[1.0, 0.0, 0.0, 0.0]');
INSERT INTO knn_test VALUES (2, '[0.9, 0.1, 0.0, 0.0]');
INSERT INTO knn_test VALUES (3, '[0.8, 0.2, 0.0, 0.0]');
INSERT INTO knn_test VALUES (4, '[0.7, 0.3, 0.0, 0.0]');
INSERT INTO knn_test VALUES (5, '[0.6, 0.4, 0.0, 0.0]');
INSERT INTO knn_test VALUES (6, '[0.0, 1.0, 0.0, 0.0]');
SELECT id, vec <-> '[1.0, 0.0, 0.0, 0.0]' AS dist FROM knn_test ORDER BY dist LIMIT 5;"

run_test "KNN k=3 with cosine similarity" "7.2" \
    "CREATE TABLE knn_cos (id INT, v VECTOR(3));
INSERT INTO knn_cos VALUES (1, '[1.0, 0.0, 0.0]');
INSERT INTO knn_cos VALUES (2, '[0.8, 0.2, 0.0]');
INSERT INTO knn_cos VALUES (3, '[0.6, 0.4, 0.0]');
INSERT INTO knn_cos VALUES (4, '[0.4, 0.6, 0.0]');
INSERT INTO knn_cos VALUES (5, '[0.0, 1.0, 0.0]');
SELECT id, v <=> '[1.0, 0.0, 0.0]' AS dist FROM knn_cos ORDER BY dist LIMIT 3;"

run_test "KNN k=10 with HNSW index" "7.3" \
    "CREATE TABLE knn_hnsw (id INT, vec VECTOR(8));
CREATE INDEX idx_knn ON knn_hnsw(vec) USING hnsw;
INSERT INTO knn_hnsw VALUES (1, '[0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1]');
INSERT INTO knn_hnsw VALUES (2, '[0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2]');
INSERT INTO knn_hnsw VALUES (3, '[0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3]');
INSERT INTO knn_hnsw VALUES (4, '[0.4,0.4,0.4,0.4,0.4,0.4,0.4,0.4]');
INSERT INTO knn_hnsw VALUES (5, '[0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5]');
INSERT INTO knn_hnsw VALUES (6, '[0.6,0.6,0.6,0.6,0.6,0.6,0.6,0.6]');
INSERT INTO knn_hnsw VALUES (7, '[0.7,0.7,0.7,0.7,0.7,0.7,0.7,0.7]');
INSERT INTO knn_hnsw VALUES (8, '[0.8,0.8,0.8,0.8,0.8,0.8,0.8,0.8]');
INSERT INTO knn_hnsw VALUES (9, '[0.9,0.9,0.9,0.9,0.9,0.9,0.9,0.9]');
INSERT INTO knn_hnsw VALUES (10, '[1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0]');
SELECT id, vec <-> '[0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5]' AS dist FROM knn_hnsw ORDER BY dist LIMIT 10;"

echo ""

# ===================================================================
# VECTOR INDEX STATS
# ===================================================================
echo -e "${YELLOW}═══ VECTOR INDEX STATS (pg_vector_index_stats) ═══${NC}"
echo ""

run_test "Query pg_vector_index_stats empty" "8.1" \
    "SELECT * FROM pg_vector_index_stats();"

run_test "Vector index stats after index creation" "8.2" \
    "CREATE TABLE stats_test (id INT, v VECTOR(4));
CREATE INDEX idx_stats ON stats_test(v) USING hnsw;
SELECT * FROM pg_vector_index_stats();"

run_test "Index stats with data" "8.3" \
    "CREATE TABLE indexed (id INT, vec VECTOR(8));
CREATE INDEX idx_indexed ON indexed(vec) USING hnsw;
INSERT INTO indexed VALUES (1, '[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8]');
INSERT INTO indexed VALUES (2, '[0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9]');
SELECT index_name, dimensions FROM pg_vector_index_stats() WHERE index_name = 'idx_indexed';"

run_test "Multiple index stats" "8.4" \
    "CREATE TABLE multi_idx (id INT, v1 VECTOR(4), v2 VECTOR(8));
CREATE INDEX idx_v1_stats ON multi_idx(v1) USING hnsw;
CREATE INDEX idx_v2_stats ON multi_idx(v2) USING hnsw;
SELECT index_name, dimensions FROM pg_vector_index_stats();"

echo ""

# ===================================================================
# BATCH VECTOR INSERT
# ===================================================================
echo -e "${YELLOW}═══ BATCH VECTOR INSERT ═══${NC}"
echo ""

run_test "Batch insert 10 vectors" "9.1" \
    "CREATE TABLE batch10 (id INT, vec VECTOR(3));
INSERT INTO batch10 VALUES (1, '[0.1, 0.1, 0.1]');
INSERT INTO batch10 VALUES (2, '[0.2, 0.2, 0.2]');
INSERT INTO batch10 VALUES (3, '[0.3, 0.3, 0.3]');
INSERT INTO batch10 VALUES (4, '[0.4, 0.4, 0.4]');
INSERT INTO batch10 VALUES (5, '[0.5, 0.5, 0.5]');
INSERT INTO batch10 VALUES (6, '[0.6, 0.6, 0.6]');
INSERT INTO batch10 VALUES (7, '[0.7, 0.7, 0.7]');
INSERT INTO batch10 VALUES (8, '[0.8, 0.8, 0.8]');
INSERT INTO batch10 VALUES (9, '[0.9, 0.9, 0.9]');
INSERT INTO batch10 VALUES (10, '[1.0, 1.0, 1.0]');
SELECT COUNT(*) FROM batch10;"

run_test "Batch insert with HNSW index" "9.2" \
    "CREATE TABLE batch_indexed (id INT, v VECTOR(4));
CREATE INDEX idx_batch ON batch_indexed(v) USING hnsw;
INSERT INTO batch_indexed VALUES (1, '[1.0, 0.0, 0.0, 0.0]');
INSERT INTO batch_indexed VALUES (2, '[0.0, 1.0, 0.0, 0.0]');
INSERT INTO batch_indexed VALUES (3, '[0.0, 0.0, 1.0, 0.0]');
INSERT INTO batch_indexed VALUES (4, '[0.0, 0.0, 0.0, 1.0]');
INSERT INTO batch_indexed VALUES (5, '[0.5, 0.5, 0.0, 0.0]');
SELECT COUNT(*) FROM batch_indexed;"

run_test "Batch insert 20 vectors" "9.3" \
    "CREATE TABLE batch20 (id INT, vec VECTOR(4));
INSERT INTO batch20 VALUES (1, '[0.1,0.1,0.1,0.1]');
INSERT INTO batch20 VALUES (2, '[0.2,0.2,0.2,0.2]');
INSERT INTO batch20 VALUES (3, '[0.3,0.3,0.3,0.3]');
INSERT INTO batch20 VALUES (4, '[0.4,0.4,0.4,0.4]');
INSERT INTO batch20 VALUES (5, '[0.5,0.5,0.5,0.5]');
INSERT INTO batch20 VALUES (6, '[0.6,0.6,0.6,0.6]');
INSERT INTO batch20 VALUES (7, '[0.7,0.7,0.7,0.7]');
INSERT INTO batch20 VALUES (8, '[0.8,0.8,0.8,0.8]');
INSERT INTO batch20 VALUES (9, '[0.9,0.9,0.9,0.9]');
INSERT INTO batch20 VALUES (10, '[1.0,1.0,1.0,1.0]');
INSERT INTO batch20 VALUES (11, '[1.1,1.1,1.1,1.1]');
INSERT INTO batch20 VALUES (12, '[1.2,1.2,1.2,1.2]');
INSERT INTO batch20 VALUES (13, '[1.3,1.3,1.3,1.3]');
INSERT INTO batch20 VALUES (14, '[1.4,1.4,1.4,1.4]');
INSERT INTO batch20 VALUES (15, '[1.5,1.5,1.5,1.5]');
INSERT INTO batch20 VALUES (16, '[1.6,1.6,1.6,1.6]');
INSERT INTO batch20 VALUES (17, '[1.7,1.7,1.7,1.7]');
INSERT INTO batch20 VALUES (18, '[1.8,1.8,1.8,1.8]');
INSERT INTO batch20 VALUES (19, '[1.9,1.9,1.9,1.9]');
INSERT INTO batch20 VALUES (20, '[2.0,2.0,2.0,2.0]');
SELECT COUNT(*) FROM batch20;"

echo ""

# ===================================================================
# VECTOR SEARCH WITH FILTER
# ===================================================================
echo -e "${YELLOW}═══ VECTOR SEARCH WITH FILTER ═══${NC}"
echo ""

run_test "Vector search with WHERE clause" "10.1" \
    "CREATE TABLE filtered (id INT, category TEXT, vec VECTOR(3));
INSERT INTO filtered VALUES (1, 'A', '[1.0, 0.0, 0.0]');
INSERT INTO filtered VALUES (2, 'B', '[0.0, 1.0, 0.0]');
INSERT INTO filtered VALUES (3, 'A', '[0.0, 0.0, 1.0]');
INSERT INTO filtered VALUES (4, 'A', '[0.5, 0.5, 0.0]');
SELECT id, vec <-> '[1.0, 0.0, 0.0]' AS dist FROM filtered WHERE category = 'A' ORDER BY dist LIMIT 2;"

run_test "Vector search with numeric filter" "10.2" \
    "CREATE TABLE price_filter (id INT, price INT, emb VECTOR(4));
INSERT INTO price_filter VALUES (1, 100, '[1.0, 0.0, 0.0, 0.0]');
INSERT INTO price_filter VALUES (2, 200, '[0.8, 0.2, 0.0, 0.0]');
INSERT INTO price_filter VALUES (3, 50, '[0.6, 0.4, 0.0, 0.0]');
INSERT INTO price_filter VALUES (4, 300, '[0.4, 0.6, 0.0, 0.0]');
SELECT id, price, emb <-> '[1.0, 0.0, 0.0, 0.0]' AS dist FROM price_filter WHERE price < 250 ORDER BY dist LIMIT 2;"

run_test "Vector search with multiple filters" "10.3" \
    "CREATE TABLE multi_filter (id INT, category TEXT, rating INT, vec VECTOR(3));
INSERT INTO multi_filter VALUES (1, 'X', 5, '[1.0, 0.0, 0.0]');
INSERT INTO multi_filter VALUES (2, 'Y', 4, '[0.9, 0.1, 0.0]');
INSERT INTO multi_filter VALUES (3, 'X', 3, '[0.8, 0.2, 0.0]');
INSERT INTO multi_filter VALUES (4, 'X', 5, '[0.7, 0.3, 0.0]');
SELECT id, vec <=> '[1.0, 0.0, 0.0]' AS dist FROM multi_filter WHERE category = 'X' AND rating >= 4 ORDER BY dist;"

echo ""

# ===================================================================
# PRODUCT QUANTIZATION INDEX
# ===================================================================
echo -e "${YELLOW}═══ PRODUCT QUANTIZATION (PQ) INDEX ═══${NC}"
echo ""

run_test "Create HNSW with PQ quantization" "11.1" \
    "CREATE TABLE pq_test (id INT, vec VECTOR(16));
CREATE INDEX idx_pq ON pq_test(vec) USING hnsw WITH (quantization='product');
INSERT INTO pq_test VALUES (1, '[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,1.0,1.1,1.2,1.3,1.4,1.5,1.6]');
SELECT COUNT(*) FROM pq_test;"

run_test "PQ with custom subquantizers" "11.2" \
    "CREATE TABLE pq_custom (id INT, v VECTOR(32));
CREATE INDEX idx_pq_custom ON pq_custom(v) USING hnsw WITH (quantization='product', pq_subquantizers=8);
\d pq_custom"

run_test "PQ with custom centroids" "11.3" \
    "CREATE TABLE pq_centroids (id INT, vec VECTOR(64));
CREATE INDEX idx_pq_cent ON pq_centroids(vec) USING hnsw WITH (quantization='product', pq_centroids=256);
\d pq_centroids"

run_test "PQ index stats" "11.4" \
    "CREATE TABLE pq_stats (id INT, v VECTOR(16));
CREATE INDEX idx_pq_stats ON pq_stats(v) USING hnsw WITH (quantization='product');
SELECT * FROM pg_vector_index_stats();"

run_test "PQ search with L2 distance" "11.5" \
    "CREATE TABLE pq_search (id INT, vec VECTOR(16));
CREATE INDEX idx_pq_search ON pq_search(vec) USING hnsw WITH (quantization='product', pq_subquantizers=8);
INSERT INTO pq_search VALUES (1, '[0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1]');
INSERT INTO pq_search VALUES (2, '[0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2,0.2]');
INSERT INTO pq_search VALUES (3, '[0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3,0.3]');
SELECT id, vec <-> '[0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1]' AS dist FROM pq_search ORDER BY dist LIMIT 2;"

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

# Cleanup
rm -f "${TEST_DB}"*

if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}✓ All Vector Search tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
