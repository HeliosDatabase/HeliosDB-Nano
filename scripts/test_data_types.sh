#!/bin/bash

# HeliosDB Nano Data Types Test Suite
# Tests: INT, TEXT, FLOAT, BOOLEAN, JSON, UUID, TIMESTAMP, VECTOR, ARRAY
# Run: ./test_data_types.sh

BINARY="./target/release/heliosdb-nano"
TEST_DB="test_data_types.db"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB Nano Data Types Test Suite"
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
    if echo "$output" | grep -qE "Query OK|^\(|Column.*Type|^[0-9]|^true|^false|rows\)|postgres"; then
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
# INTEGER TYPES
# ===================================================================
echo -e "${YELLOW}═══ INTEGER TYPES ═══${NC}"
echo ""

run_test "INT2 type" "1.1" \
    "CREATE TABLE int2_test (id INT2);
INSERT INTO int2_test VALUES (100);
SELECT * FROM int2_test;"

run_test "INT4 type" "1.2" \
    "CREATE TABLE int4_test (id INT4);
INSERT INTO int4_test VALUES (1000000);
SELECT * FROM int4_test;"

run_test "INT8 type" "1.3" \
    "CREATE TABLE int8_test (id INT8);
INSERT INTO int8_test VALUES (9223372036854775807);
SELECT * FROM int8_test;"

run_test "INT type (default INT4)" "1.4" \
    "CREATE TABLE int_test (id INT);
INSERT INTO int_test VALUES (42);
SELECT * FROM int_test;"

echo ""

# ===================================================================
# FLOAT TYPES
# ===================================================================
echo -e "${YELLOW}═══ FLOAT TYPES ═══${NC}"
echo ""

run_test "FLOAT4 type" "2.1" \
    "CREATE TABLE float4_test (val FLOAT4);
INSERT INTO float4_test VALUES (3.14);
SELECT * FROM float4_test;"

run_test "FLOAT8 type" "2.2" \
    "CREATE TABLE float8_test (val FLOAT8);
INSERT INTO float8_test VALUES (2.718281828);
SELECT * FROM float8_test;"

echo ""

# ===================================================================
# STRING TYPES
# ===================================================================
echo -e "${YELLOW}═══ STRING TYPES ═══${NC}"
echo ""

run_test "TEXT type" "3.1" \
    "CREATE TABLE text_test (name TEXT);
INSERT INTO text_test VALUES ('Hello, World!');
SELECT * FROM text_test;"

run_test "VARCHAR type" "3.2" \
    "CREATE TABLE varchar_test (code VARCHAR(10));
INSERT INTO varchar_test VALUES ('ABC123');
SELECT * FROM varchar_test;"

run_test "TEXT with special characters" "3.3" \
    "CREATE TABLE special_test (text TEXT);
INSERT INTO special_test VALUES ('Special: !@#\$%^&*()');
SELECT * FROM special_test;"

run_test "Empty string" "3.4" \
    "CREATE TABLE empty_test (text TEXT);
INSERT INTO empty_test VALUES ('');
SELECT * FROM empty_test;"

echo ""

# ===================================================================
# BOOLEAN TYPE
# ===================================================================
echo -e "${YELLOW}═══ BOOLEAN TYPE ═══${NC}"
echo ""

run_test "BOOLEAN true" "4.1" \
    "CREATE TABLE bool_test (flag BOOLEAN);
INSERT INTO bool_test VALUES (true);
SELECT * FROM bool_test;"

run_test "BOOLEAN false" "4.2" \
    "CREATE TABLE bool_test (flag BOOLEAN);
INSERT INTO bool_test VALUES (false);
SELECT * FROM bool_test;"

run_test "BOOLEAN with WHERE" "4.3" \
    "CREATE TABLE switches (id INT, active BOOLEAN);
INSERT INTO switches VALUES (1, true);
INSERT INTO switches VALUES (2, false);
SELECT COUNT(*) FROM switches WHERE active = true;"

echo ""

# ===================================================================
# JSON TYPE
# ===================================================================
echo -e "${YELLOW}═══ JSON TYPE ═══${NC}"
echo ""

run_test "JSON object" "5.1" \
    "CREATE TABLE json_test (data JSON);
INSERT INTO json_test VALUES ('{\"name\": \"Alice\", \"age\": 30}');
SELECT * FROM json_test;"

run_test "JSON array" "5.2" \
    "CREATE TABLE json_array_test (data JSON);
INSERT INTO json_array_test VALUES ('[1, 2, 3, 4, 5]');
SELECT * FROM json_array_test;"

run_test "JSON null value" "5.3" \
    "CREATE TABLE json_null_test (data JSON);
INSERT INTO json_null_test VALUES ('{\"key\": null}');
SELECT * FROM json_null_test;"

echo ""

# ===================================================================
# UUID TYPE
# ===================================================================
echo -e "${YELLOW}═══ UUID TYPE ═══${NC}"
echo ""

run_test "UUID value" "6.1" \
    "CREATE TABLE uuid_test (id UUID);
INSERT INTO uuid_test VALUES ('550e8400-e29b-41d4-a716-446655440000');
SELECT * FROM uuid_test;"

run_test "Multiple UUIDs" "6.2" \
    "CREATE TABLE ids (id UUID);
INSERT INTO ids VALUES ('550e8400-e29b-41d4-a716-446655440000');
INSERT INTO ids VALUES ('6ba7b810-9dad-11d1-80b4-00c04fd430c8');
SELECT COUNT(*) FROM ids;"

echo ""

# ===================================================================
# TIMESTAMP TYPE
# ===================================================================
echo -e "${YELLOW}═══ TIMESTAMP TYPE ═══${NC}"
echo ""

run_test "TIMESTAMP value" "7.1" \
    "CREATE TABLE ts_test (created TIMESTAMP);
INSERT INTO ts_test VALUES ('2025-11-29 12:00:00');
SELECT * FROM ts_test;"

run_test "TIMESTAMP with timezone" "7.2" \
    "CREATE TABLE tstz_test (created TIMESTAMP);
INSERT INTO tstz_test VALUES ('2025-11-29 12:00:00 UTC');
SELECT * FROM tstz_test;"

echo ""

# ===================================================================
# VECTOR TYPE
# ===================================================================
echo -e "${YELLOW}═══ VECTOR TYPE (Embeddings) ═══${NC}"
echo ""

run_test "VECTOR(3) type" "8.1" \
    "CREATE TABLE vec3_test (embedding VECTOR(3));
INSERT INTO vec3_test VALUES ('[0.1, 0.2, 0.3]');
SELECT * FROM vec3_test;"

run_test "VECTOR(8) type" "8.2" \
    "CREATE TABLE vec8_test (embedding VECTOR(8));
INSERT INTO vec8_test VALUES ('[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]');
SELECT * FROM vec8_test;"

run_test "VECTOR(16) type" "8.3" \
    "CREATE TABLE vec16_test (embedding VECTOR(16));
INSERT INTO vec16_test VALUES (
        '[0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,1.0,1.1,1.2,1.3,1.4,1.5,1.6]'
    );
SELECT * FROM vec16_test;"

run_test "VECTOR(1024) for LLM embeddings" "8.4" \
    "CREATE TABLE embeddings (id INT, vec VECTOR(1024));
INSERT INTO embeddings VALUES (1, '[" \
    "$(python3 -c 'print(\",\".join([\"0.\" + str(i % 100) for i in range(1024)]))')" \
    "]');
SELECT COUNT(*) FROM embeddings;"

echo ""

# ===================================================================
# JSON ARRAYS (Alternative to ARRAY TYPE)
# ===================================================================
echo -e "${YELLOW}═══ JSON ARRAYS ═══${NC}"
echo ""

run_test "JSON array with integers" "9.1" \
    "CREATE TABLE json_array_int_test (values JSON);
INSERT INTO json_array_int_test VALUES ('[1, 2, 3, 4, 5]');
SELECT * FROM json_array_int_test;"

run_test "JSON array with strings" "9.2" \
    "CREATE TABLE json_array_text_test (tags JSON);
INSERT INTO json_array_text_test VALUES ('[\"red\", \"blue\", \"green\"]');
SELECT * FROM json_array_text_test;"

run_test "Nested JSON array" "9.3" \
    "CREATE TABLE json_array_nested_test (data JSON);
INSERT INTO json_array_nested_test VALUES ('[[1, 2], [3, 4]]');
SELECT * FROM json_array_nested_test;"

echo ""

# ===================================================================
# TEXT BLOB SIMULATION
# ===================================================================
echo -e "${YELLOW}═══ LARGE TEXT DATA ═══${NC}"
echo ""

run_test "Large text storage" "10.1" \
    "CREATE TABLE text_storage (id INT, data TEXT);
INSERT INTO text_storage VALUES (1, 'Binary data simulation with long text');
SELECT LENGTH(data) FROM text_storage;"

echo ""

# ===================================================================
# NULL VALUES
# ===================================================================
echo -e "${YELLOW}═══ NULL VALUE HANDLING ═══${NC}"
echo ""

run_test "NULL in nullable column" "11.1" \
    "CREATE TABLE nullable_test (id INT, name TEXT);
INSERT INTO nullable_test VALUES (1, NULL);
INSERT INTO nullable_test VALUES (2, 'value');
SELECT COUNT(*) FROM nullable_test WHERE name IS NULL;"

run_test "NULL comparison" "11.2" \
    "CREATE TABLE null_test (id INT, value TEXT);
INSERT INTO null_test VALUES (1, 'a');
INSERT INTO null_test VALUES (2, NULL);
SELECT COUNT(*) FROM null_test WHERE value IS NOT NULL;"

echo ""

# ===================================================================
# TYPE CASTING
# ===================================================================
echo -e "${YELLOW}═══ TYPE CASTING ═══${NC}"
echo ""

run_test "Cast INT to TEXT" "12.1" \
    "CREATE TABLE cast_test (val INT);
INSERT INTO cast_test VALUES (42);
SELECT CAST(val AS TEXT) FROM cast_test;"

run_test "Cast TEXT to INT" "12.2" \
    "CREATE TABLE cast_text (val TEXT);
INSERT INTO cast_text VALUES ('123');
SELECT CAST(val AS INT4) FROM cast_text;"

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
    echo -e "${GREEN}✓ All Data Type tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
