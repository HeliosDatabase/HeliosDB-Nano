#!/bin/bash
# HeliosDB-Lite HA Hardening Test Harness
# Core library for all HA tests

set -o pipefail

# ============================================================================
# Configuration
# ============================================================================

export TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export LIB_DIR="$TEST_DIR/lib"
export LOG_DIR="${LOG_DIR:-/tmp/ha-tests-$(date +%Y%m%d-%H%M%S)}"
export RESULTS_FILE="$LOG_DIR/results.json"

# Cluster configuration
export PRIMARY_PG="localhost:15432"
export PRIMARY_HTTP="localhost:18080"
export STANDBY_SYNC_PG="localhost:15442"
export STANDBY_SYNC_HTTP="localhost:18081"
export STANDBY_SEMISYNC_PG="localhost:15452"
export STANDBY_SEMISYNC_HTTP="localhost:18082"
export STANDBY_ASYNC_PG="localhost:15462"
export STANDBY_ASYNC_HTTP="localhost:18084"
export OBSERVER_HTTP="localhost:18083"
export PROXY_PG="localhost:15400"
export PROXY_ADMIN="localhost:19090"

# Container names
export PRIMARY_CONTAINER="heliosdb-primary"
export STANDBY_SYNC_CONTAINER="heliosdb-standby-sync"
export STANDBY_SEMISYNC_CONTAINER="heliosdb-standby-semisync"
export STANDBY_ASYNC_CONTAINER="heliosdb-standby-async"
export OBSERVER_CONTAINER="heliosdb-observer"
export PROXY_CONTAINER="heliosdb-proxy"

# Timeouts
export DEFAULT_TIMEOUT=30
export FAILOVER_TIMEOUT=60
export SYNC_TIMEOUT=30

# Colors
export RED='\033[0;31m'
export GREEN='\033[0;32m'
export YELLOW='\033[1;33m'
export BLUE='\033[0;34m'
export MAGENTA='\033[0;35m'
export CYAN='\033[0;36m'
export NC='\033[0m'

# ============================================================================
# Initialization
# ============================================================================

init_test_harness() {
    mkdir -p "$LOG_DIR"
    echo '{"tests": [], "summary": {}}' > "$RESULTS_FILE"

    # Initialize counters
    export TESTS_TOTAL=0
    export TESTS_PASSED=0
    export TESTS_FAILED=0
    export TESTS_SKIPPED=0

    log_info "Test harness initialized"
    log_info "Log directory: $LOG_DIR"
}

# ============================================================================
# Logging
# ============================================================================

log_info() {
    echo -e "${BLUE}[INFO]${NC} $(date '+%H:%M:%S') $1" | tee -a "$LOG_DIR/harness.log"
}

log_success() {
    echo -e "${GREEN}[PASS]${NC} $(date '+%H:%M:%S') $1" | tee -a "$LOG_DIR/harness.log"
}

log_error() {
    echo -e "${RED}[FAIL]${NC} $(date '+%H:%M:%S') $1" | tee -a "$LOG_DIR/harness.log"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $(date '+%H:%M:%S') $1" | tee -a "$LOG_DIR/harness.log"
}

log_debug() {
    if [[ "${DEBUG:-0}" == "1" ]]; then
        echo -e "${MAGENTA}[DEBUG]${NC} $(date '+%H:%M:%S') $1" | tee -a "$LOG_DIR/harness.log"
    fi
}

# ============================================================================
# Test Execution
# ============================================================================

# Start a test
# Usage: start_test "TEST_ID" "Test description"
start_test() {
    local test_id="$1"
    local description="$2"

    export CURRENT_TEST_ID="$test_id"
    export CURRENT_TEST_DESC="$description"
    export CURRENT_TEST_START=$(date +%s%3N)
    export CURRENT_TEST_LOG="$LOG_DIR/${test_id}.log"

    TESTS_TOTAL=$((TESTS_TOTAL + 1))

    echo "" | tee -a "$LOG_DIR/harness.log"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}" | tee -a "$LOG_DIR/harness.log"
    echo -e "${CYAN}[TEST]${NC} $test_id: $description" | tee -a "$LOG_DIR/harness.log"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}" | tee -a "$LOG_DIR/harness.log"

    echo "=== TEST START: $test_id ===" > "$CURRENT_TEST_LOG"
    echo "Description: $description" >> "$CURRENT_TEST_LOG"
    echo "Started at: $(date)" >> "$CURRENT_TEST_LOG"
    echo "" >> "$CURRENT_TEST_LOG"
}

# Pass a test
# Usage: pass_test ["Optional message"]
pass_test() {
    local message="${1:-Test passed}"
    local end_time=$(date +%s%3N)
    local duration=$((end_time - CURRENT_TEST_START))

    TESTS_PASSED=$((TESTS_PASSED + 1))

    log_success "$CURRENT_TEST_ID: $message (${duration}ms)"

    echo "" >> "$CURRENT_TEST_LOG"
    echo "=== TEST PASSED ===" >> "$CURRENT_TEST_LOG"
    echo "Duration: ${duration}ms" >> "$CURRENT_TEST_LOG"

    # Record result
    record_result "$CURRENT_TEST_ID" "PASSED" "$duration" "$message"
}

# Fail a test
# Usage: fail_test "Failure reason"
fail_test() {
    local reason="$1"
    local end_time=$(date +%s%3N)
    local duration=$((end_time - CURRENT_TEST_START))

    TESTS_FAILED=$((TESTS_FAILED + 1))

    log_error "$CURRENT_TEST_ID: $reason (${duration}ms)"

    echo "" >> "$CURRENT_TEST_LOG"
    echo "=== TEST FAILED ===" >> "$CURRENT_TEST_LOG"
    echo "Reason: $reason" >> "$CURRENT_TEST_LOG"
    echo "Duration: ${duration}ms" >> "$CURRENT_TEST_LOG"

    # Record result
    record_result "$CURRENT_TEST_ID" "FAILED" "$duration" "$reason"
}

# Skip a test
# Usage: skip_test "Skip reason"
skip_test() {
    local reason="$1"

    TESTS_SKIPPED=$((TESTS_SKIPPED + 1))

    log_warn "$CURRENT_TEST_ID: SKIPPED - $reason"

    echo "" >> "$CURRENT_TEST_LOG"
    echo "=== TEST SKIPPED ===" >> "$CURRENT_TEST_LOG"
    echo "Reason: $reason" >> "$CURRENT_TEST_LOG"

    # Record result
    record_result "$CURRENT_TEST_ID" "SKIPPED" "0" "$reason"
}

# Record result to JSON
record_result() {
    local test_id="$1"
    local status="$2"
    local duration="$3"
    local message="$4"

    local temp_file=$(mktemp)
    jq --arg id "$test_id" \
       --arg status "$status" \
       --arg duration "$duration" \
       --arg message "$message" \
       --arg desc "$CURRENT_TEST_DESC" \
       '.tests += [{"id": $id, "description": $desc, "status": $status, "duration_ms": ($duration | tonumber), "message": $message}]' \
       "$RESULTS_FILE" > "$temp_file" && mv "$temp_file" "$RESULTS_FILE"
}

# ============================================================================
# Cluster Operations
# ============================================================================

# Check if node is healthy
# Usage: check_node_health "host:port" [timeout]
check_node_health() {
    local endpoint="$1"
    local timeout="${2:-5}"

    local host="${endpoint%:*}"
    local port="${endpoint#*:}"

    if curl -sf --connect-timeout "$timeout" "http://${host}:${port}/health" >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

# Wait for node to be healthy
# Usage: wait_for_node "host:port" [max_wait]
wait_for_node() {
    local endpoint="$1"
    local max_wait="${2:-60}"
    local waited=0

    while [[ $waited -lt $max_wait ]]; do
        if check_node_health "$endpoint" 2; then
            log_debug "Node $endpoint is healthy"
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    log_error "Node $endpoint not healthy after ${max_wait}s"
    return 1
}

# Wait for entire cluster to be healthy
wait_for_cluster() {
    local max_wait="${1:-120}"

    log_info "Waiting for cluster to be healthy..."

    wait_for_node "$PRIMARY_HTTP" "$max_wait" || return 1
    wait_for_node "$STANDBY_SYNC_HTTP" "$max_wait" || return 1
    wait_for_node "$STANDBY_SEMISYNC_HTTP" "$max_wait" || return 1
    wait_for_node "$STANDBY_ASYNC_HTTP" "$max_wait" || return 1
    wait_for_node "$PROXY_ADMIN" "$max_wait" || return 1

    log_success "Cluster is healthy"
    return 0
}

# Execute SQL via psql
# Usage: exec_sql "host:port" "SQL query"
exec_sql() {
    local endpoint="$1"
    local query="$2"

    local host="${endpoint%:*}"
    local port="${endpoint#*:}"

    psql -h "$host" -p "$port" -t -A -c "$query" 2>&1
}

# Execute SQL and check success
# Usage: exec_sql_check "host:port" "SQL query"
exec_sql_check() {
    local endpoint="$1"
    local query="$2"

    local result
    result=$(exec_sql "$endpoint" "$query" 2>&1)
    local status=$?

    if [[ $status -eq 0 ]] && [[ ! "$result" =~ ERROR ]]; then
        echo "$result"
        return 0
    else
        echo "$result" >&2
        return 1
    fi
}

# Get replication lag in bytes
# Usage: get_replication_lag "standby_http_endpoint"
get_replication_lag() {
    local endpoint="$1"
    local host="${endpoint%:*}"
    local port="${endpoint#*:}"

    curl -sf "http://${host}:${port}/metrics" 2>/dev/null | \
        grep -oP 'replication_lag_bytes\s+\K\d+' || echo "0"
}

# ============================================================================
# Container Operations
# ============================================================================

# Kill a container (SIGKILL)
# Usage: kill_container "container_name"
kill_container() {
    local container="$1"
    log_info "Killing container: $container"
    docker kill "$container" 2>/dev/null
}

# Stop a container gracefully
# Usage: stop_container "container_name"
stop_container() {
    local container="$1"
    log_info "Stopping container: $container"
    docker stop "$container" 2>/dev/null
}

# Start a container
# Usage: start_container "container_name"
start_container() {
    local container="$1"
    log_info "Starting container: $container"
    docker start "$container" 2>/dev/null
}

# Restart a container
# Usage: restart_container "container_name"
restart_container() {
    local container="$1"
    log_info "Restarting container: $container"
    docker restart "$container" 2>/dev/null
}

# Pause a container (freeze process)
# Usage: pause_container "container_name"
pause_container() {
    local container="$1"
    log_info "Pausing container: $container"
    docker pause "$container" 2>/dev/null
}

# Unpause a container
# Usage: unpause_container "container_name"
unpause_container() {
    local container="$1"
    log_info "Unpausing container: $container"
    docker unpause "$container" 2>/dev/null
}

# Isolate container network
# Usage: isolate_container_network "container_name"
isolate_container_network() {
    local container="$1"
    log_info "Isolating network for: $container"
    docker exec "$container" sh -c "iptables -A INPUT -j DROP; iptables -A OUTPUT -j DROP" 2>/dev/null || \
    docker network disconnect heliosdb-ha "$container" 2>/dev/null
}

# Restore container network
# Usage: restore_container_network "container_name"
restore_container_network() {
    local container="$1"
    log_info "Restoring network for: $container"
    docker exec "$container" sh -c "iptables -F" 2>/dev/null || \
    docker network connect heliosdb-ha "$container" 2>/dev/null
}

# ============================================================================
# Assertion Helpers
# ============================================================================

# Assert equality
# Usage: assert_eq "actual" "expected" "message"
assert_eq() {
    local actual="$1"
    local expected="$2"
    local message="$3"

    if [[ "$actual" == "$expected" ]]; then
        log_debug "Assert passed: $message (actual='$actual')"
        return 0
    else
        log_error "Assert failed: $message (expected='$expected', actual='$actual')"
        return 1
    fi
}

# Assert not equal
# Usage: assert_ne "actual" "not_expected" "message"
assert_ne() {
    local actual="$1"
    local not_expected="$2"
    local message="$3"

    if [[ "$actual" != "$not_expected" ]]; then
        log_debug "Assert passed: $message (actual='$actual' != '$not_expected')"
        return 0
    else
        log_error "Assert failed: $message (should not be '$not_expected')"
        return 1
    fi
}

# Assert greater than
# Usage: assert_gt "actual" "threshold" "message"
assert_gt() {
    local actual="$1"
    local threshold="$2"
    local message="$3"

    if [[ "$actual" -gt "$threshold" ]]; then
        log_debug "Assert passed: $message ($actual > $threshold)"
        return 0
    else
        log_error "Assert failed: $message ($actual not > $threshold)"
        return 1
    fi
}

# Assert less than
# Usage: assert_lt "actual" "threshold" "message"
assert_lt() {
    local actual="$1"
    local threshold="$2"
    local message="$3"

    if [[ "$actual" -lt "$threshold" ]]; then
        log_debug "Assert passed: $message ($actual < $threshold)"
        return 0
    else
        log_error "Assert failed: $message ($actual not < $threshold)"
        return 1
    fi
}

# Assert command succeeds
# Usage: assert_success "command" "message"
assert_success() {
    local cmd="$1"
    local message="$2"

    if eval "$cmd" >/dev/null 2>&1; then
        log_debug "Assert passed: $message"
        return 0
    else
        log_error "Assert failed: $message (command failed: $cmd)"
        return 1
    fi
}

# Assert command fails
# Usage: assert_fails "command" "message"
assert_fails() {
    local cmd="$1"
    local message="$2"

    if ! eval "$cmd" >/dev/null 2>&1; then
        log_debug "Assert passed: $message (command correctly failed)"
        return 0
    else
        log_error "Assert failed: $message (command should have failed: $cmd)"
        return 1
    fi
}

# ============================================================================
# Data Verification
# ============================================================================

# Create test table
# Usage: create_test_table "table_name" ["host:port"]
create_test_table() {
    local table="$1"
    local endpoint="${2:-$PRIMARY_PG}"

    exec_sql_check "$endpoint" "CREATE TABLE IF NOT EXISTS $table (
        id INTEGER PRIMARY KEY,
        value TEXT,
        counter INTEGER DEFAULT 0
    )"
}

# Drop test table
# Usage: drop_test_table "table_name" ["host:port"]
drop_test_table() {
    local table="$1"
    local endpoint="${2:-$PRIMARY_PG}"

    exec_sql "$endpoint" "DROP TABLE IF EXISTS $table" 2>/dev/null
}

# Insert test data
# Usage: insert_test_data "table_name" "id" "value" ["host:port"]
insert_test_data() {
    local table="$1"
    local id="$2"
    local value="$3"
    local endpoint="${4:-$PRIMARY_PG}"

    exec_sql_check "$endpoint" "INSERT INTO $table (id, value) VALUES ($id, '$value')"
}

# Verify data exists
# Usage: verify_data_exists "table_name" "id" "expected_value" ["host:port"]
verify_data_exists() {
    local table="$1"
    local id="$2"
    local expected="$3"
    local endpoint="${4:-$PRIMARY_PG}"

    local actual
    actual=$(exec_sql "$endpoint" "SELECT value FROM $table WHERE id = $id" 2>/dev/null | tr -d ' \n')

    if [[ "$actual" == "$expected" ]]; then
        return 0
    else
        log_debug "Data verification failed: expected='$expected', actual='$actual'"
        return 1
    fi
}

# Wait for data to be replicated to standby
# Usage: wait_for_data_replicated "table_name" "id" "expected_value" "standby_endpoint" [max_seconds]
wait_for_data_replicated() {
    local table="$1"
    local id="$2"
    local expected="$3"
    local endpoint="$4"
    local max_wait="${5:-10}"
    local waited=0

    while [[ $waited -lt $max_wait ]]; do
        if verify_data_exists "$table" "$id" "$expected" "$endpoint" 2>/dev/null; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    log_debug "Data not replicated after ${max_wait}s"
    return 1
}

# Wait for row count to match on standby
# Usage: wait_for_row_count "table_name" "expected_count" "standby_endpoint" [max_seconds]
wait_for_row_count() {
    local table="$1"
    local expected="$2"
    local endpoint="$3"
    local max_wait="${4:-10}"
    local waited=0

    while [[ $waited -lt $max_wait ]]; do
        local actual
        actual=$(count_rows "$table" "$endpoint" 2>/dev/null)
        if [[ "$actual" == "$expected" ]]; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    log_debug "Row count mismatch after ${max_wait}s (expected=$expected, actual=$actual)"
    return 1
}

# Wait for table to be replicated to standby
# Usage: wait_for_table_replicated "table_name" "standby_endpoint" [max_seconds]
wait_for_table_replicated() {
    local table="$1"
    local endpoint="$2"
    local max_wait="${3:-10}"
    local waited=0

    while [[ $waited -lt $max_wait ]]; do
        local result
        result=$(exec_sql "$endpoint" "SELECT 1 FROM $table LIMIT 1" 2>&1)
        if [[ ! "$result" =~ ERROR ]] && [[ ! "$result" =~ "does not exist" ]]; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    return 1
}

# Wait for minimum row count on standby (for async replication verification)
# Usage: wait_for_min_row_count "table_name" "min_count" "standby_endpoint" [max_seconds]
wait_for_min_row_count() {
    local table="$1"
    local min_count="$2"
    local endpoint="$3"
    local max_wait="${4:-10}"
    local waited=0

    while [[ $waited -lt $max_wait ]]; do
        local actual
        actual=$(count_rows "$table" "$endpoint")
        if [[ "$actual" -ge "$min_count" ]]; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    return 1
}

# Count rows in table (returns 0 on error)
# Usage: count_rows "table_name" ["host:port"]
count_rows() {
    local table="$1"
    local endpoint="${2:-$PRIMARY_PG}"

    local result
    result=$(exec_sql "$endpoint" "SELECT COUNT(*) FROM $table" 2>/dev/null | tr -d ' \n')

    # Return 0 if result is empty or contains error
    if [[ -z "$result" ]] || [[ "$result" =~ ERROR ]] || [[ ! "$result" =~ ^[0-9]+$ ]]; then
        echo "0"
    else
        echo "$result"
    fi
}

# ============================================================================
# Timing Utilities
# ============================================================================

# Measure command execution time
# Usage: time_cmd "command" -> outputs milliseconds
time_cmd() {
    local cmd="$1"
    local start=$(date +%s%3N)
    eval "$cmd"
    local status=$?
    local end=$(date +%s%3N)
    echo $((end - start))
    return $status
}

# Wait with timeout
# Usage: wait_until "condition_command" [max_seconds]
wait_until() {
    local condition="$1"
    local max_wait="${2:-30}"
    local waited=0

    while [[ $waited -lt $max_wait ]]; do
        if eval "$condition" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    return 1
}

# ============================================================================
# Reporting
# ============================================================================

# Print test summary
print_summary() {
    echo ""
    echo -e "${CYAN}╔══════════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║                         TEST EXECUTION SUMMARY                           ║${NC}"
    echo -e "${CYAN}╠══════════════════════════════════════════════════════════════════════════╣${NC}"
    printf "${CYAN}║${NC}  Total Tests:    %-58s${CYAN}║${NC}\n" "$TESTS_TOTAL"
    printf "${CYAN}║${NC}  ${GREEN}Passed:${NC}         %-58s${CYAN}║${NC}\n" "$TESTS_PASSED"
    printf "${CYAN}║${NC}  ${RED}Failed:${NC}         %-58s${CYAN}║${NC}\n" "$TESTS_FAILED"
    printf "${CYAN}║${NC}  ${YELLOW}Skipped:${NC}        %-58s${CYAN}║${NC}\n" "$TESTS_SKIPPED"
    echo -e "${CYAN}╠══════════════════════════════════════════════════════════════════════════╣${NC}"

    local pass_rate=0
    if [[ $TESTS_TOTAL -gt 0 ]]; then
        pass_rate=$(( (TESTS_PASSED * 100) / TESTS_TOTAL ))
    fi
    printf "${CYAN}║${NC}  Pass Rate:      %-58s${CYAN}║${NC}\n" "${pass_rate}%"
    echo -e "${CYAN}╠══════════════════════════════════════════════════════════════════════════╣${NC}"
    printf "${CYAN}║${NC}  Log Directory:  %-58s${CYAN}║${NC}\n" "$LOG_DIR"
    printf "${CYAN}║${NC}  Results File:   %-58s${CYAN}║${NC}\n" "$RESULTS_FILE"
    echo -e "${CYAN}╚══════════════════════════════════════════════════════════════════════════╝${NC}"

    # Update JSON summary
    local temp_file=$(mktemp)
    jq --arg total "$TESTS_TOTAL" \
       --arg passed "$TESTS_PASSED" \
       --arg failed "$TESTS_FAILED" \
       --arg skipped "$TESTS_SKIPPED" \
       --arg rate "$pass_rate" \
       '.summary = {"total": ($total | tonumber), "passed": ($passed | tonumber), "failed": ($failed | tonumber), "skipped": ($skipped | tonumber), "pass_rate": ($rate | tonumber)}' \
       "$RESULTS_FILE" > "$temp_file" && mv "$temp_file" "$RESULTS_FILE"

    if [[ $TESTS_FAILED -gt 0 ]]; then
        return 1
    fi
    return 0
}

# Print failed tests
print_failures() {
    if [[ $TESTS_FAILED -gt 0 ]]; then
        echo ""
        echo -e "${RED}Failed Tests:${NC}"
        jq -r '.tests[] | select(.status == "FAILED") | "  - \(.id): \(.message)"' "$RESULTS_FILE"
    fi
}

# ============================================================================
# Cleanup
# ============================================================================

# Reset cluster to clean state
reset_cluster() {
    log_info "Resetting cluster to clean state..."

    # Restart all containers
    for container in "$PRIMARY_CONTAINER" "$STANDBY_SYNC_CONTAINER" "$STANDBY_SEMISYNC_CONTAINER" "$STANDBY_ASYNC_CONTAINER" "$OBSERVER_CONTAINER" "$PROXY_CONTAINER"; do
        docker start "$container" 2>/dev/null || true
        unpause_container "$container" 2>/dev/null || true
        restore_container_network "$container" 2>/dev/null || true
    done

    sleep 5
    wait_for_cluster 120
}

# Cleanup test artifacts
cleanup_tests() {
    log_info "Cleaning up test artifacts..."

    # Drop test tables
    for table in test_switchover test_failover test_durability test_tr test_consistency test_load; do
        drop_test_table "$table" 2>/dev/null
    done
}
