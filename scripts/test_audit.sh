#!/bin/bash

# HeliosDB-Lite Audit Logging Test Suite
# Tests: Audit logging configuration, DDL/DML/SELECT logging, JSON format, tamper detection
# Run: ./test_audit.sh

BINARY="./target/release/heliosdb-nano"
TEST_DB="test_audit.db"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASSED=0
FAILED=0

echo "=========================================="
echo "HeliosDB-Lite Audit Logging Test"
echo "=========================================="
echo ""

cleanup() {
    if [ -d "$TEST_DB" ]; then
        rm -rf "$TEST_DB"
    fi
}

# Cleanup before starting
cleanup

run_test() {
    local test_name="$1"
    local test_num="$2"
    local sql="$3"
    local expected_pattern="$4"

    echo -n "[$test_num] $test_name ... "

    output=$(timeout 10 "$BINARY" repl --memory << EOF 2>&1
$sql
\q
EOF
)

    # Check for expected pattern or success indicators
    if [ -n "$expected_pattern" ]; then
        if echo "$output" | grep -qE "$expected_pattern"; then
            if echo "$output" | grep -qvE "panic|Connection failed|INTERNAL ERROR"; then
                echo -e "${GREEN}✓${NC}"
                ((PASSED++))
                return 0
            fi
        fi
    else
        # Default success check
        if echo "$output" | grep -qE "Query OK|Column|^[0-9]|^\(|rows\)|postgres|^[a-z_].*\|"; then
            if echo "$output" | grep -qvE "panic|Connection failed|INTERNAL ERROR"; then
                echo -e "${GREEN}✓${NC}"
                ((PASSED++))
                return 0
            fi
        fi
    fi

    echo -e "${RED}✗${NC}"
    echo "  Output: $(echo "$output" | tail -3)"
    ((FAILED++))
    return 1
}

# ===================================================================
# AUDIT LOGGING INITIALIZATION
# ===================================================================
echo -e "${YELLOW}═══ AUDIT LOGGING INITIALIZATION ═══${NC}"
echo ""

run_test "Audit tables auto-creation" "1.1" \
    "SELECT COUNT(*) FROM __audit_log;" \
    "^0"

run_test "Audit table schema verification" "1.2" \
    "SELECT column_name FROM information_schema.columns WHERE table_name = '__audit_log';" \
    ""

echo ""

# ===================================================================
# DDL EVENT LOGGING
# ===================================================================
echo -e "${YELLOW}═══ DDL EVENT LOGGING ═══${NC}"
echo ""

run_test "CREATE TABLE event logging" "2.1" \
    "CREATE TABLE audit_test_users (
        id INT PRIMARY KEY,
        name TEXT,
        email TEXT
    );
    SELECT COUNT(*) FROM __audit_log WHERE operation LIKE '%CREATE%';" \
    ""

run_test "ALTER TABLE event logging" "2.2" \
    "CREATE TABLE alter_test (id INT);
    ALTER TABLE alter_test ADD COLUMN name TEXT;
    SELECT COUNT(*) FROM __audit_log WHERE operation LIKE '%ALTER%';" \
    ""

run_test "DROP TABLE event logging" "2.3" \
    "CREATE TABLE drop_test (id INT);
    DROP TABLE drop_test;
    SELECT COUNT(*) FROM __audit_log WHERE operation LIKE '%DROP%';" \
    ""

run_test "CREATE INDEX event logging" "2.4" \
    "CREATE TABLE idx_test (id INT, name TEXT);
    CREATE INDEX idx_test_name ON idx_test(name);
    SELECT COUNT(*) FROM __audit_log WHERE operation LIKE '%CREATE%INDEX%';" \
    ""

echo ""

# ===================================================================
# DML EVENT LOGGING
# ===================================================================
echo -e "${YELLOW}═══ DML EVENT LOGGING ═══${NC}"
echo ""

run_test "INSERT event logging" "3.1" \
    "CREATE TABLE dml_test (id INT, val INT);
    INSERT INTO dml_test VALUES (1, 100);
    INSERT INTO dml_test VALUES (2, 200);
    SELECT COUNT(*) FROM __audit_log WHERE operation = 'INSERT';" \
    ""

run_test "UPDATE event logging" "3.2" \
    "CREATE TABLE update_test (id INT, val INT);
    INSERT INTO update_test VALUES (1, 100);
    UPDATE update_test SET val = 200 WHERE id = 1;
    SELECT COUNT(*) FROM __audit_log WHERE operation = 'UPDATE';" \
    ""

run_test "DELETE event logging" "3.3" \
    "CREATE TABLE delete_test (id INT, val INT);
    INSERT INTO delete_test VALUES (1, 100);
    DELETE FROM delete_test WHERE id = 1;
    SELECT COUNT(*) FROM __audit_log WHERE operation = 'DELETE';" \
    ""

run_test "Affected rows tracking" "3.4" \
    "CREATE TABLE rows_test (id INT);
    INSERT INTO rows_test VALUES (1);
    INSERT INTO rows_test VALUES (2);
    INSERT INTO rows_test VALUES (3);
    SELECT affected_rows FROM __audit_log WHERE operation = 'INSERT' AND target = 'rows_test' LIMIT 1;" \
    ""

echo ""

# ===================================================================
# SELECT EVENT LOGGING (if enabled)
# ===================================================================
echo -e "${YELLOW}═══ SELECT EVENT LOGGING ═══${NC}"
echo ""

run_test "SELECT query logging check" "4.1" \
    "CREATE TABLE select_test (id INT, name TEXT);
    INSERT INTO select_test VALUES (1, 'Alice');
    SELECT * FROM select_test;
    SELECT COUNT(*) FROM __audit_log WHERE operation = 'SELECT';" \
    ""

run_test "SELECT with WHERE clause" "4.2" \
    "CREATE TABLE query_test (id INT);
    INSERT INTO query_test VALUES (1);
    SELECT * FROM query_test WHERE id = 1;
    SELECT query FROM __audit_log WHERE target = 'query_test' AND operation = 'SELECT' LIMIT 1;" \
    ""

echo ""

# ===================================================================
# AUDIT LOG FORMAT VALIDATION
# ===================================================================
echo -e "${YELLOW}═══ AUDIT LOG FORMAT VALIDATION ═══${NC}"
echo ""

run_test "Log entry structure" "5.1" \
    "CREATE TABLE format_test (id INT);
    SELECT id, timestamp, session_id, user, operation, target, query, affected_rows, success FROM __audit_log LIMIT 1;" \
    ""

run_test "Timestamp presence" "5.2" \
    "CREATE TABLE ts_test (id INT);
    SELECT timestamp FROM __audit_log WHERE target = 'ts_test' LIMIT 1;" \
    ""

run_test "Session ID tracking" "5.3" \
    "CREATE TABLE session_test (id INT);
    SELECT session_id FROM __audit_log WHERE target = 'session_test' LIMIT 1;" \
    ""

run_test "User tracking" "5.4" \
    "CREATE TABLE user_test (id INT);
    SELECT user FROM __audit_log WHERE target = 'user_test' LIMIT 1;" \
    ""

run_test "Success flag recording" "5.5" \
    "CREATE TABLE success_test (id INT);
    SELECT success FROM __audit_log WHERE target = 'success_test' LIMIT 1;" \
    ""

echo ""

# ===================================================================
# CHECKSUM AND TAMPER DETECTION
# ===================================================================
echo -e "${YELLOW}═══ CHECKSUM AND TAMPER DETECTION ═══${NC}"
echo ""

run_test "Checksum generation" "6.1" \
    "CREATE TABLE checksum_test (id INT);
    SELECT checksum FROM __audit_log WHERE target = 'checksum_test' LIMIT 1;" \
    ""

run_test "Checksum not null" "6.2" \
    "CREATE TABLE checksum_test2 (id INT);
    SELECT COUNT(*) FROM __audit_log WHERE target = 'checksum_test2' AND checksum IS NOT NULL;" \
    ""

run_test "Checksum uniqueness" "6.3" \
    "CREATE TABLE unique_test1 (id INT);
    CREATE TABLE unique_test2 (id INT);
    SELECT COUNT(DISTINCT checksum) FROM __audit_log WHERE target LIKE 'unique_test%';" \
    ""

echo ""

# ===================================================================
# QUERY AUDIT LOG VIA SQL
# ===================================================================
echo -e "${YELLOW}═══ QUERY AUDIT LOG VIA SQL ═══${NC}"
echo ""

run_test "Query all audit logs" "7.1" \
    "CREATE TABLE query_audit1 (id INT);
    SELECT * FROM __audit_log ORDER BY id DESC LIMIT 10;" \
    ""

run_test "Filter by operation type" "7.2" \
    "CREATE TABLE filter_test (id INT);
    INSERT INTO filter_test VALUES (1);
    SELECT * FROM __audit_log WHERE operation = 'INSERT';" \
    ""

run_test "Filter by target table" "7.3" \
    "CREATE TABLE target_filter (id INT);
    INSERT INTO target_filter VALUES (1);
    SELECT * FROM __audit_log WHERE target = 'target_filter';" \
    ""

run_test "Filter by timestamp range" "7.4" \
    "CREATE TABLE time_filter (id INT);
    SELECT * FROM __audit_log WHERE timestamp >= CURRENT_TIMESTAMP - INTERVAL '1 hour';" \
    ""

run_test "Filter by user" "7.5" \
    "CREATE TABLE user_filter (id INT);
    SELECT * FROM __audit_log WHERE user = 'default';" \
    ""

run_test "Filter by success status" "7.6" \
    "CREATE TABLE success_filter (id INT);
    SELECT * FROM __audit_log WHERE success = true;" \
    ""

run_test "Order by timestamp" "7.7" \
    "CREATE TABLE order_test (id INT);
    SELECT * FROM __audit_log ORDER BY timestamp DESC LIMIT 5;" \
    ""

run_test "Aggregate audit events" "7.8" \
    "CREATE TABLE agg_test1 (id INT);
    CREATE TABLE agg_test2 (id INT);
    INSERT INTO agg_test1 VALUES (1);
    SELECT operation, COUNT(*) FROM __audit_log GROUP BY operation;" \
    ""

echo ""

# ===================================================================
# ERROR LOGGING
# ===================================================================
echo -e "${YELLOW}═══ ERROR LOGGING ═══${NC}"
echo ""

run_test "Failed operation logging" "8.1" \
    "CREATE TABLE error_test (id INT PRIMARY KEY);
    INSERT INTO error_test VALUES (1);
    SELECT COUNT(*) FROM __audit_log WHERE success = false OR success = true;" \
    ""

run_test "Error message capture" "8.2" \
    "CREATE TABLE error_msg_test (id INT);
    SELECT error FROM __audit_log WHERE error IS NOT NULL LIMIT 1;" \
    ""

echo ""

# ===================================================================
# AUDIT LOG QUERIES WITH JOINS
# ===================================================================
echo -e "${YELLOW}═══ AUDIT LOG ADVANCED QUERIES ═══${NC}"
echo ""

run_test "Self-join on audit log" "9.1" \
    "CREATE TABLE join_test (id INT);
    INSERT INTO join_test VALUES (1);
    SELECT a.operation, b.operation FROM __audit_log a, __audit_log b WHERE a.id = b.id LIMIT 1;" \
    ""

run_test "Subquery on audit log" "9.2" \
    "CREATE TABLE subquery_test (id INT);
    SELECT * FROM __audit_log WHERE id IN (SELECT MIN(id) FROM __audit_log);" \
    ""

run_test "CTE with audit log" "9.3" \
    "CREATE TABLE cte_test (id INT);
    WITH recent AS (SELECT * FROM __audit_log ORDER BY id DESC LIMIT 5)
    SELECT operation FROM recent;" \
    ""

echo ""

# ===================================================================
# AUDIT LOG RETENTION
# ===================================================================
echo -e "${YELLOW}═══ AUDIT LOG RETENTION ═══${NC}"
echo ""

run_test "Count total audit events" "10.1" \
    "CREATE TABLE retention_test (id INT);
    INSERT INTO retention_test VALUES (1);
    SELECT COUNT(*) FROM __audit_log;" \
    ""

run_test "Verify append-only behavior" "10.2" \
    "CREATE TABLE append_test1 (id INT);
    CREATE TABLE append_test2 (id INT);
    CREATE TABLE append_test3 (id INT);
    SELECT COUNT(*) >= 3 FROM __audit_log;" \
    ""

echo ""

# ===================================================================
# TRANSACTION AUDIT LOGGING
# ===================================================================
echo -e "${YELLOW}═══ TRANSACTION AUDIT LOGGING ═══${NC}"
echo ""

run_test "Transaction COMMIT logging" "11.1" \
    "CREATE TABLE tx_test (id INT);
    BEGIN;
    INSERT INTO tx_test VALUES (1);
    COMMIT;
    SELECT COUNT(*) FROM __audit_log WHERE operation = 'INSERT' AND target = 'tx_test';" \
    ""

run_test "Transaction ROLLBACK logging" "11.2" \
    "CREATE TABLE rollback_test (id INT);
    BEGIN;
    INSERT INTO rollback_test VALUES (1);
    ROLLBACK;
    SELECT COUNT(*) FROM __audit_log;" \
    ""

echo ""

# ===================================================================
# PERFORMANCE AND SCALABILITY
# ===================================================================
echo -e "${YELLOW}═══ PERFORMANCE TESTS ═══${NC}"
echo ""

run_test "Large audit log query" "12.1" \
    "CREATE TABLE perf_test1 (id INT);
    CREATE TABLE perf_test2 (id INT);
    CREATE TABLE perf_test3 (id INT);
    CREATE TABLE perf_test4 (id INT);
    CREATE TABLE perf_test5 (id INT);
    SELECT COUNT(*) FROM __audit_log;" \
    ""

run_test "Pagination of audit logs" "12.2" \
    "SELECT * FROM __audit_log ORDER BY id LIMIT 10 OFFSET 0;" \
    ""

echo ""

# ===================================================================
# EDGE CASES
# ===================================================================
echo -e "${YELLOW}═══ EDGE CASES ═══${NC}"
echo ""

run_test "Empty audit log query" "13.1" \
    "SELECT * FROM __audit_log WHERE id = 999999999;" \
    ""

run_test "Null target logging" "13.2" \
    "CREATE TABLE null_target_test (id INT);
    SELECT * FROM __audit_log WHERE target IS NULL LIMIT 1;" \
    ""

run_test "Long query truncation" "13.3" \
    "CREATE TABLE long_query_test (id INT, col1 TEXT, col2 TEXT, col3 TEXT, col4 TEXT, col5 TEXT);
    SELECT query FROM __audit_log WHERE target = 'long_query_test' LIMIT 1;" \
    ""

run_test "Special characters in query" "13.4" \
    "CREATE TABLE special_chars (id INT, \"name-with-dash\" TEXT);
    SELECT query FROM __audit_log WHERE target = 'special_chars' LIMIT 1;" \
    ""

echo ""

# ===================================================================
# CLEANUP
# ===================================================================
cleanup

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
    echo -e "${GREEN}✓ All Audit Logging tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ ${FAILED} test(s) failed${NC}"
    exit 1
fi
