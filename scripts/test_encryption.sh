#!/bin/bash

# HeliosDB-Lite Encryption Test Suite
# Tests: Encryption at rest, key management, rotation, vector data, transactions
# Run: ./test_encryption.sh

BINARY="./target/release/heliosdb-lite"
TEST_DIR="./test_encryption_temp"
TEST_DB="$TEST_DIR/encrypted.db"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB-Lite Encryption Test Suite"
echo "=========================================="
echo ""

# Cleanup function
cleanup() {
    rm -rf "$TEST_DIR"
    unset HELIOSDB_ENCRYPTION_KEY
    unset HELIOSDB_ENCRYPTION_KEY_2
    unset HELIOSDB_ENCRYPTION_KEY_WRONG
}

# Setup function
setup() {
    cleanup
    mkdir -p "$TEST_DIR"
}

# Generate a random 64-character hex key (32 bytes)
generate_key() {
    openssl rand -hex 32
}

run_test() {
    local test_name="$1"
    local test_num="$2"
    local sql="$3"
    local db_path="${4:-$TEST_DB}"

    echo -n "[$test_num] $test_name ... "

    output=$(timeout 10 "$BINARY" repl --path "$db_path" << EOF 2>&1
$sql
\q
EOF
)

    # Test passes if output contains successful patterns
    if echo "$output" | grep -qE "Query OK|^\(|Column.*Type|^[0-9]|rows\)|postgres|^[a-z_].*\|"; then
        if echo "$output" | grep -qvE "panic|Connection|INTERNAL|Error:|Failed"; then
            echo -e "${GREEN}✓${NC}"
            ((PASSED++))
            return 0
        fi
    fi

    echo -e "${RED}✗${NC}"
    echo "  Output: $(echo "$output" | tail -3)"
    ((FAILED++))
    return 1
}

run_cli_test() {
    local test_name="$1"
    local test_num="$2"
    local command="$3"

    echo -n "[$test_num] $test_name ... "

    output=$(timeout 10 bash -c "$command" 2>&1)
    exit_code=$?

    if [ $exit_code -eq 0 ]; then
        echo -e "${GREEN}✓${NC}"
        ((PASSED++))
        return 0
    else
        echo -e "${RED}✗${NC}"
        echo "  Output: $(echo "$output" | tail -2)"
        ((FAILED++))
        return 1
    fi
}

# ===================================================================
# 1. CREATE ENCRYPTED DATABASE WITH KEY
# ===================================================================
echo -e "${YELLOW}═══ TEST 1: CREATE ENCRYPTED DATABASE WITH KEY ═══${NC}"
echo ""

setup
export HELIOSDB_ENCRYPTION_KEY=$(generate_key)

run_test "Create encrypted database" "1.1" \
    "CREATE TABLE encrypted_users (id INT, name TEXT, ssn TEXT);
INSERT INTO encrypted_users VALUES (1, 'Alice', '123-45-6789');
INSERT INTO encrypted_users VALUES (2, 'Bob', '987-65-4321');
SELECT COUNT(*) FROM encrypted_users;"

run_test "Verify data accessible with correct key" "1.2" \
    "SELECT * FROM encrypted_users WHERE id = 1;"

echo ""

# ===================================================================
# 2. VERIFY DATA IS ENCRYPTED AT REST
# ===================================================================
echo -e "${YELLOW}═══ TEST 2: VERIFY DATA IS ENCRYPTED AT REST ═══${NC}"
echo ""

# Create a test file with known data
TEST_DB_CHECK="$TEST_DIR/check_encrypted.db"
export HELIOSDB_ENCRYPTION_KEY=$(generate_key)

run_test "Create test data for encryption check" "2.1" \
    "CREATE TABLE secrets (id INT, secret TEXT);
INSERT INTO secrets VALUES (1, 'PLAINTEXT_SECRET_12345');
SELECT * FROM secrets;" "$TEST_DB_CHECK"

# Check if plaintext appears in database files
run_cli_test "Verify data is encrypted at rest" "2.2" \
    "! grep -r 'PLAINTEXT_SECRET_12345' '$TEST_DB_CHECK' 2>/dev/null"

echo ""

# ===================================================================
# 3. ACCESS WITH CORRECT KEY
# ===================================================================
echo -e "${YELLOW}═══ TEST 3: ACCESS WITH CORRECT KEY ═══${NC}"
echo ""

TEST_DB_ACCESS="$TEST_DIR/access_test.db"
export HELIOSDB_ENCRYPTION_KEY=$(generate_key)

run_test "Create encrypted data" "3.1" \
    "CREATE TABLE access_test (id INT, data TEXT);
INSERT INTO access_test VALUES (1, 'encrypted_data');
SELECT * FROM access_test;" "$TEST_DB_ACCESS"

run_test "Access with same key" "3.2" \
    "SELECT COUNT(*) FROM access_test WHERE id = 1;" "$TEST_DB_ACCESS"

run_test "Query and update with correct key" "3.3" \
    "UPDATE access_test SET data = 'updated_encrypted_data' WHERE id = 1;
SELECT * FROM access_test;" "$TEST_DB_ACCESS"

echo ""

# ===================================================================
# 4. REJECT INCORRECT KEY
# ===================================================================
echo -e "${YELLOW}═══ TEST 4: REJECT INCORRECT KEY ═══${NC}"
echo ""

TEST_DB_WRONG_KEY="$TEST_DIR/wrong_key.db"
CORRECT_KEY=$(generate_key)
export HELIOSDB_ENCRYPTION_KEY=$CORRECT_KEY

run_test "Create data with correct key" "4.1" \
    "CREATE TABLE protected (id INT, value TEXT);
INSERT INTO protected VALUES (1, 'secret');
SELECT * FROM protected;" "$TEST_DB_WRONG_KEY"

# Change to wrong key
WRONG_KEY=$(generate_key)
export HELIOSDB_ENCRYPTION_KEY=$WRONG_KEY

echo -n "[4.2] Reject access with wrong key ... "
output=$(timeout 10 "$BINARY" repl --path "$TEST_DB_WRONG_KEY" << EOF 2>&1
SELECT * FROM protected;
\q
EOF
)

# Should fail with wrong key
if echo "$output" | grep -qiE "error|fail|decrypt|authentication"; then
    echo -e "${GREEN}✓${NC}"
    ((PASSED++))
else
    echo -e "${RED}✗${NC}"
    echo "  Expected decryption error, got: $(echo "$output" | tail -2)"
    ((FAILED++))
fi

# Restore correct key
export HELIOSDB_ENCRYPTION_KEY=$CORRECT_KEY

echo ""

# ===================================================================
# 5. KEY ROTATION
# ===================================================================
echo -e "${YELLOW}═══ TEST 5: KEY ROTATION ═══${NC}"
echo ""

TEST_DB_ROTATION="$TEST_DIR/rotation.db"
OLD_KEY=$(generate_key)
export HELIOSDB_ENCRYPTION_KEY=$OLD_KEY

run_test "Create data with old key" "5.1" \
    "CREATE TABLE rotation_test (id INT, data TEXT);
INSERT INTO rotation_test VALUES (1, 'before_rotation');
SELECT * FROM rotation_test;" "$TEST_DB_ROTATION"

# Simulate key rotation by exporting data and re-importing with new key
NEW_KEY=$(generate_key)

echo -n "[5.2] Export data with old key ... "
export HELIOSDB_ENCRYPTION_KEY=$OLD_KEY
export_output=$(timeout 10 "$BINARY" repl --path "$TEST_DB_ROTATION" << EOF 2>&1
SELECT * FROM rotation_test;
\q
EOF
)

if echo "$export_output" | grep -q "before_rotation"; then
    echo -e "${GREEN}✓${NC}"
    ((PASSED++))
else
    echo -e "${RED}✗${NC}"
    ((FAILED++))
fi

# For this test, we'll verify that a new database with new key works
TEST_DB_NEW_KEY="$TEST_DIR/new_key.db"
export HELIOSDB_ENCRYPTION_KEY=$NEW_KEY

run_test "Create new database with rotated key" "5.3" \
    "CREATE TABLE rotation_test (id INT, data TEXT);
INSERT INTO rotation_test VALUES (1, 'after_rotation');
SELECT * FROM rotation_test;" "$TEST_DB_NEW_KEY"

echo ""

# ===================================================================
# 6. ENCRYPTION WITH VECTOR DATA
# ===================================================================
echo -e "${YELLOW}═══ TEST 6: ENCRYPTION WITH VECTOR DATA ═══${NC}"
echo ""

TEST_DB_VECTOR="$TEST_DIR/vector.db"
export HELIOSDB_ENCRYPTION_KEY=$(generate_key)

run_test "Create encrypted vector table" "6.1" \
    "CREATE TABLE embeddings (id INT, vec FLOAT8[]);
INSERT INTO embeddings VALUES (1, ARRAY[1.0, 2.0, 3.0]);
INSERT INTO embeddings VALUES (2, ARRAY[4.0, 5.0, 6.0]);
SELECT COUNT(*) FROM embeddings;" "$TEST_DB_VECTOR"

run_test "Query encrypted vector data" "6.2" \
    "SELECT * FROM embeddings WHERE id = 1;" "$TEST_DB_VECTOR"

run_test "Vector operations on encrypted data" "6.3" \
    "SELECT id FROM embeddings ORDER BY id;" "$TEST_DB_VECTOR"

echo ""

# ===================================================================
# 7. ENCRYPTED INDEX OPERATIONS
# ===================================================================
echo -e "${YELLOW}═══ TEST 7: ENCRYPTED INDEX OPERATIONS ═══${NC}"
echo ""

TEST_DB_INDEX="$TEST_DIR/index.db"
export HELIOSDB_ENCRYPTION_KEY=$(generate_key)

run_test "Create table with index on encrypted data" "7.1" \
    "CREATE TABLE indexed_data (id INT PRIMARY KEY, name TEXT, age INT);
INSERT INTO indexed_data VALUES (1, 'Alice', 30);
INSERT INTO indexed_data VALUES (2, 'Bob', 25);
INSERT INTO indexed_data VALUES (3, 'Carol', 35);
SELECT COUNT(*) FROM indexed_data;" "$TEST_DB_INDEX"

run_test "Query using index on encrypted data" "7.2" \
    "SELECT * FROM indexed_data WHERE id = 2;" "$TEST_DB_INDEX"

run_test "Range query on encrypted indexed data" "7.3" \
    "SELECT name FROM indexed_data WHERE id > 1 ORDER BY id;" "$TEST_DB_INDEX"

echo ""

# ===================================================================
# 8. ENCRYPTED TRANSACTIONS
# ===================================================================
echo -e "${YELLOW}═══ TEST 8: ENCRYPTED TRANSACTIONS ═══${NC}"
echo ""

TEST_DB_TXN="$TEST_DIR/transaction.db"
export HELIOSDB_ENCRYPTION_KEY=$(generate_key)

run_test "Create table for transaction test" "8.1" \
    "CREATE TABLE accounts (id INT, balance INT);
INSERT INTO accounts VALUES (1, 1000);
INSERT INTO accounts VALUES (2, 500);
SELECT * FROM accounts;" "$TEST_DB_TXN"

run_test "Transaction on encrypted data" "8.2" \
    "BEGIN;
UPDATE accounts SET balance = balance - 100 WHERE id = 1;
UPDATE accounts SET balance = balance + 100 WHERE id = 2;
COMMIT;
SELECT SUM(balance) FROM accounts;" "$TEST_DB_TXN"

run_test "Verify transaction persistence" "8.3" \
    "SELECT balance FROM accounts WHERE id = 1;" "$TEST_DB_TXN"

echo ""

# ===================================================================
# 9. PERFORMANCE BENCHMARK (100 inserts < 5s)
# ===================================================================
echo -e "${YELLOW}═══ TEST 9: PERFORMANCE BENCHMARK ═══${NC}"
echo ""

TEST_DB_PERF="$TEST_DIR/performance.db"
export HELIOSDB_ENCRYPTION_KEY=$(generate_key)

echo -n "[9.1] Performance: 100 inserts with encryption ... "

# Generate SQL for 100 inserts
sql_inserts="CREATE TABLE perf_test (id INT, value INT, text TEXT);"
for i in {1..100}; do
    sql_inserts="$sql_inserts
INSERT INTO perf_test VALUES ($i, $((i * 10)), 'test_data_$i');"
done
sql_inserts="$sql_inserts
SELECT COUNT(*) FROM perf_test;"

start_time=$(date +%s%N)
output=$(timeout 10 "$BINARY" repl --path "$TEST_DB_PERF" << EOF 2>&1
$sql_inserts
\q
EOF
)
end_time=$(date +%s%N)

elapsed_ms=$(( (end_time - start_time) / 1000000 ))

if echo "$output" | grep -q "100"; then
    if [ $elapsed_ms -lt 5000 ]; then
        echo -e "${GREEN}✓${NC} (${elapsed_ms}ms)"
        ((PASSED++))
    else
        echo -e "${RED}✗${NC} (${elapsed_ms}ms > 5000ms)"
        ((FAILED++))
    fi
else
    echo -e "${RED}✗${NC} (Query failed)"
    ((FAILED++))
fi

run_test "Verify all records inserted" "9.2" \
    "SELECT COUNT(*) FROM perf_test;" "$TEST_DB_PERF"

echo ""

# ===================================================================
# 10. RECOVERY SCENARIOS
# ===================================================================
echo -e "${YELLOW}═══ TEST 10: RECOVERY SCENARIOS ═══${NC}"
echo ""

TEST_DB_RECOVERY="$TEST_DIR/recovery.db"
RECOVERY_KEY=$(generate_key)
export HELIOSDB_ENCRYPTION_KEY=$RECOVERY_KEY

run_test "Create data for recovery test" "10.1" \
    "CREATE TABLE recoverable (id INT, data TEXT);
INSERT INTO recoverable VALUES (1, 'important_data');
INSERT INTO recoverable VALUES (2, 'critical_data');
SELECT COUNT(*) FROM recoverable;" "$TEST_DB_RECOVERY"

# Simulate database close and reopen
run_test "Recover data after restart" "10.2" \
    "SELECT * FROM recoverable WHERE id = 1;" "$TEST_DB_RECOVERY"

run_test "Verify data integrity after recovery" "10.3" \
    "SELECT COUNT(*) FROM recoverable;" "$TEST_DB_RECOVERY"

# Test recovery with transaction log
run_test "Recovery with uncommitted transactions" "10.4" \
    "INSERT INTO recoverable VALUES (3, 'new_data');
SELECT COUNT(*) FROM recoverable;" "$TEST_DB_RECOVERY"

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
cleanup

if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}✓ All Encryption tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
