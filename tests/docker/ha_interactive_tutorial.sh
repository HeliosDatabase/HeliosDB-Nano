#!/bin/bash
#
# HeliosDB-Lite HA Interactive Tutorial Script
#
# This script provides interactive checkpoints for the HA tutorial.
# Each checkpoint pauses to let users verify cluster status and data replication.
#
# Usage:
#   ./ha_interactive_tutorial.sh              # Run full tutorial interactively
#   ./ha_interactive_tutorial.sh checkpoint1  # Run specific checkpoint
#   ./ha_interactive_tutorial.sh all          # Run all checkpoints non-interactively
#

set -e

# Configuration
COMPOSE_FILE="docker-compose.ha-cluster.yml"
PRIMARY_PORT=15432
STANDBY_SYNC_PORT=15442
STANDBY_SEMI_PORT=15452
STANDBY_ASYNC_PORT=15462
PROXY_PORT=15400
PROXY_ADMIN=19090

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

# Default to interactive mode
INTERACTIVE=${INTERACTIVE:-true}

# Helper functions
print_header() {
    echo ""
    echo -e "${BLUE}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║${NC} ${BOLD}$1${NC}"
    echo -e "${BLUE}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
}

print_subheader() {
    echo ""
    echo -e "${CYAN}═══ $1 ═══${NC}"
    echo ""
}

print_success() {
    echo -e "${GREEN}✓${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}⚠${NC} $1"
}

print_error() {
    echo -e "${RED}✗${NC} $1"
}

print_info() {
    echo -e "${CYAN}ℹ${NC} $1"
}

pause() {
    if [ "$INTERACTIVE" = true ]; then
        echo ""
        echo -e "${YELLOW}────────────────────────────────────────────────────────────────────────${NC}"
        echo -e "${YELLOW}│ PAUSE: Take a moment to observe the results above.                  │${NC}"
        echo -e "${YELLOW}│ Press ENTER to continue or Ctrl+C to stop...                       │${NC}"
        echo -e "${YELLOW}────────────────────────────────────────────────────────────────────────${NC}"
        read -r
    else
        echo ""
        echo -e "${CYAN}[Non-interactive mode - continuing...]${NC}"
        sleep 2
    fi
}

check_node() {
    local port=$1
    local name=$2
    local result=$(PGPASSWORD=helios psql -h localhost -p $port -U helios -d heliosdb -t -c "SELECT 1" 2>&1)
    if [[ "$result" == *"1"* ]]; then
        print_success "$name (port $port): UP"
        return 0
    else
        print_error "$name (port $port): DOWN"
        return 1
    fi
}

# Checkpoint 1: Verify Cluster Startup
checkpoint1() {
    print_header "CHECKPOINT 1: Cluster Startup Verification"

    print_subheader "Container Status"
    docker compose -f "$COMPOSE_FILE" ps 2>/dev/null || docker-compose -f "$COMPOSE_FILE" ps

    print_subheader "Node Connectivity Check"
    check_node $PRIMARY_PORT "Primary" || true
    check_node $STANDBY_SYNC_PORT "Standby-Sync" || true
    check_node $STANDBY_SEMI_PORT "Standby-Semi" || true
    check_node $STANDBY_ASYNC_PORT "Standby-Async" || true

    print_subheader "Proxy Health"
    if curl -sf http://localhost:$PROXY_ADMIN/health > /dev/null 2>&1; then
        print_success "HeliosProxy: HEALTHY"
    else
        print_warning "HeliosProxy: NOT RESPONDING (may need more time to start)"
    fi

    print_info "All nodes should show 'UP' before proceeding."
    print_info "If nodes are DOWN, wait a few seconds and run this checkpoint again."

    pause
}

# Checkpoint 2: Verify Replication Status
checkpoint2() {
    print_header "CHECKPOINT 2: Replication Status Verification"

    print_subheader "Primary's View of Standbys"
    PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
SELECT
    node_id,
    sync_mode,
    state,
    flush_lsn,
    apply_lsn,
    lag_bytes,
    lag_ms
FROM pg_replication_standbys
ORDER BY node_id;
" 2>/dev/null || print_warning "Could not query replication status (primary may not be ready)"

    print_subheader "Standby-Sync's View of Primary"
    PGPASSWORD=helios psql -h localhost -p $STANDBY_SYNC_PORT -U helios -d heliosdb -c "
SELECT
    node_id as primary_id,
    state,
    primary_lsn,
    local_lsn,
    lag_bytes,
    lag_ms
FROM pg_replication_primary;
" 2>/dev/null || print_warning "Could not query primary status from standby"

    print_info "Key things to observe:"
    print_info "  - 'state' should be 'streaming' for all standbys"
    print_info "  - 'lag_ms' should be low (< 100ms typically)"
    print_info "  - 'sync_mode' shows each standby's replication mode"

    pause
}

# Checkpoint 3: Verify Data Replication
checkpoint3() {
    print_header "CHECKPOINT 3: Data Replication Verification"

    # Create test table if it doesn't exist (ignore error if it already exists)
    PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
CREATE TABLE replication_test (
    id INTEGER PRIMARY KEY,
    message TEXT,
    sync_mode TEXT,
    created_at TEXT
);
" 2>/dev/null || true  # Ignore error if table exists

    # Insert test data with unique ID based on timestamp
    print_info "Inserting test row..."
    local test_id=$(($(date +%s) % 100000))
    PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
INSERT INTO replication_test (id, message, sync_mode, created_at)
VALUES ($test_id, 'Checkpoint 3 test', 'tutorial', '$(date -Iseconds)');
" 2>/dev/null || print_warning "Insert may have failed (duplicate id?)"

    sleep 1  # Brief wait for replication

    print_subheader "Data on Primary (port $PRIMARY_PORT)"
    PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
SELECT id, message, sync_mode, created_at FROM replication_test ORDER BY id DESC LIMIT 5;
" 2>/dev/null

    print_subheader "Data on Standby-Sync (port $STANDBY_SYNC_PORT)"
    PGPASSWORD=helios psql -h localhost -p $STANDBY_SYNC_PORT -U helios -d heliosdb -c "
SELECT id, message, sync_mode, created_at FROM replication_test ORDER BY id DESC LIMIT 5;
" 2>/dev/null

    print_subheader "Data on Standby-Async (port $STANDBY_ASYNC_PORT)"
    PGPASSWORD=helios psql -h localhost -p $STANDBY_ASYNC_PORT -U helios -d heliosdb -c "
SELECT id, message, sync_mode, created_at FROM replication_test ORDER BY id DESC LIMIT 5;
" 2>/dev/null

    print_info "Key things to observe:"
    print_info "  - All nodes should have identical data"
    print_info "  - Timestamps are replicated, not regenerated"
    print_info "  - Sync standby gets data before async (usually same timing though)"

    pause
}

# Checkpoint 4: Verify TWR Functionality
checkpoint4() {
    print_header "CHECKPOINT 4: Transparent Write Routing (TWR) Verification"

    local base_id=$(($(date +%s) % 100000))

    print_subheader "Test 1: Write via Standby-Sync (should SUCCEED via TWR)"
    if PGPASSWORD=helios psql -h localhost -p $STANDBY_SYNC_PORT -U helios -d heliosdb -c "
INSERT INTO replication_test (id, message, sync_mode, created_at)
VALUES ($((base_id + 1)), 'TWR test via standby-sync', 'sync', '$(date -Iseconds)');
" 2>/dev/null; then
        print_success "Write via standby-sync: FORWARDED TO PRIMARY"
    else
        print_warning "Write via standby-sync: Failed (check if TWR is enabled)"
    fi

    print_subheader "Test 2: Write via Standby-Semi (should SUCCEED via TWR)"
    if PGPASSWORD=helios psql -h localhost -p $STANDBY_SEMI_PORT -U helios -d heliosdb -c "
INSERT INTO replication_test (id, message, sync_mode, created_at)
VALUES ($((base_id + 2)), 'TWR test via standby-semi', 'semi-sync', '$(date -Iseconds)');
" 2>/dev/null; then
        print_success "Write via standby-semi: FORWARDED TO PRIMARY"
    else
        print_warning "Write via standby-semi: Failed (check if TWR is enabled)"
    fi

    print_subheader "Test 3: Write via Standby-Async (should be REJECTED)"
    if PGPASSWORD=helios psql -h localhost -p $STANDBY_ASYNC_PORT -U helios -d heliosdb -c "
INSERT INTO replication_test (id, message, sync_mode, created_at)
VALUES ($((base_id + 3)), 'TWR test via standby-async', 'async', '$(date -Iseconds)');
" 2>&1 | grep -qi "error\|denied\|read-only"; then
        print_success "Write via standby-async: CORRECTLY REJECTED (async doesn't support TWR)"
    else
        result=$(PGPASSWORD=helios psql -h localhost -p $STANDBY_ASYNC_PORT -U helios -d heliosdb -c "
INSERT INTO replication_test (id, message, sync_mode, created_at)
VALUES ($((base_id + 4)), 'TWR test via standby-async', 'async', '$(date -Iseconds)');
" 2>&1)
        if [[ "$result" == *"INSERT"* ]]; then
            print_warning "Write via standby-async: Succeeded (unexpected - async may have TWR enabled)"
        else
            print_success "Write via standby-async: CORRECTLY REJECTED"
        fi
    fi

    print_subheader "Verify All TWR Writes Landed on Primary"
    PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
SELECT id, message, sync_mode FROM replication_test ORDER BY id;
" 2>/dev/null
    print_info "(TWR test rows are the ones with 'TWR test via...' in message)"

    print_info "Key things to observe:"
    print_info "  - Writes via sync/semi-sync standbys are forwarded to primary"
    print_info "  - Async standby rejects writes (preserves data consistency)"
    print_info "  - All successful writes appear on primary"

    pause
}

# Checkpoint 5: Verify Failover Behavior
checkpoint5() {
    print_header "CHECKPOINT 5: Failover Behavior Verification"

    print_info "This checkpoint assumes the primary is STOPPED."
    print_info "If primary is running, stop it first."
    echo ""
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}STOP PRIMARY - Copy and run in another terminal:${NC}           ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                             ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   docker compose -f docker-compose.ha-cluster.yml stop primary ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                             ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    print_subheader "Test 1: Primary Connection (should FAIL)"
    if PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "SELECT 1" 2>&1 | grep -qi "refused\|error\|failed"; then
        print_success "Primary (port $PRIMARY_PORT): CONNECTION REFUSED (as expected)"
    else
        print_warning "Primary is still responding - stop it to test failover"
    fi

    print_subheader "Test 2: Standby-Sync Read (should SUCCEED)"
    if PGPASSWORD=helios psql -h localhost -p $STANDBY_SYNC_PORT -U helios -d heliosdb -c "
SELECT COUNT(*) as total_rows FROM replication_test;
" 2>/dev/null; then
        print_success "Standby-Sync read: WORKING"
    else
        print_error "Standby-Sync read: FAILED"
    fi

    print_subheader "Test 3: Proxy Read (should route to standby)"
    if PGPASSWORD=helios psql -h localhost -p $PROXY_PORT -U helios -d heliosdb -c "
SELECT COUNT(*) as total_rows FROM replication_test;
" 2>/dev/null; then
        print_success "Proxy read: WORKING (routed to standby)"
    else
        print_error "Proxy read: FAILED"
    fi

    print_subheader "Test 4: Proxy Write (will timeout waiting for primary)"
    print_info "Attempting write through proxy (will wait up to 10s)..."
    local fail_test_id=$(($(date +%s) % 100000 + 5000))
    if timeout 10 bash -c "PGPASSWORD=helios psql -h localhost -p 15400 -U helios -d heliosdb -c \"INSERT INTO replication_test (id, message, sync_mode, created_at) VALUES ($fail_test_id, 'failover test', 'proxy', '$(date -Iseconds)')\"" 2>&1; then
        print_warning "Write succeeded - primary may have returned"
    else
        print_success "Write timed out (proxy waiting for primary - expected behavior)"
    fi

    print_info "Key things to observe:"
    print_info "  - Primary connections are refused (container stopped)"
    print_info "  - Standby reads continue working"
    print_info "  - Proxy routes reads to healthy standbys"
    print_info "  - Proxy writes wait for primary (with timeout)"

    pause
}

# Checkpoint 6: Verify Recovery and Reconnection
checkpoint6() {
    print_header "CHECKPOINT 6: Recovery and Reconnection Verification"

    print_info "This checkpoint verifies cluster recovery after primary restart."
    print_info "If primary is not running, start it first."
    echo ""
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}START PRIMARY - Copy and run in another terminal:${NC}          ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                             ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   docker compose -f docker-compose.ha-cluster.yml start primary ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                             ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    print_subheader "Waiting for primary to start (10 seconds)..."
    sleep 10

    print_subheader "Test 1: Primary Connection"
    if check_node $PRIMARY_PORT "Primary"; then
        print_success "Primary has recovered!"
    else
        print_warning "Primary not yet ready - wait longer and retry"
    fi

    print_subheader "Test 2: Replication Status (standbys reconnected?)"
    PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
SELECT
    node_id,
    state,
    flush_lsn,
    lag_ms
FROM pg_replication_standbys
ORDER BY node_id;
" 2>/dev/null || print_warning "Could not query replication status"

    print_subheader "Test 3: Write Capability Restored"
    local recovery_id=$(($(date +%s) % 100000 + 6000))
    if PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
INSERT INTO replication_test (id, message, sync_mode, created_at)
VALUES ($recovery_id, 'Post-recovery write', 'recovery', '$(date -Iseconds)');
" 2>/dev/null; then
        print_success "Write capability: RESTORED"
    else
        print_error "Write capability: NOT WORKING"
    fi

    print_subheader "Test 4: Replication Still Working"
    sleep 1
    PGPASSWORD=helios psql -h localhost -p $STANDBY_SYNC_PORT -U helios -d heliosdb -c "
SELECT id, message FROM replication_test
WHERE message = 'Post-recovery write';
" 2>/dev/null

    print_info "Key things to observe:"
    print_info "  - All standbys show 'state = streaming'"
    print_info "  - Writes work and replicate normally"
    print_info "  - Check standby logs for reconnection messages"
    print_info "    docker logs heliosdb-standby-sync 2>&1 | tail -20"

    pause
}

# Checkpoint 7: Final Verification
checkpoint7() {
    print_header "CHECKPOINT 7: Final Cluster Verification"

    print_subheader "Node Status Summary"
    echo "┌────────────────────┬────────────┬────────────────┐"
    echo "│ Node               │ Status     │ Role           │"
    echo "├────────────────────┼────────────┼────────────────┤"

    # Primary
    if PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -t -c "SELECT 1" 2>/dev/null | grep -q "1"; then
        printf "│ %-18s │ ${GREEN}%-10s${NC} │ %-14s │\n" "Primary" "UP" "Read/Write"
    else
        printf "│ %-18s │ ${RED}%-10s${NC} │ %-14s │\n" "Primary" "DOWN" "Read/Write"
    fi

    # Standby-Sync
    if PGPASSWORD=helios psql -h localhost -p $STANDBY_SYNC_PORT -U helios -d heliosdb -t -c "SELECT 1" 2>/dev/null | grep -q "1"; then
        printf "│ %-18s │ ${GREEN}%-10s${NC} │ %-14s │\n" "Standby-Sync" "UP" "Read + TWR"
    else
        printf "│ %-18s │ ${RED}%-10s${NC} │ %-14s │\n" "Standby-Sync" "DOWN" "Read + TWR"
    fi

    # Standby-Semi
    if PGPASSWORD=helios psql -h localhost -p $STANDBY_SEMI_PORT -U helios -d heliosdb -t -c "SELECT 1" 2>/dev/null | grep -q "1"; then
        printf "│ %-18s │ ${GREEN}%-10s${NC} │ %-14s │\n" "Standby-Semi" "UP" "Read + TWR"
    else
        printf "│ %-18s │ ${RED}%-10s${NC} │ %-14s │\n" "Standby-Semi" "DOWN" "Read + TWR"
    fi

    # Standby-Async
    if PGPASSWORD=helios psql -h localhost -p $STANDBY_ASYNC_PORT -U helios -d heliosdb -t -c "SELECT 1" 2>/dev/null | grep -q "1"; then
        printf "│ %-18s │ ${GREEN}%-10s${NC} │ %-14s │\n" "Standby-Async" "UP" "Read-only"
    else
        printf "│ %-18s │ ${RED}%-10s${NC} │ %-14s │\n" "Standby-Async" "DOWN" "Read-only"
    fi

    # Proxy
    if curl -sf http://localhost:$PROXY_ADMIN/health > /dev/null 2>&1; then
        printf "│ %-18s │ ${GREEN}%-10s${NC} │ %-14s │\n" "HeliosProxy" "HEALTHY" "Router"
    else
        printf "│ %-18s │ ${RED}%-10s${NC} │ %-14s │\n" "HeliosProxy" "UNHEALTHY" "Router"
    fi

    echo "└────────────────────┴────────────┴────────────────┘"

    print_subheader "Data Consistency Check"
    PRIMARY_COUNT=$(PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -t -c "SELECT COUNT(*) FROM replication_test" 2>/dev/null | tr -d ' ' || echo "N/A")
    SYNC_COUNT=$(PGPASSWORD=helios psql -h localhost -p $STANDBY_SYNC_PORT -U helios -d heliosdb -t -c "SELECT COUNT(*) FROM replication_test" 2>/dev/null | tr -d ' ' || echo "N/A")
    ASYNC_COUNT=$(PGPASSWORD=helios psql -h localhost -p $STANDBY_ASYNC_PORT -U helios -d heliosdb -t -c "SELECT COUNT(*) FROM replication_test" 2>/dev/null | tr -d ' ' || echo "N/A")

    echo "Rows in replication_test table:"
    echo "  Primary:       $PRIMARY_COUNT"
    echo "  Standby-Sync:  $SYNC_COUNT"
    echo "  Standby-Async: $ASYNC_COUNT"

    if [ "$PRIMARY_COUNT" == "$SYNC_COUNT" ] && [ "$SYNC_COUNT" == "$ASYNC_COUNT" ]; then
        print_success "Data is CONSISTENT across all nodes!"
    else
        print_warning "Data counts differ - replication may be catching up"
    fi

    print_subheader "Replication Lag Summary"
    PGPASSWORD=helios psql -h localhost -p $PRIMARY_PORT -U helios -d heliosdb -c "
SELECT
    node_id,
    sync_mode,
    state,
    lag_ms as \"lag (ms)\"
FROM pg_replication_standbys
ORDER BY lag_ms ASC;
" 2>/dev/null || print_warning "Could not query replication lag"

    echo ""
    echo -e "${GREEN}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║                   TUTORIAL COMPLETED SUCCESSFULLY!                   ║${NC}"
    echo -e "${GREEN}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    print_info "You've completed the HeliosDB-Lite HA Interactive Tutorial!"
    print_info ""
    print_info "Key learnings:"
    print_info "  1. Sync mode guarantees zero data loss"
    print_info "  2. TWR simplifies application connectivity"
    print_info "  3. Automatic reconnection provides self-healing"
    print_info "  4. Proxy provides seamless failover for applications"
    print_info ""
    print_info "Next steps:"
    print_info "  - Review: docs/guides/HA_HANDS_ON_TUTORIAL.md"
    print_info "  - Test failover: ./test_failover.sh"
    print_info "  - Run workload: ./pg_workload.sh"
    echo ""
}

# Full tutorial
run_full_tutorial() {
    print_header "HeliosDB-Lite HA Interactive Tutorial"

    echo -e "${CYAN}Welcome to the HeliosDB-Lite High Availability Interactive Tutorial!${NC}"
    echo ""
    echo "This tutorial will guide you through:"
    echo "  1. Verifying cluster startup"
    echo "  2. Understanding replication status"
    echo "  3. Testing data replication"
    echo "  4. Testing Transparent Write Routing (TWR)"
    echo "  5. Testing failover behavior"
    echo "  6. Testing recovery and reconnection"
    echo "  7. Final verification"
    echo ""
    echo "At each checkpoint, you'll have time to observe and verify results."
    echo ""

    pause

    checkpoint1
    checkpoint2
    checkpoint3
    checkpoint4

    echo ""
    print_info "The next checkpoints involve stopping the primary."
    print_info "Run them manually when ready:"
    echo ""
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}CHECKPOINT 5 - Failover Test:${NC}                                      ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   1. Stop primary:  docker compose -f docker-compose.ha-cluster.yml stop primary  ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   2. Run test:      ./ha_interactive_tutorial.sh checkpoint5         ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}CHECKPOINT 6 - Recovery Test:${NC}                                      ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   1. Start primary: docker compose -f docker-compose.ha-cluster.yml start primary ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   2. Run test:      ./ha_interactive_tutorial.sh checkpoint6         ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}CHECKPOINT 7 - Final Verification:${NC}                                 ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Run:              ./ha_interactive_tutorial.sh checkpoint7         ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""
}

# Main
main() {
    cd "$(dirname "$0")"

    case "${1:-interactive}" in
        checkpoint1)
            checkpoint1
            ;;
        checkpoint2)
            checkpoint2
            ;;
        checkpoint3)
            checkpoint3
            ;;
        checkpoint4)
            checkpoint4
            ;;
        checkpoint5)
            checkpoint5
            ;;
        checkpoint6)
            checkpoint6
            ;;
        checkpoint7)
            checkpoint7
            ;;
        all)
            INTERACTIVE=false
            checkpoint1
            checkpoint2
            checkpoint3
            checkpoint4
            # Skip 5 and 6 in automated mode (require manual intervention)
            checkpoint7
            ;;
        interactive)
            run_full_tutorial
            ;;
        help|--help|-h)
            echo "HeliosDB-Lite HA Interactive Tutorial"
            echo ""
            echo "Usage: $0 [command]"
            echo ""
            echo "Commands:"
            echo "  interactive    Run full tutorial with pauses (default)"
            echo "  checkpoint1    Verify cluster startup"
            echo "  checkpoint2    Verify replication status"
            echo "  checkpoint3    Verify data replication"
            echo "  checkpoint4    Verify TWR functionality"
            echo "  checkpoint5    Verify failover behavior (primary must be stopped)"
            echo "  checkpoint6    Verify recovery (primary must be restarted)"
            echo "  checkpoint7    Final verification"
            echo "  all            Run all checkpoints non-interactively"
            echo "  help           Show this help"
            echo ""
            echo "Examples:"
            echo "  $0                    # Start interactive tutorial"
            echo "  $0 checkpoint3        # Run checkpoint 3 only"
            echo "  INTERACTIVE=false $0 checkpoint1  # Run without pauses"
            ;;
        *)
            echo "Unknown command: $1"
            echo "Run '$0 help' for usage"
            exit 1
            ;;
    esac
}

main "$@"
