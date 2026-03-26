#!/bin/bash
#
# HeliosDB Nano REST API Comprehensive Test Script
#
# Tests all REST API endpoints with proper validation, error handling,
# and cleanup. Includes color-coded output and detailed test reporting.
#
# Usage: ./test_rest_api.sh [PORT]
#   PORT: Optional port number (default: 8080)
#

set -e

# Configuration
PORT="${1:-8080}"
BASE_URL="http://localhost:${PORT}"
API_BASE="${BASE_URL}/v1"
SERVER_PID=""
TEST_BRANCH="test_branch_$$"
TEST_TABLE="test_users"
DB_PATH="/tmp/heliosdb_api_test_$$"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# Test counters
TOTAL_TESTS=0
PASSED_TESTS=0
FAILED_TESTS=0

# Cleanup function
cleanup() {
    echo ""
    echo -e "${CYAN}=== Cleanup ===${NC}"

    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        echo -e "${YELLOW}Stopping API server (PID: $SERVER_PID)...${NC}"
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        echo -e "${GREEN}Server stopped${NC}"
    fi

    if [ -d "$DB_PATH" ]; then
        echo -e "${YELLOW}Removing test database: $DB_PATH${NC}"
        rm -rf "$DB_PATH"
        echo -e "${GREEN}Test database removed${NC}"
    fi

    # Print test summary
    echo ""
    echo -e "${CYAN}=== Test Summary ===${NC}"
    echo -e "Total Tests:  ${BLUE}${TOTAL_TESTS}${NC}"
    echo -e "Passed:       ${GREEN}${PASSED_TESTS}${NC}"
    echo -e "Failed:       ${RED}${FAILED_TESTS}${NC}"

    if [ $FAILED_TESTS -eq 0 ]; then
        echo -e "${GREEN}All tests passed!${NC}"
        exit 0
    else
        echo -e "${RED}Some tests failed!${NC}"
        exit 1
    fi
}

# Set trap for cleanup on exit
trap cleanup EXIT INT TERM

# Test helper function
run_test() {
    local test_name="$1"
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -e "\n${BLUE}[Test $TOTAL_TESTS]${NC} $test_name"
}

# Pass/Fail helper functions
test_passed() {
    PASSED_TESTS=$((PASSED_TESTS + 1))
    echo -e "${GREEN}✓ PASSED${NC}"
}

test_failed() {
    local message="$1"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    echo -e "${RED}✗ FAILED${NC}: $message"
}

# JSON validation helper
validate_json() {
    local response="$1"
    if ! echo "$response" | jq empty 2>/dev/null; then
        return 1
    fi
    return 0
}

# HTTP request helper with better error handling
http_request() {
    local method="$1"
    local url="$2"
    local data="$3"
    local expected_status="${4:-200}"

    local response
    local status_code

    if [ -z "$data" ]; then
        response=$(curl -s -w "\n%{http_code}" -X "$method" "$url" -H "Content-Type: application/json" 2>&1)
    else
        response=$(curl -s -w "\n%{http_code}" -X "$method" "$url" -H "Content-Type: application/json" -d "$data" 2>&1)
    fi

    status_code=$(echo "$response" | tail -n 1)
    response=$(echo "$response" | head -n -1)

    if [ "$status_code" != "$expected_status" ]; then
        echo -e "${YELLOW}Expected status: $expected_status, Got: $status_code${NC}"
        echo -e "${YELLOW}Response: $response${NC}"
        return 1
    fi

    echo "$response"
    return 0
}

# Wait for server to be ready
wait_for_server() {
    local max_attempts=30
    local attempt=0

    echo -e "${YELLOW}Waiting for server to be ready...${NC}"

    while [ $attempt -lt $max_attempts ]; do
        if curl -s -f "${BASE_URL}/health" > /dev/null 2>&1; then
            echo -e "${GREEN}Server is ready!${NC}"
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 1
        echo -n "."
    done

    echo -e "\n${RED}Server failed to start within ${max_attempts} seconds${NC}"
    return 1
}

# Main test execution
main() {
    echo -e "${CYAN}╔════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║  HeliosDB Nano REST API Test Suite           ║${NC}"
    echo -e "${CYAN}╚════════════════════════════════════════════════╝${NC}"
    echo ""
    echo -e "${YELLOW}Configuration:${NC}"
    echo -e "  Port:        ${PORT}"
    echo -e "  Base URL:    ${BASE_URL}"
    echo -e "  DB Path:     ${DB_PATH}"
    echo -e "  Test Branch: ${TEST_BRANCH}"

    # Check dependencies
    echo ""
    echo -e "${CYAN}=== Checking Dependencies ===${NC}"

    if ! command -v jq &> /dev/null; then
        echo -e "${RED}Error: jq is required but not installed${NC}"
        echo "Install with: sudo apt-get install jq (Ubuntu/Debian) or brew install jq (macOS)"
        exit 1
    fi
    echo -e "${GREEN}✓ jq found${NC}"

    if ! command -v curl &> /dev/null; then
        echo -e "${RED}Error: curl is required but not installed${NC}"
        exit 1
    fi
    echo -e "${GREEN}✓ curl found${NC}"

    # Check if binary exists
    if [ ! -f "./target/debug/heliosdb-nano" ] && [ ! -f "./target/release/heliosdb-nano" ]; then
        echo -e "${RED}Error: heliosdb-nano binary not found${NC}"
        echo "Build with: cargo build"
        exit 1
    fi

    BINARY="./target/debug/heliosdb-nano"
    if [ -f "./target/release/heliosdb-nano" ]; then
        BINARY="./target/release/heliosdb-nano"
    fi
    echo -e "${GREEN}✓ Binary found: $BINARY${NC}"

    # Start API server
    echo ""
    echo -e "${CYAN}=== Starting API Server ===${NC}"

    # Start server in background
    HELIOSDB_API_PORT="$PORT" HELIOSDB_DATA_DIR="$DB_PATH" "$BINARY" api &
    SERVER_PID=$!

    echo -e "${GREEN}Server started with PID: $SERVER_PID${NC}"

    # Wait for server to be ready
    if ! wait_for_server; then
        echo -e "${RED}Failed to start server${NC}"
        exit 1
    fi

    echo ""
    echo -e "${CYAN}=== Running Tests ===${NC}"

    # ========================================
    # Test 1: Health Check
    # ========================================
    run_test "Health Check Endpoint"
    response=$(http_request "GET" "${BASE_URL}/health" "" 200)
    if [ $? -eq 0 ] && [ "$response" = "OK" ]; then
        test_passed
    else
        test_failed "Health check failed"
    fi

    # ========================================
    # Test 2: Version Info
    # ========================================
    run_test "Version Info Endpoint"
    response=$(http_request "GET" "${BASE_URL}/version" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        name=$(echo "$response" | jq -r '.name')
        version=$(echo "$response" | jq -r '.version')
        api_version=$(echo "$response" | jq -r '.api_version')

        if [ "$name" = "HeliosDB Nano" ] && [ -n "$version" ] && [ "$api_version" = "v1" ]; then
            echo -e "${CYAN}  Name: $name, Version: $version, API: $api_version${NC}"
            test_passed
        else
            test_failed "Invalid version info"
        fi
    else
        test_failed "Version endpoint failed"
    fi

    # ========================================
    # Test 3: List Branches (Initial)
    # ========================================
    run_test "List Branches (Initial)"
    response=$(http_request "GET" "${API_BASE}/branches" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        branch_count=$(echo "$response" | jq -r '.branches | length')
        echo -e "${CYAN}  Found $branch_count branch(es)${NC}"
        test_passed
    else
        test_failed "Failed to list branches"
    fi

    # ========================================
    # Test 4: Create Test Branch
    # ========================================
    run_test "Create Branch: $TEST_BRANCH"
    data=$(jq -n --arg name "$TEST_BRANCH" '{name: $name, from: "main"}')
    response=$(http_request "POST" "${API_BASE}/branches" "$data" 201)
    if [ $? -eq 0 ] && validate_json "$response"; then
        created_name=$(echo "$response" | jq -r '.name')
        if [ "$created_name" = "$TEST_BRANCH" ]; then
            echo -e "${CYAN}  Created branch: $created_name${NC}"
            test_passed
        else
            test_failed "Branch name mismatch"
        fi
    else
        test_failed "Failed to create branch"
    fi

    # ========================================
    # Test 5: Get Branch Details
    # ========================================
    run_test "Get Branch Details: $TEST_BRANCH"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        branch_name=$(echo "$response" | jq -r '.name')
        if [ "$branch_name" = "$TEST_BRANCH" ]; then
            test_passed
        else
            test_failed "Branch name mismatch"
        fi
    else
        test_failed "Failed to get branch details"
    fi

    # ========================================
    # Test 6: Create Table via Execute
    # ========================================
    run_test "Create Table: $TEST_TABLE"
    sql="CREATE TABLE $TEST_TABLE (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT, age INTEGER)"
    data=$(jq -n --arg sql "$sql" '{sql: $sql}')
    response=$(http_request "POST" "${API_BASE}/branches/${TEST_BRANCH}/execute" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        affected=$(echo "$response" | jq -r '.affected_rows')
        echo -e "${CYAN}  Table created, affected rows: $affected${NC}"
        test_passed
    else
        test_failed "Failed to create table"
    fi

    # ========================================
    # Test 7: List Tables
    # ========================================
    run_test "List Tables in Branch: $TEST_BRANCH"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}/tables" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        tables=$(echo "$response" | jq -r '.tables | map(.name) | join(", ")')
        echo -e "${CYAN}  Tables: $tables${NC}"
        if echo "$response" | jq -e ".tables[] | select(.name == \"$TEST_TABLE\")" > /dev/null; then
            test_passed
        else
            test_failed "Test table not found in list"
        fi
    else
        test_failed "Failed to list tables"
    fi

    # ========================================
    # Test 8: Insert Data via Execute
    # ========================================
    run_test "Insert Data via Execute Endpoint"
    sql="INSERT INTO $TEST_TABLE (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)"
    data=$(jq -n --arg sql "$sql" '{sql: $sql}')
    response=$(http_request "POST" "${API_BASE}/branches/${TEST_BRANCH}/execute" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        affected=$(echo "$response" | jq -r '.affected_rows')
        if [ "$affected" = "1" ]; then
            test_passed
        else
            test_failed "Expected 1 affected row, got $affected"
        fi
    else
        test_failed "Failed to insert data"
    fi

    # ========================================
    # Test 9: Insert Data via Data Handler
    # ========================================
    run_test "Insert Data via Data Handler"
    data='{"rows": [{"id": 2, "name": "Bob", "email": "bob@example.com", "age": 25}]}'
    response=$(http_request "POST" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        affected=$(echo "$response" | jq -r '.affected_rows')
        if [ "$affected" = "1" ]; then
            test_passed
        else
            test_failed "Expected 1 affected row, got $affected"
        fi
    else
        test_failed "Failed to insert data via handler"
    fi

    # ========================================
    # Test 10: Query Data via Query Endpoint
    # ========================================
    run_test "Query Data via Query Endpoint"
    sql="SELECT * FROM $TEST_TABLE ORDER BY id"
    data=$(jq -n --arg sql "$sql" '{sql: $sql}')
    response=$(http_request "POST" "${API_BASE}/branches/${TEST_BRANCH}/query" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        row_count=$(echo "$response" | jq -r '.row_count')
        if [ "$row_count" = "2" ]; then
            echo -e "${CYAN}  Retrieved $row_count rows${NC}"
            test_passed
        else
            test_failed "Expected 2 rows, got $row_count"
        fi
    else
        test_failed "Failed to query data"
    fi

    # ========================================
    # Test 11: Query Data via Data Handler
    # ========================================
    run_test "Query Data via Data Handler"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        row_count=$(echo "$response" | jq -r '.row_count')
        if [ "$row_count" = "2" ]; then
            test_passed
        else
            test_failed "Expected 2 rows, got $row_count"
        fi
    else
        test_failed "Failed to query data via handler"
    fi

    # ========================================
    # Test 12: Query with Filter
    # ========================================
    run_test "Query Data with Filter"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data?filter=age>25" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        row_count=$(echo "$response" | jq -r '.row_count')
        if [ "$row_count" = "1" ]; then
            echo -e "${CYAN}  Filter applied, retrieved $row_count row${NC}"
            test_passed
        else
            test_failed "Expected 1 row with age>25, got $row_count"
        fi
    else
        test_failed "Failed to query with filter"
    fi

    # ========================================
    # Test 13: Query with Pagination
    # ========================================
    run_test "Query Data with Pagination"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data?page=1&limit=1" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        row_count=$(echo "$response" | jq -r '.row_count')
        if [ "$row_count" = "1" ]; then
            echo -e "${CYAN}  Pagination applied, retrieved $row_count row${NC}"
            test_passed
        else
            test_failed "Expected 1 row with limit=1, got $row_count"
        fi
    else
        test_failed "Failed to query with pagination"
    fi

    # ========================================
    # Test 14: Query Specific Columns
    # ========================================
    run_test "Query Specific Columns"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data?columns=id,name" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        columns=$(echo "$response" | jq -r '.columns | length')
        if [ "$columns" = "2" ]; then
            echo -e "${CYAN}  Retrieved $columns columns${NC}"
            test_passed
        else
            test_failed "Expected 2 columns, got $columns"
        fi
    else
        test_failed "Failed to query specific columns"
    fi

    # ========================================
    # Test 15: Update Data
    # ========================================
    run_test "Update Data via Data Handler"
    data='{"values": {"email": "alice.updated@example.com"}, "filter": "id = 1"}'
    response=$(http_request "PUT" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        affected=$(echo "$response" | jq -r '.affected_rows')
        if [ "$affected" = "1" ]; then
            test_passed
        else
            test_failed "Expected 1 affected row, got $affected"
        fi
    else
        test_failed "Failed to update data"
    fi

    # ========================================
    # Test 16: Verify Update
    # ========================================
    run_test "Verify Data Update"
    sql="SELECT email FROM $TEST_TABLE WHERE id = 1"
    data=$(jq -n --arg sql "$sql" '{sql: $sql}')
    response=$(http_request "POST" "${API_BASE}/branches/${TEST_BRANCH}/query" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        email=$(echo "$response" | jq -r '.rows[0].email')
        if [ "$email" = "alice.updated@example.com" ]; then
            echo -e "${CYAN}  Email updated successfully${NC}"
            test_passed
        else
            test_failed "Email not updated correctly"
        fi
    else
        test_failed "Failed to verify update"
    fi

    # ========================================
    # Test 17: Parameterized Query
    # ========================================
    run_test "Parameterized Query"
    sql="SELECT * FROM $TEST_TABLE WHERE id = \$1"
    data=$(jq -n --arg sql "$sql" '{sql: $sql, params: [{"type": "int4", "value": 1}]}')
    response=$(http_request "POST" "${API_BASE}/branches/${TEST_BRANCH}/query" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        row_count=$(echo "$response" | jq -r '.row_count')
        if [ "$row_count" = "1" ]; then
            test_passed
        else
            test_failed "Expected 1 row, got $row_count"
        fi
    else
        test_failed "Failed parameterized query"
    fi

    # ========================================
    # Test 18: Delete Data via Data Handler
    # ========================================
    run_test "Delete Data via Data Handler"
    data='{"filter": "id = 2"}'
    response=$(http_request "DELETE" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        affected=$(echo "$response" | jq -r '.affected_rows')
        if [ "$affected" = "1" ]; then
            test_passed
        else
            test_failed "Expected 1 affected row, got $affected"
        fi
    else
        test_failed "Failed to delete data"
    fi

    # ========================================
    # Test 19: Verify Deletion
    # ========================================
    run_test "Verify Data Deletion"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}/tables/${TEST_TABLE}/data" "" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        row_count=$(echo "$response" | jq -r '.row_count')
        if [ "$row_count" = "1" ]; then
            echo -e "${CYAN}  1 row remaining after deletion${NC}"
            test_passed
        else
            test_failed "Expected 1 row after deletion, got $row_count"
        fi
    else
        test_failed "Failed to verify deletion"
    fi

    # ========================================
    # Test 20: Merge Branch (Create target first)
    # ========================================
    run_test "Create Target Branch for Merge"
    target_branch="${TEST_BRANCH}_target"
    data=$(jq -n --arg name "$target_branch" '{name: $name, from: "main"}')
    response=$(http_request "POST" "${API_BASE}/branches" "$data" 201)
    if [ $? -eq 0 ] && validate_json "$response"; then
        test_passed
    else
        test_failed "Failed to create target branch"
    fi

    # ========================================
    # Test 21: Merge Branches
    # ========================================
    run_test "Merge Branch: $TEST_BRANCH -> $target_branch"
    data=$(jq -n --arg target "$target_branch" '{target: $target}')
    response=$(http_request "POST" "${API_BASE}/branches/${TEST_BRANCH}/merge" "$data" 200)
    if [ $? -eq 0 ] && validate_json "$response"; then
        echo -e "${CYAN}  Merge completed${NC}"
        test_passed
    else
        test_failed "Failed to merge branches"
    fi

    # ========================================
    # Test 22: Delete Target Branch
    # ========================================
    run_test "Delete Branch: $target_branch"
    response=$(http_request "DELETE" "${API_BASE}/branches/${target_branch}" "" 200)
    if [ $? -eq 0 ]; then
        test_passed
    else
        test_failed "Failed to delete target branch"
    fi

    # ========================================
    # Test 23: Delete Test Branch
    # ========================================
    run_test "Delete Branch: $TEST_BRANCH"
    response=$(http_request "DELETE" "${API_BASE}/branches/${TEST_BRANCH}" "" 200)
    if [ $? -eq 0 ]; then
        test_passed
    else
        test_failed "Failed to delete test branch"
    fi

    # ========================================
    # Test 24: Verify Branch Deletion
    # ========================================
    run_test "Verify Branch Deletion"
    response=$(http_request "GET" "${API_BASE}/branches/${TEST_BRANCH}" "" 404)
    if [ $? -eq 0 ]; then
        echo -e "${CYAN}  Branch not found (expected)${NC}"
        test_passed
    else
        test_failed "Branch still exists after deletion"
    fi

    # ========================================
    # Test 25: Error Handling - Invalid SQL
    # ========================================
    run_test "Error Handling - Invalid SQL"
    sql="SELECT * FROM nonexistent_table"
    data=$(jq -n --arg sql "$sql" '{sql: $sql}')
    response=$(http_request "POST" "${API_BASE}/branches/main/query" "$data" 400)
    if [ $? -eq 0 ]; then
        echo -e "${CYAN}  Error handled correctly (400)${NC}"
        test_passed
    else
        test_failed "Invalid SQL not handled correctly"
    fi

    # ========================================
    # Test 26: Error Handling - Nonexistent Branch
    # ========================================
    run_test "Error Handling - Nonexistent Branch"
    response=$(http_request "GET" "${API_BASE}/branches/nonexistent_branch_123" "" 404)
    if [ $? -eq 0 ]; then
        echo -e "${CYAN}  Error handled correctly (404)${NC}"
        test_passed
    else
        test_failed "Nonexistent branch not handled correctly"
    fi

    echo ""
    echo -e "${CYAN}=== All Tests Completed ===${NC}"
}

# Run main function
main
