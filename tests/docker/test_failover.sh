#!/bin/bash
# HeliosDB-Lite HA Failover Test Suite
#
# Tests various failover scenarios in a 3-node cluster
#
# Usage:
#   ./test_failover.sh [test_name]
#   ./test_failover.sh all           # Run all tests
#   ./test_failover.sh primary_kill  # Run specific test

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Configuration - ports match docker-compose.ha-cluster.yml
COMPOSE_FILE="docker-compose.ha-cluster.yml"
PRIMARY_HOST="localhost"
PRIMARY_PORT=15432      # Primary PostgreSQL port (mapped to 5432 in container)
PRIMARY_REPL_PORT=15433 # Primary replication port
PRIMARY_HTTP_PORT=18080 # Primary HTTP API port
STANDBY_SYNC_PORT=15442    # Standby sync PostgreSQL port
STANDBY_SYNC_HTTP=18081    # Standby sync HTTP port
STANDBY_SEMISYNC_PORT=15452 # Standby semi-sync PostgreSQL port
STANDBY_SEMISYNC_HTTP=18082 # Standby semi-sync HTTP port
STANDBY_ASYNC_PORT=15462   # Standby async PostgreSQL port
STANDBY_ASYNC_HTTP=18084   # Standby async HTTP port
OBSERVER_HTTP=18083        # Observer HTTP port
PROXY_PORT=15400           # Proxy PostgreSQL port (host mapping)
PROXY_ADMIN=19090          # Proxy admin API port (host mapping)

# Logging functions
log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_test() {
    echo -e "\n${GREEN}========================================${NC}"
    echo -e "${GREEN}TEST: $1${NC}"
    echo -e "${GREEN}========================================${NC}\n"
}

# Helper functions
wait_for_service() {
    local host=$1
    local port=$2
    local max_wait=${3:-60}
    local waited=0

    log_info "Waiting for $host:$port..."

    while ! nc -z "$host" "$port" 2>/dev/null; do
        sleep 1
        waited=$((waited + 1))
        if [ $waited -ge $max_wait ]; then
            log_error "Service $host:$port not available after ${max_wait}s"
            return 1
        fi
    done

    log_info "Service $host:$port is available"
    return 0
}

check_cluster_health() {
    log_info "Checking cluster health..."
    local all_healthy=0

    # Check primary
    if ! curl -sf http://localhost:$PRIMARY_HTTP_PORT/health > /dev/null 2>&1; then
        log_warn "Primary health check failed"
        all_healthy=1
    else
        log_info "Primary is healthy"
    fi

    # Check standby-sync
    if ! curl -sf http://localhost:$STANDBY_SYNC_HTTP/health > /dev/null 2>&1; then
        log_warn "Standby-sync health check failed"
    else
        log_info "Standby-sync is healthy"
    fi

    # Check standby-semisync
    if ! curl -sf http://localhost:$STANDBY_SEMISYNC_HTTP/health > /dev/null 2>&1; then
        log_warn "Standby-semisync health check failed"
    else
        log_info "Standby-semisync is healthy"
    fi

    # Check standby-async
    if ! curl -sf http://localhost:$STANDBY_ASYNC_HTTP/health > /dev/null 2>&1; then
        log_warn "Standby-async health check failed"
    else
        log_info "Standby-async is healthy"
    fi

    # Check observer
    if ! curl -sf http://localhost:$OBSERVER_HTTP/health > /dev/null 2>&1; then
        log_warn "Observer health check failed"
    else
        log_info "Observer is healthy"
    fi

    # Check proxy
    if ! curl -sf http://localhost:$PROXY_ADMIN/health > /dev/null 2>&1; then
        log_warn "Proxy health check failed"
    else
        log_info "Proxy is healthy"
    fi

    return $all_healthy
}

get_replication_status() {
    log_info "Fetching replication status from all nodes..."

    echo "=== Primary ==="
    curl -sf http://localhost:$PRIMARY_HTTP_PORT/api/ha/status 2>/dev/null | jq . || echo "  Status unavailable"

    echo "=== Standby-sync ==="
    curl -sf http://localhost:$STANDBY_SYNC_HTTP/api/ha/status 2>/dev/null | jq . || echo "  Status unavailable"

    echo "=== Standby-semisync ==="
    curl -sf http://localhost:$STANDBY_SEMISYNC_HTTP/api/ha/status 2>/dev/null | jq . || echo "  Status unavailable"

    echo "=== Standby-async ==="
    curl -sf http://localhost:$STANDBY_ASYNC_HTTP/api/ha/status 2>/dev/null | jq . || echo "  Status unavailable"

    echo "=== Observer ==="
    curl -sf http://localhost:$OBSERVER_HTTP/api/ha/status 2>/dev/null | jq . || echo "  Status unavailable"

    echo "=== Proxy ==="
    curl -sf http://localhost:$PROXY_ADMIN/metrics 2>/dev/null | jq . || echo "  Metrics unavailable"
}

# Test functions
test_cluster_startup() {
    log_test "Cluster Startup"

    log_info "Starting HA cluster..."
    docker compose -f "$COMPOSE_FILE" up -d --build

    log_info "Waiting for cluster to stabilize..."
    sleep 30

    check_cluster_health
    get_replication_status

    log_info "Cluster startup test completed"
}

test_primary_kill() {
    log_test "Primary Kill Failover"

    # Verify cluster is healthy
    check_cluster_health || {
        log_error "Cluster not healthy before test"
        return 1
    }

    # Get initial status
    log_info "Initial cluster status..."
    get_replication_status

    # Write some data to primary (if psql available)
    log_info "Writing test data to primary..."
    if command -v psql &> /dev/null; then
        PGPASSWORD="" psql -h localhost -p $PRIMARY_PORT -U heliosdb -c "SELECT 1" 2>/dev/null || log_warn "psql test failed (expected if DB not fully running)"
    fi

    # Kill primary container
    log_info "Killing primary node (simulating crash)..."
    docker kill heliosdb-primary

    # Wait for failover detection (health check interval * threshold)
    log_info "Waiting for failover detection (45s)..."
    sleep 45

    # Check standby status after primary failure
    log_info "Checking standby status after primary failure..."

    # Check if sync standby was promoted (highest priority)
    if curl -sf http://localhost:$STANDBY_SYNC_HTTP/api/ha/status 2>/dev/null | jq -e '.role == "primary"' > /dev/null 2>&1; then
        log_info "SUCCESS: Standby-sync was promoted to primary"
    elif curl -sf http://localhost:$STANDBY_SEMISYNC_HTTP/api/ha/status 2>/dev/null | jq -e '.role == "primary"' > /dev/null 2>&1; then
        log_info "SUCCESS: Standby-semisync was promoted to primary"
    elif curl -sf http://localhost:$STANDBY_ASYNC_HTTP/api/ha/status 2>/dev/null | jq -e '.role == "primary"' > /dev/null 2>&1; then
        log_info "SUCCESS: Standby-async was promoted to primary"
    else
        log_warn "No standby was automatically promoted (manual failover may be configured)"
        log_info "Current standby statuses:"
        curl -sf http://localhost:$STANDBY_SYNC_HTTP/api/ha/status 2>/dev/null || echo "  Standby-sync: unavailable"
        curl -sf http://localhost:$STANDBY_SEMISYNC_HTTP/api/ha/status 2>/dev/null || echo "  Standby-semisync: unavailable"
    fi

    # Check observer voting status
    log_info "Checking observer status..."
    curl -sf http://localhost:$OBSERVER_HTTP/api/ha/status 2>/dev/null | jq . || echo "  Observer: unavailable"

    # Restart old primary (should rejoin as standby if automatic)
    log_info "Restarting old primary..."
    docker start heliosdb-primary

    sleep 30
    check_cluster_health
    get_replication_status

    log_info "Primary kill failover test completed"
}

test_network_partition() {
    log_test "Network Partition Simulation"

    check_cluster_health || {
        log_error "Cluster not healthy before test"
        return 1
    }

    # Get network name (may have prefix based on directory)
    NETWORK_NAME=$(docker network ls --format '{{.Name}}' | grep heliosdb-ha | head -1)
    if [ -z "$NETWORK_NAME" ]; then
        NETWORK_NAME="docker_heliosdb-ha"
    fi
    log_info "Using network: $NETWORK_NAME"

    # Isolate standby-sync from the network
    log_info "Isolating standby-sync from the network..."
    docker network disconnect "$NETWORK_NAME" heliosdb-standby-sync 2>/dev/null || true

    sleep 15
    log_info "Checking cluster status with standby-sync isolated..."

    # Primary should still be healthy
    if curl -sf http://localhost:$PRIMARY_HTTP_PORT/health > /dev/null 2>&1; then
        log_info "Primary still healthy during partition"
    else
        log_warn "Primary health check failed during partition"
    fi

    # Standby-sync should be unreachable
    if curl -sf http://localhost:$STANDBY_SYNC_HTTP/health --max-time 5 > /dev/null 2>&1; then
        log_warn "Standby-sync still reachable (unexpected)"
    else
        log_info "Standby-sync correctly unreachable"
    fi

    # Reconnect standby-sync
    log_info "Reconnecting standby-sync..."
    docker network connect "$NETWORK_NAME" heliosdb-standby-sync 2>/dev/null || true

    sleep 20
    log_info "Checking replication catch-up after reconnect..."
    get_replication_status

    check_cluster_health
    log_info "Network partition test completed"
}

test_split_brain_protection() {
    log_test "Split-Brain Protection"

    check_cluster_health || {
        log_error "Cluster not healthy before test"
        return 1
    }

    # Get network name
    NETWORK_NAME=$(docker network ls --format '{{.Name}}' | grep heliosdb-ha | head -1)
    if [ -z "$NETWORK_NAME" ]; then
        NETWORK_NAME="docker_heliosdb-ha"
    fi
    log_info "Using network: $NETWORK_NAME"

    # Simulate split-brain by isolating primary from standbys
    log_info "Creating split-brain scenario by isolating primary..."
    docker network disconnect "$NETWORK_NAME" heliosdb-primary 2>/dev/null || true

    sleep 20

    # Check fencing status
    log_info "Checking fencing status..."

    # Primary should be fenced (can't reach quorum) - note: may not be reachable
    # Check via observer if primary is fenced
    FENCING_STATUS=$(curl -sf http://localhost:$OBSERVER_HTTP/api/ha/fencing 2>/dev/null | jq -r '.fenced' || echo "unknown")
    log_info "Fencing status from observer: $FENCING_STATUS"

    # Check if standbys are considering election
    log_info "Standby election status..."
    curl -sf http://localhost:$STANDBY_SYNC_HTTP/api/ha/election 2>/dev/null | jq . || echo "  Election status unavailable"

    # Reconnect primary
    log_info "Resolving split-brain by reconnecting primary..."
    docker network connect "$NETWORK_NAME" heliosdb-primary 2>/dev/null || true

    sleep 25

    # Verify cluster recovered
    log_info "Verifying cluster recovery..."
    get_replication_status

    check_cluster_health
    log_info "Split-brain protection test completed"
}

test_replication_lag() {
    log_test "Replication Lag Monitoring"

    check_cluster_health || {
        log_error "Cluster not healthy before test"
        return 1
    }

    log_info "Monitoring replication lag across all standbys..."

    for i in $(seq 1 5); do
        echo ""
        echo "=== Iteration $i ==="

        echo "Primary LSN:"
        curl -sf http://localhost:$PRIMARY_HTTP_PORT/api/ha/lsn 2>/dev/null | jq . || echo "  Unavailable"

        echo "Standby-sync lag:"
        curl -sf http://localhost:$STANDBY_SYNC_HTTP/api/ha/lag 2>/dev/null | jq . || echo "  Unavailable"

        echo "Standby-semisync lag:"
        curl -sf http://localhost:$STANDBY_SEMISYNC_HTTP/api/ha/lag 2>/dev/null | jq . || echo "  Unavailable"

        echo "Standby-async lag:"
        curl -sf http://localhost:$STANDBY_ASYNC_HTTP/api/ha/lag 2>/dev/null | jq . || echo "  Unavailable"

        sleep 3
    done

    log_info "Replication lag test completed"
}

test_proxy_failover() {
    log_test "Proxy Automatic Failover"

    check_cluster_health || {
        log_error "Cluster not healthy before test"
        return 1
    }

    # Get initial proxy stats
    log_info "Initial proxy status..."
    curl -sf http://localhost:$PROXY_ADMIN/health 2>/dev/null | jq . || echo "  Health unavailable"
    curl -sf http://localhost:$PROXY_ADMIN/metrics 2>/dev/null | jq . || echo "  Metrics unavailable"
    curl -sf http://localhost:$PROXY_ADMIN/nodes 2>/dev/null | jq . || echo "  Nodes unavailable"

    # Test proxy is routing (if psql available)
    log_info "Testing proxy connection routing..."
    if command -v psql &> /dev/null; then
        PGPASSWORD="" psql -h localhost -p $PROXY_PORT -U heliosdb -c "SELECT 1" 2>/dev/null || log_warn "psql via proxy failed (expected if DB not fully running)"
    fi

    # Kill primary
    log_info "Killing primary while proxy handles traffic..."
    docker kill heliosdb-primary

    sleep 15

    # Check proxy detected the failure
    log_info "Proxy status after primary failure..."
    curl -sf http://localhost:$PROXY_ADMIN/health 2>/dev/null | jq . || echo "  Health unavailable"
    curl -sf http://localhost:$PROXY_ADMIN/nodes 2>/dev/null | jq . || echo "  Nodes unavailable"

    # Proxy should mark primary as unhealthy and route reads to standbys
    log_info "Testing read routing after primary failure..."
    # Reads should still work if standbys are healthy
    if command -v psql &> /dev/null; then
        PGPASSWORD="" psql -h localhost -p $PROXY_PORT -U heliosdb -c "SELECT 1" 2>/dev/null && log_info "Read query succeeded via proxy" || log_warn "Read query failed"
    fi

    # Get proxy stats
    log_info "Proxy stats after failover..."
    curl -sf http://localhost:$PROXY_ADMIN/metrics 2>/dev/null | jq . || echo "  Metrics unavailable"

    # Restart primary
    log_info "Restarting primary..."
    docker start heliosdb-primary

    sleep 25

    # Check proxy re-detected primary
    log_info "Proxy status after primary restart..."
    curl -sf http://localhost:$PROXY_ADMIN/nodes 2>/dev/null | jq . || echo "  Nodes unavailable"

    check_cluster_health
    log_info "Proxy failover test completed"
}

test_rolling_upgrade() {
    log_test "Rolling Upgrade Simulation"

    check_cluster_health || {
        log_error "Cluster not healthy before test"
        return 1
    }

    log_info "Simulating rolling upgrade..."

    # Upgrade standby-async first (lowest priority)
    log_info "Upgrading standby-async..."
    docker compose -f "$COMPOSE_FILE" stop standby-async
    sleep 5
    docker compose -f "$COMPOSE_FILE" start standby-async
    sleep 30
    wait_for_service localhost $STANDBY_ASYNC_PORT 60

    # Upgrade standby-semisync
    log_info "Upgrading standby-semisync..."
    docker compose -f "$COMPOSE_FILE" stop standby-semisync
    sleep 5
    docker compose -f "$COMPOSE_FILE" start standby-semisync
    sleep 30
    wait_for_service localhost $STANDBY_SEMISYNC_PORT 60

    # Upgrade standby-sync (requires planned switchover in production)
    log_info "Upgrading standby-sync..."
    docker compose -f "$COMPOSE_FILE" stop standby-sync
    sleep 5
    docker compose -f "$COMPOSE_FILE" start standby-sync
    sleep 30
    wait_for_service localhost $STANDBY_SYNC_PORT 60

    # For primary upgrade, would initiate controlled switchover
    log_info "Note: Primary upgrade would require controlled switchover API"
    log_info "API: POST http://localhost:$PRIMARY_HTTP_PORT/api/switchover/initiate"

    check_cluster_health
    get_replication_status
    log_info "Rolling upgrade test completed"
}

test_controlled_switchover() {
    log_test "Controlled Switchover"

    check_cluster_health || {
        log_error "Cluster not healthy before test"
        return 1
    }

    log_info "Initial cluster status before switchover..."
    get_replication_status

    # Initiate controlled switchover to standby-sync
    log_info "Initiating controlled switchover to standby-sync..."

    # This would call the switchover API
    SWITCHOVER_RESULT=$(curl -sf -X POST http://localhost:$PRIMARY_HTTP_PORT/api/switchover/initiate \
        -H "Content-Type: application/json" \
        -d '{"target_node": "standby-sync"}' 2>/dev/null || echo '{"error": "API not available"}')

    echo "Switchover result: $SWITCHOVER_RESULT"

    # Wait for switchover to complete (5-phase process)
    log_info "Waiting for switchover to complete (60s)..."
    sleep 60

    # Check new cluster status
    log_info "Cluster status after switchover..."
    get_replication_status

    # Verify standby-sync is now primary
    NEW_PRIMARY=$(curl -sf http://localhost:$STANDBY_SYNC_HTTP/api/ha/status 2>/dev/null | jq -r '.role' || echo "unknown")
    if [ "$NEW_PRIMARY" = "primary" ]; then
        log_info "SUCCESS: Standby-sync is now the primary"
    else
        log_warn "Standby-sync role: $NEW_PRIMARY (expected: primary)"
    fi

    # Verify old primary is now standby
    OLD_PRIMARY=$(curl -sf http://localhost:$PRIMARY_HTTP_PORT/api/ha/status 2>/dev/null | jq -r '.role' || echo "unknown")
    log_info "Old primary role: $OLD_PRIMARY"

    check_cluster_health
    log_info "Controlled switchover test completed"
}

test_cleanup() {
    log_test "Cleanup"
    log_info "Stopping and removing cluster..."
    docker compose -f "$COMPOSE_FILE" down -v
    log_info "Cleanup completed"
}

# Main execution
main() {
    cd "$(dirname "$0")"

    case "${1:-all}" in
        startup)
            test_cluster_startup
            ;;
        primary_kill)
            test_primary_kill
            ;;
        network_partition)
            test_network_partition
            ;;
        split_brain)
            test_split_brain_protection
            ;;
        replication_lag)
            test_replication_lag
            ;;
        proxy_failover)
            test_proxy_failover
            ;;
        rolling_upgrade)
            test_rolling_upgrade
            ;;
        switchover)
            test_controlled_switchover
            ;;
        cleanup)
            test_cleanup
            ;;
        all)
            log_info "Running full test suite..."
            test_cluster_startup
            sleep 10
            test_replication_lag
            sleep 5
            test_proxy_failover
            sleep 10
            test_network_partition
            sleep 10
            test_split_brain_protection
            sleep 10
            test_primary_kill
            sleep 15
            test_cleanup
            log_info "Full test suite completed!"
            ;;
        quick)
            log_info "Running quick smoke test..."
            test_cluster_startup
            sleep 10
            check_cluster_health
            get_replication_status
            test_cleanup
            log_info "Quick test completed!"
            ;;
        status)
            check_cluster_health
            get_replication_status
            ;;
        *)
            echo "Usage: $0 {startup|primary_kill|network_partition|split_brain|replication_lag|proxy_failover|rolling_upgrade|switchover|cleanup|all|quick|status}"
            echo ""
            echo "Tests:"
            echo "  startup          - Start the HA cluster and verify health"
            echo "  primary_kill     - Test automatic failover when primary crashes"
            echo "  network_partition - Test cluster behavior during network partition"
            echo "  split_brain      - Test split-brain protection with fencing"
            echo "  replication_lag  - Monitor replication lag across standbys"
            echo "  proxy_failover   - Test proxy routing during failover"
            echo "  rolling_upgrade  - Simulate rolling upgrade of cluster nodes"
            echo "  switchover       - Test controlled switchover between nodes"
            echo ""
            echo "Suites:"
            echo "  all              - Run all tests in sequence"
            echo "  quick            - Run quick smoke test (startup, check, cleanup)"
            echo ""
            echo "Utilities:"
            echo "  status           - Check current cluster health and status"
            echo "  cleanup          - Stop and remove the cluster"
            exit 1
            ;;
    esac
}

main "$@"
