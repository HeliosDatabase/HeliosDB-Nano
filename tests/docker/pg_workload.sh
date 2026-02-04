#!/bin/bash
# PostgreSQL Protocol Workload Test for HeliosProxy
# Tests write timeout during failover

set -e

PROXY_HOST="${PROXY_HOST:-localhost}"
PROXY_PORT="${PROXY_PORT:-15400}"
ADMIN_PORT="${ADMIN_PORT:-19090}"
DURATION="${DURATION:-300}"
INTERVAL="${INTERVAL:-1}"
PRIMARY_HTTP="${PRIMARY_HTTP:-http://localhost:18080}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

timestamp() {
    date "+%Y-%m-%d %H:%M:%S.%3N"
}

log_ok() {
    echo -e "[$(timestamp)] ${GREEN}[OK]${NC} $1"
}

log_fail() {
    echo -e "[$(timestamp)] ${RED}[FAIL]${NC} $1"
}

log_info() {
    echo -e "[$(timestamp)] ${CYAN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "[$(timestamp)] ${YELLOW}[WARN]${NC} $1"
}

# Create test table through proxy
setup_test_table() {
    log_info "Setting up test table..."
    PGPASSWORD=helios psql -h "$PROXY_HOST" -p "$PROXY_PORT" -U helios -d heliosdb -q << 'EOF'
DROP TABLE IF EXISTS pg_workload_test;
CREATE TABLE pg_workload_test (
    id INTEGER,
    value TEXT,
    updated_at TEXT
);
INSERT INTO pg_workload_test VALUES (1, 'initial', '2024-01-01');
EOF
    log_ok "Test table created"
}

# Get node health from proxy admin API
get_node_health() {
    curl -sf "http://${PROXY_HOST}:${ADMIN_PORT}/nodes" 2>/dev/null || echo "[]"
}

# Run a single query iteration through psql
run_pg_iteration() {
    local iteration=$1
    local start_time=$(date +%s%3N)

    # Run SELECT (read query)
    local select_result=$(PGPASSWORD=helios psql -h "$PROXY_HOST" -p "$PROXY_PORT" -U helios -d heliosdb -t -c "SELECT value FROM pg_workload_test WHERE id = 1" 2>&1)
    local select_status=$?

    # Run UPDATE (write query)
    local update_result=$(PGPASSWORD=helios psql -h "$PROXY_HOST" -p "$PROXY_PORT" -U helios -d heliosdb -t -c "UPDATE pg_workload_test SET value = 'iter_$iteration', updated_at = '$(date -Iseconds)' WHERE id = 1" 2>&1)
    local update_status=$?

    local end_time=$(date +%s%3N)
    local start_ms=$(echo "$start_time" | sed 's/\.//')
    local duration_ms=$((end_time - start_ms))

    if [ $select_status -eq 0 ] && [ $update_status -eq 0 ]; then
        log_ok "#$iteration: SELECT=[ok] UPDATE=[ok] [${duration_ms}ms]"
        return 0
    else
        if [ $select_status -ne 0 ]; then
            log_fail "#$iteration: SELECT failed: $select_result"
        fi
        if [ $update_status -ne 0 ]; then
            log_fail "#$iteration: UPDATE failed: $update_result [${duration_ms}ms]"
        fi
        return 1
    fi
}

# Main workload loop
run_workload() {
    local total_iterations=0
    local successful=0
    local failed=0
    local start_time=$(date +%s)
    local end_time=$((start_time + DURATION))

    log_info "Starting PostgreSQL protocol workload for ${DURATION}s with ${INTERVAL}s interval"
    log_info "Proxy: $PROXY_HOST:$PROXY_PORT"

    # Print initial node health
    log_info "Current node health:"
    get_node_health | jq -r '.[] | "  \(.address): healthy=\(.healthy) failures=\(.failure_count)"' 2>/dev/null || echo "  Unable to get health status"

    while [ $(date +%s) -lt $end_time ]; do
        total_iterations=$((total_iterations + 1))

        if run_pg_iteration $total_iterations; then
            successful=$((successful + 1))
        else
            failed=$((failed + 1))
        fi

        sleep $INTERVAL
    done

    # Summary
    echo ""
    log_info "=== Workload Summary ==="
    log_info "Total iterations: $total_iterations"
    log_info "Successful: $successful"
    log_info "Failed: $failed"
    local success_rate=$((successful * 100 / total_iterations))
    log_info "Success rate: ${success_rate}%"
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --duration)
            DURATION="$2"
            shift 2
            ;;
        --interval)
            INTERVAL="$2"
            shift 2
            ;;
        --host)
            PROXY_HOST="$2"
            shift 2
            ;;
        --port)
            PROXY_PORT="$2"
            shift 2
            ;;
        --admin-port)
            ADMIN_PORT="$2"
            shift 2
            ;;
        --setup)
            setup_test_table
            exit 0
            ;;
        --help)
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  --duration N      Run workload for N seconds (default: 300)"
            echo "  --interval N      Interval between iterations in seconds (default: 1)"
            echo "  --host HOST       Proxy host (default: localhost)"
            echo "  --port PORT       Proxy PostgreSQL port (default: 15400)"
            echo "  --admin-port PORT Proxy admin port (default: 19090)"
            echo "  --setup           Setup test table and exit"
            echo "  --help            Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Run setup first, then workload
setup_test_table
run_workload
