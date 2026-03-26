#!/bin/bash
# HeliosDB Nano HA Failover Test Suite
# Comprehensive testing of failover scenarios

set -e

# Configuration
COMPOSE_FILE="${COMPOSE_FILE:-docker-compose.ha-cluster.yml}"
PROXY_PORT="${PROXY_PORT:-15400}"
PRIMARY_PORT="${PRIMARY_PORT:-15432}"
WRITE_TIMEOUT="${WRITE_TIMEOUT:-30}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

log_info() {
    echo -e "${BLUE}[$(date +%H:%M:%S)]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[$(date +%H:%M:%S)]${NC} $1"
}

log_error() {
    echo -e "${RED}[$(date +%H:%M:%S)]${NC} $1"
}

header() {
    echo -e "${CYAN}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║  $1${NC}"
    echo -e "${CYAN}╚════════════════════════════════════════════════════════════════╝${NC}"
}

wait_for_healthy() {
    local service=$1
    local max_wait=${2:-30}
    local waited=0

    while [ $waited -lt $max_wait ]; do
        if docker compose -f $COMPOSE_FILE ps $service 2>/dev/null | grep -q "healthy"; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

# Test 1: Basic Failover
test_basic_failover() {
    header "Test 1: Basic Primary Failover"
    echo ""

    log_info "Starting workload..."
    ./pg_workload.sh --duration 60 --interval 1 > /tmp/test1.log 2>&1 &
    WORKLOAD_PID=$!

    sleep 15
    log_warn "STOPPING PRIMARY..."
    docker compose -f $COMPOSE_FILE stop primary

    sleep 35
    log_info "RESTARTING PRIMARY..."
    docker compose -f $COMPOSE_FILE start primary

    wait $WORKLOAD_PID 2>/dev/null || true

    # Analyze results
    local total=$(grep -c 'SELECT=\[ok\]' /tmp/test1.log || echo 0)
    local success=$(grep -c '\[OK\]' /tmp/test1.log || echo 0)
    local rate=$(echo "scale=0; $success * 100 / $total" | bc 2>/dev/null || echo 0)

    echo ""
    if [ "$rate" -ge 95 ]; then
        log_success "PASSED: $success/$total operations succeeded ($rate%)"
    else
        log_error "FAILED: $success/$total operations succeeded ($rate%)"
    fi
    echo ""
}

# Test 2: Rapid Failover Recovery
test_rapid_failover() {
    header "Test 2: Rapid Failover Recovery"
    echo ""

    log_info "Starting workload..."
    ./pg_workload.sh --duration 90 --interval 1 > /tmp/test2.log 2>&1 &
    WORKLOAD_PID=$!

    # Multiple rapid failovers
    for i in 1 2 3; do
        sleep 15
        log_warn "Failover #$i: Stopping primary..."
        docker compose -f $COMPOSE_FILE stop primary
        sleep 10
        log_info "Failover #$i: Restarting primary..."
        docker compose -f $COMPOSE_FILE start primary
        wait_for_healthy primary 20
    done

    wait $WORKLOAD_PID 2>/dev/null || true

    local total=$(grep -c 'SELECT=\[ok\]' /tmp/test2.log || echo 0)
    local success=$(grep -c '\[OK\]' /tmp/test2.log || echo 0)
    local rate=$(echo "scale=0; $success * 100 / $total" | bc 2>/dev/null || echo 0)

    echo ""
    if [ "$rate" -ge 90 ]; then
        log_success "PASSED: $success/$total operations survived 3 failovers ($rate%)"
    else
        log_error "FAILED: $success/$total operations ($rate%)"
    fi
    echo ""
}

# Test 3: Cascading Failure
test_cascading_failure() {
    header "Test 3: Cascading Node Failure"
    echo ""

    log_info "Starting workload..."
    ./pg_workload.sh --duration 120 --interval 1 > /tmp/test3.log 2>&1 &
    WORKLOAD_PID=$!

    sleep 15
    log_warn "Stopping standby-async..."
    docker compose -f $COMPOSE_FILE stop standby-async

    sleep 15
    log_warn "Stopping standby-sync..."
    docker compose -f $COMPOSE_FILE stop standby-sync

    sleep 15
    log_warn "Stopping primary (TOTAL OUTAGE)..."
    docker compose -f $COMPOSE_FILE stop primary

    sleep 20
    log_info "Recovering primary..."
    docker compose -f $COMPOSE_FILE start primary
    wait_for_healthy primary 30

    sleep 10
    log_info "Recovering standby-sync..."
    docker compose -f $COMPOSE_FILE start standby-sync

    sleep 10
    log_info "Recovering standby-async..."
    docker compose -f $COMPOSE_FILE start standby-async

    wait $WORKLOAD_PID 2>/dev/null || true

    local total=$(grep -c 'SELECT=\[ok\]' /tmp/test3.log || echo 0)
    local success=$(grep -c '\[OK\]' /tmp/test3.log || echo 0)
    local failed=$(grep -c '\[FAIL\]' /tmp/test3.log || echo 0)

    echo ""
    log_info "Results: $success successful, $failed failed out of $total"
    if [ "$failed" -lt 10 ]; then
        log_success "PASSED: Recovered from cascading failure"
    else
        log_warn "PARTIAL: Some operations failed during total outage (expected)"
    fi
    echo ""
}

# Test 4: Write Timeout Verification
test_write_timeout() {
    header "Test 4: Write Timeout Verification"
    echo ""

    log_info "Verifying write timeout behavior (${WRITE_TIMEOUT}s configured)..."

    # Stop primary
    log_warn "Stopping primary..."
    docker compose -f $COMPOSE_FILE stop primary

    sleep 5

    # Time a write operation
    log_info "Attempting write (should wait up to ${WRITE_TIMEOUT}s)..."
    local start=$(date +%s)

    PGPASSWORD=helios timeout $((WRITE_TIMEOUT + 10)) psql -h localhost -p $PROXY_PORT \
        -U helios -d heliosdb -c "INSERT INTO test_timeout (id) VALUES (999)" 2>&1 &
    WRITE_PID=$!

    # Restart primary after 15 seconds
    sleep 15
    log_info "Restarting primary..."
    docker compose -f $COMPOSE_FILE start primary

    wait $WRITE_PID 2>/dev/null
    local end=$(date +%s)
    local duration=$((end - start))

    echo ""
    if [ $duration -ge 10 ] && [ $duration -le $((WRITE_TIMEOUT + 5)) ]; then
        log_success "PASSED: Write waited ${duration}s for primary recovery"
    else
        log_warn "PARTIAL: Write completed in ${duration}s"
    fi
    echo ""
}

# Test 5: Read Availability During Failover
test_read_availability() {
    header "Test 5: Read Availability During Primary Outage"
    echo ""

    log_info "Stopping primary..."
    docker compose -f $COMPOSE_FILE stop primary

    sleep 5

    log_info "Testing reads through proxy..."
    local read_success=0
    local read_total=10

    for i in $(seq 1 $read_total); do
        result=$(PGPASSWORD=helios psql -h localhost -p $PROXY_PORT -U helios -d heliosdb \
            -t -c "SELECT 1" 2>&1)
        if [[ "$result" == *"1"* ]]; then
            read_success=$((read_success + 1))
            echo -e "    Read #$i: ${GREEN}OK${NC}"
        else
            echo -e "    Read #$i: ${RED}FAIL${NC}"
        fi
        sleep 0.5
    done

    log_info "Restarting primary..."
    docker compose -f $COMPOSE_FILE start primary
    wait_for_healthy primary 30

    echo ""
    if [ $read_success -eq $read_total ]; then
        log_success "PASSED: All $read_total reads succeeded during primary outage"
    else
        log_error "FAILED: Only $read_success/$read_total reads succeeded"
    fi
    echo ""
}

# Run all tests
run_all_tests() {
    header "HeliosDB Nano HA Failover Test Suite"
    echo ""
    log_info "Starting comprehensive failover testing..."
    echo ""

    # Ensure clean state
    log_info "Ensuring all nodes are healthy..."
    docker compose -f $COMPOSE_FILE start primary standby-sync standby-async
    wait_for_healthy primary 30
    wait_for_healthy standby-sync 30
    wait_for_healthy standby-async 30

    # Create test table
    log_info "Creating test tables..."
    PGPASSWORD=helios psql -h localhost -p $PROXY_PORT -U helios -d heliosdb <<EOF 2>/dev/null
DROP TABLE IF EXISTS test_timeout;
CREATE TABLE test_timeout (id INTEGER PRIMARY KEY);
EOF

    # Run tests
    test_read_availability
    sleep 5

    test_basic_failover
    sleep 5

    test_write_timeout
    sleep 5

    test_rapid_failover
    sleep 5

    test_cascading_failure

    echo ""
    header "Test Suite Complete"
    echo ""
    log_info "Review /tmp/test*.log files for detailed results"
}

# Parse arguments
case "${1:-all}" in
    basic)
        test_basic_failover
        ;;
    rapid)
        test_rapid_failover
        ;;
    cascade)
        test_cascading_failure
        ;;
    timeout)
        test_write_timeout
        ;;
    reads)
        test_read_availability
        ;;
    all)
        run_all_tests
        ;;
    *)
        echo "Usage: $0 [basic|rapid|cascade|timeout|reads|all]"
        echo ""
        echo "Tests:"
        echo "  basic    - Basic primary failover test"
        echo "  rapid    - Rapid consecutive failovers"
        echo "  cascade  - Cascading node failure"
        echo "  timeout  - Write timeout verification"
        echo "  reads    - Read availability during failover"
        echo "  all      - Run all tests"
        exit 1
        ;;
esac
