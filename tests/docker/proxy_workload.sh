#!/bin/bash
# Proxy Workload Simulator for TWR/TR Testing
# Runs continuous queries through the proxy's HTTP SQL API

PROXY_ADMIN="http://localhost:19090"
LOG_FILE="${LOG_FILE:-/tmp/proxy_workload.log}"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
NC='\033[0m'

log() {
    local timestamp=$(date '+%Y-%m-%d %H:%M:%S.%3N')
    echo -e "[$timestamp] $1" | tee -a "$LOG_FILE"
}

# Initialize test table
init_test_data() {
    log "${CYAN}[INIT]${NC} Creating test table via proxy..."

    # Create table
    local result=$(curl -sf -X POST "$PROXY_ADMIN/api/sql" \
        -H "Content-Type: application/json" \
        -d '{"query": "CREATE TABLE IF NOT EXISTS workload_test (id INTEGER PRIMARY KEY, value TEXT, counter INTEGER, updated_at TEXT, written_by TEXT)"}' 2>&1)

    if echo "$result" | grep -q "error"; then
        log "${RED}[INIT]${NC} Failed to create table: $result"
        return 1
    fi

    # Clear existing data
    curl -sf -X POST "$PROXY_ADMIN/api/sql" \
        -H "Content-Type: application/json" \
        -d '{"query": "DELETE FROM workload_test"}' >/dev/null 2>&1

    # Insert initial rows
    for i in $(seq 1 10); do
        curl -sf -X POST "$PROXY_ADMIN/api/sql" \
            -H "Content-Type: application/json" \
            -d "{\"query\": \"INSERT INTO workload_test VALUES ($i, 'initial', 0, datetime('now'), 'init')\"}" >/dev/null 2>&1
    done

    log "${GREEN}[INIT]${NC} Test data initialized with 10 rows"
}

# Run a single query iteration
run_query_iteration() {
    local iteration=$1
    local start_time=$(date +%s%3N)

    # SELECT query (read - should go to standby)
    local select_result=$(curl -sf -X POST "$PROXY_ADMIN/api/sql" \
        -H "Content-Type: application/json" \
        -d '{"query": "SELECT COUNT(*) as cnt FROM workload_test"}' 2>&1)
    local select_status=$?
    local select_routed=$(echo "$select_result" | jq -r '.routed_to // "error"' 2>/dev/null)
    local select_role=$(echo "$select_result" | jq -r '.node_role // "error"' 2>/dev/null)

    # UPDATE query (write - should go to primary)
    local update_id=$((($iteration % 10) + 1))
    local update_result=$(curl -sf -X POST "$PROXY_ADMIN/api/sql" \
        -H "Content-Type: application/json" \
        -d "{\"query\": \"UPDATE workload_test SET value = 'iter_${iteration}', counter = counter + 1, updated_at = datetime('now'), written_by = 'workload' WHERE id = $update_id\"}" 2>&1)
    local update_status=$?
    local update_routed=$(echo "$update_result" | jq -r '.routed_to // "error"' 2>/dev/null)
    local update_role=$(echo "$update_result" | jq -r '.node_role // "error"' 2>/dev/null)

    # INSERT query (write - should go to primary)
    local insert_id=$((1000 + $iteration))
    local insert_result=$(curl -sf -X POST "$PROXY_ADMIN/api/sql" \
        -H "Content-Type: application/json" \
        -d "{\"query\": \"INSERT OR REPLACE INTO workload_test VALUES ($insert_id, 'new_${iteration}', 1, datetime('now'), 'workload')\"}" 2>&1)
    local insert_status=$?
    local insert_routed=$(echo "$insert_result" | jq -r '.routed_to // "error"' 2>/dev/null)
    local insert_role=$(echo "$insert_result" | jq -r '.node_role // "error"' 2>/dev/null)

    local end_time=$(date +%s%3N)
    local duration=$((end_time - start_time))

    # Check for errors
    local select_error=$(echo "$select_result" | jq -r '.error // empty' 2>/dev/null)
    local update_error=$(echo "$update_result" | jq -r '.error // empty' 2>/dev/null)
    local insert_error=$(echo "$insert_result" | jq -r '.error // empty' 2>/dev/null)

    if [ -z "$select_error" ] && [ -z "$update_error" ] && [ -z "$insert_error" ]; then
        log "${GREEN}[OK]${NC} #$iteration: SELECT->$select_routed($select_role) UPDATE->$update_routed($update_role) INSERT->$insert_routed($insert_role) [${duration}ms]"
    else
        if [ -n "$select_error" ]; then
            log "${RED}[FAIL]${NC} #$iteration SELECT: $select_error"
        fi
        if [ -n "$update_error" ]; then
            log "${RED}[FAIL]${NC} #$iteration UPDATE: $update_error"
        fi
        if [ -n "$insert_error" ]; then
            log "${RED}[FAIL]${NC} #$iteration INSERT: $insert_error"
        fi
    fi
}

# Show current node status
show_status() {
    log "${CYAN}[STATUS]${NC} Current node health:"
    curl -sf "$PROXY_ADMIN/nodes" 2>/dev/null | jq -r '.[] | "  \(.address): healthy=\(.healthy) failures=\(.failure_count)"' | while read line; do
        log "  $line"
    done
}

# Main workload loop
run_workload() {
    local duration=${1:-300}  # Default 5 minutes
    local interval=${2:-1}    # Default 1 second between iterations
    local end_time=$(($(date +%s) + duration))
    local iteration=0

    log "${CYAN}[START]${NC} Starting proxy workload for ${duration}s with ${interval}s interval"
    show_status

    while [ $(date +%s) -lt $end_time ]; do
        iteration=$((iteration + 1))
        run_query_iteration $iteration
        sleep $interval
    done

    log "${CYAN}[END]${NC} Workload completed after $iteration iterations"
    show_status
}

case "${1:-workload}" in
    init)
        init_test_data
        ;;
    workload)
        run_workload "${2:-300}" "${3:-1}"
        ;;
    status)
        show_status
        ;;
    single)
        run_query_iteration 1
        ;;
    *)
        echo "Usage: $0 {init|workload [duration_sec] [interval_sec]|status|single}"
        ;;
esac
