#!/bin/bash
# Workload Simulator for TWR/TR Testing
# Runs continuous queries and DML through the proxy

PROXY_HOST="${PROXY_HOST:-localhost}"
PROXY_PORT="${PROXY_PORT:-15400}"
PROXY_ADMIN="${PROXY_ADMIN:-19090}"
LOG_FILE="${LOG_FILE:-/tmp/workload_test.log}"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log() {
    local timestamp=$(date '+%Y-%m-%d %H:%M:%S.%3N')
    echo -e "[$timestamp] $1" | tee -a "$LOG_FILE"
}

# Initialize test table via HTTP API
init_test_data() {
    log "${CYAN}[INIT]${NC} Creating test table and initial data..."
    
    # Use primary's HTTP API to create table
    curl -sf -X POST "http://localhost:18080/api/sql" \
        -H "Content-Type: application/json" \
        -d '{"query": "CREATE TABLE IF NOT EXISTS switchover_test (id INTEGER PRIMARY KEY, value TEXT, updated_at TEXT, node_info TEXT)"}' 2>/dev/null
    
    curl -sf -X POST "http://localhost:18080/api/sql" \
        -H "Content-Type: application/json" \
        -d '{"query": "DELETE FROM switchover_test"}' 2>/dev/null
    
    # Insert initial rows
    for i in $(seq 1 10); do
        curl -sf -X POST "http://localhost:18080/api/sql" \
            -H "Content-Type: application/json" \
            -d "{\"query\": \"INSERT INTO switchover_test VALUES ($i, 'initial_$i', datetime('now'), 'init')\"}" 2>/dev/null
    done
    
    log "${GREEN}[INIT]${NC} Test data initialized"
}

# Run a single query iteration
run_query_iteration() {
    local iteration=$1
    local start_time=$(date +%s%3N)
    
    # SELECT query
    local select_result=$(curl -sf -X POST "http://localhost:18080/api/sql" \
        -H "Content-Type: application/json" \
        -d '{"query": "SELECT COUNT(*) as cnt FROM switchover_test"}' 2>&1)
    local select_status=$?
    
    # UPDATE query (DML)
    local update_id=$((($iteration % 10) + 1))
    local update_result=$(curl -sf -X POST "http://localhost:18080/api/sql" \
        -H "Content-Type: application/json" \
        -d "{\"query\": \"UPDATE switchover_test SET value = 'iter_${iteration}', updated_at = datetime('now') WHERE id = $update_id\"}" 2>&1)
    local update_status=$?
    
    # INSERT query
    local insert_id=$((100 + $iteration))
    local insert_result=$(curl -sf -X POST "http://localhost:18080/api/sql" \
        -H "Content-Type: application/json" \
        -d "{\"query\": \"INSERT OR REPLACE INTO switchover_test VALUES ($insert_id, 'new_${iteration}', datetime('now'), 'workload')\"}" 2>&1)
    local insert_status=$?
    
    local end_time=$(date +%s%3N)
    local duration=$((end_time - start_time))
    
    # Get proxy status
    local proxy_nodes=$(curl -sf "http://localhost:$PROXY_ADMIN/nodes" 2>/dev/null | jq -c '[.[] | select(.healthy==true) | .address]' 2>/dev/null || echo "[]")
    
    if [ $select_status -eq 0 ] && [ $update_status -eq 0 ] && [ $insert_status -eq 0 ]; then
        log "${GREEN}[OK]${NC} Iteration $iteration: SELECT+UPDATE+INSERT in ${duration}ms | Healthy nodes: $proxy_nodes"
    else
        log "${RED}[FAIL]${NC} Iteration $iteration: SELECT=$select_status UPDATE=$update_status INSERT=$insert_status | Duration: ${duration}ms"
        log "${YELLOW}[DEBUG]${NC} Select: $select_result"
        log "${YELLOW}[DEBUG]${NC} Update: $update_result"
    fi
}

# Main workload loop
run_workload() {
    local duration=${1:-300}  # Default 5 minutes
    local interval=${2:-1}    # Default 1 second between iterations
    local end_time=$(($(date +%s) + duration))
    local iteration=0
    
    log "${CYAN}[START]${NC} Starting workload simulation for ${duration}s with ${interval}s interval"
    
    while [ $(date +%s) -lt $end_time ]; do
        iteration=$((iteration + 1))
        run_query_iteration $iteration
        sleep $interval
    done
    
    log "${CYAN}[END]${NC} Workload completed after $iteration iterations"
}

# Show current proxy routing status
show_proxy_status() {
    log "${CYAN}[PROXY]${NC} Current proxy status:"
    curl -sf "http://localhost:$PROXY_ADMIN/nodes" 2>/dev/null | jq '.' || echo "Proxy unavailable"
    curl -sf "http://localhost:$PROXY_ADMIN/metrics" 2>/dev/null | jq '.' || echo "Metrics unavailable"
}

case "${1:-workload}" in
    init)
        init_test_data
        ;;
    workload)
        run_workload "${2:-300}" "${3:-1}"
        ;;
    status)
        show_proxy_status
        ;;
    *)
        echo "Usage: $0 {init|workload [duration_sec] [interval_sec]|status}"
        ;;
esac
