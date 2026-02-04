#!/bin/bash
# ============================================================================
# HeliosProxy Functionality Tutorial
# ============================================================================
#
# This tutorial demonstrates all HeliosProxy features:
#   1. Health Monitoring (health, ready, live endpoints)
#   2. Metrics & Prometheus Integration
#   3. Node Management (list, enable, disable)
#   4. Configuration Inspection
#   5. Session Monitoring
#   6. Read/Write Splitting & Load Balancing
#   7. PostgreSQL Wire Protocol Proxy
#
# Usage:
#   ./heliosproxy_tutorial.sh              # Run full tutorial interactively
#   ./heliosproxy_tutorial.sh <section>    # Run specific section
#   INTERACTIVE=false ./heliosproxy_tutorial.sh  # Non-interactive mode
#
# ============================================================================

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Configuration
PROXY_HOST="localhost"
PROXY_PORT="15400"           # PostgreSQL wire protocol
PROXY_METRICS_PORT="19090"   # Admin/Metrics API

PRIMARY_PORT="15432"
STANDBY_SYNC_PORT="15442"
STANDBY_SEMI_PORT="15452"
STANDBY_ASYNC_PORT="15462"

HELIOS_USER="helios"
HELIOS_PASSWORD="helios"
HELIOS_DATABASE="heliosdb"

# Interactive mode (set INTERACTIVE=false to skip pauses)
INTERACTIVE=${INTERACTIVE:-true}

# ============================================================================
# Helper Functions
# ============================================================================

print_header() {
    echo -e "${BLUE}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║${NC} ${BOLD}$1${NC}"
    echo -e "${BLUE}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
}

print_subheader() {
    echo -e "\n${CYAN}═══ $1 ═══${NC}\n"
}

print_success() {
    echo -e "${GREEN}✓${NC} $1"
}

print_error() {
    echo -e "${RED}✗${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}⚠${NC} $1"
}

print_info() {
    echo -e "${CYAN}ℹ${NC} $1"
}

print_command() {
    echo -e "${YELLOW}▶${NC} ${BOLD}$1${NC}"
}

pause() {
    if [[ "$INTERACTIVE" == "true" ]]; then
        echo ""
        echo -e "${YELLOW}────────────────────────────────────────────────────────────────────────${NC}"
        echo -e "${YELLOW}│ Press ENTER to continue or Ctrl+C to stop...                        │${NC}"
        echo -e "${YELLOW}────────────────────────────────────────────────────────────────────────${NC}"
        read -r
    else
        echo -e "\n${CYAN}[Non-interactive mode - continuing...]${NC}\n"
    fi
}

api_call() {
    local method=$1
    local endpoint=$2
    local data=$3

    if [[ -n "$data" ]]; then
        curl -s -X "$method" "http://${PROXY_HOST}:${PROXY_METRICS_PORT}${endpoint}" \
            -H "Content-Type: application/json" \
            -d "$data" | jq . 2>/dev/null || echo "(raw response)"
    else
        curl -s -X "$method" "http://${PROXY_HOST}:${PROXY_METRICS_PORT}${endpoint}" | jq . 2>/dev/null || echo "(raw response)"
    fi
}

# ============================================================================
# Section 1: Health Monitoring
# ============================================================================

section_health() {
    print_header "Section 1: Health Monitoring"

    echo "HeliosProxy provides multiple health endpoints for integration with"
    echo "load balancers, Kubernetes probes, and monitoring systems."
    echo ""

    print_subheader "1.1 Basic Health Check"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/health"
    api_call GET /health
    echo ""
    print_info "Returns 'ok' if the proxy is running."

    print_subheader "1.2 Readiness Probe (Kubernetes)"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/health/ready"
    api_call GET /health/ready
    echo ""
    print_info "Returns 200 if proxy can route queries (has healthy backends)."
    print_info "Returns 503 if no healthy backends available."

    print_subheader "1.3 Liveness Probe (Kubernetes)"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/health/live"
    api_call GET /health/live
    echo ""
    print_info "Returns 200 if the proxy process is alive."

    print_subheader "Kubernetes Probe Configuration Example"
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Add to your Kubernetes deployment:${NC}                                ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   livenessProbe:                                                    ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}     httpGet:                                                        ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}       path: /health/live                                            ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}       port: 9090                                                    ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   readinessProbe:                                                   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}     httpGet:                                                        ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}       path: /health/ready                                           ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}       port: 9090                                                    ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"

    pause
}

# ============================================================================
# Section 2: Metrics & Prometheus
# ============================================================================

section_metrics() {
    print_header "Section 2: Metrics & Prometheus Integration"

    echo "HeliosProxy exposes detailed metrics for monitoring query routing,"
    echo "connection pooling, and failover events."
    echo ""

    print_subheader "2.1 JSON Metrics"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/metrics"
    api_call GET /metrics
    echo ""

    print_info "Key metrics explained:"
    echo "  • connections_accepted: Total client connections received"
    echo "  • connections_closed:   Connections that have been closed"
    echo "  • connections_active:   Currently active connections"
    echo "  • queries_processed:    Total queries routed"
    echo "  • bytes_received/sent:  Network traffic volume"
    echo "  • failovers:            Number of automatic failovers triggered"

    print_subheader "2.2 Prometheus Format"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/metrics/prometheus"
    echo ""
    curl -s "http://${PROXY_HOST}:${PROXY_METRICS_PORT}/metrics/prometheus" | jq -r '.text' 2>/dev/null || \
        curl -s "http://${PROXY_HOST}:${PROXY_METRICS_PORT}/metrics/prometheus"
    echo ""

    print_subheader "Prometheus Scrape Configuration"
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Add to prometheus.yml:${NC}                                              ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   scrape_configs:                                                   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}     - job_name: 'heliosdb-proxy'                                    ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}       static_configs:                                               ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}         - targets: ['heliosdb-proxy:9090']                          ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}       metrics_path: /metrics/prometheus                             ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"

    pause
}

# ============================================================================
# Section 3: Node Management
# ============================================================================

section_nodes() {
    print_header "Section 3: Node Management"

    echo "HeliosProxy monitors backend nodes and allows dynamic management"
    echo "without restarting the proxy."
    echo ""

    print_subheader "3.1 List All Nodes"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/nodes"
    api_call GET /nodes
    echo ""

    print_info "Each node shows:"
    echo "  • address:         Host:port of the backend"
    echo "  • healthy:         Current health status"
    echo "  • last_check:      Timestamp of last health check"
    echo "  • failure_count:   Consecutive failures (resets on success)"
    echo "  • latency_ms:      Response time of last health check"
    echo "  • replication_lag: Bytes behind primary (for standbys)"

    print_subheader "3.2 Get Specific Node Details"
    # Get first node address from the list
    local node_addr=$(curl -s "http://${PROXY_HOST}:${PROXY_METRICS_PORT}/nodes" | jq -r '.[0].address' 2>/dev/null)
    if [[ -n "$node_addr" && "$node_addr" != "null" ]]; then
        print_command "curl http://localhost:$PROXY_METRICS_PORT/nodes/$node_addr"
        api_call GET "/nodes/$node_addr"
    else
        print_warning "No nodes found to query"
    fi
    echo ""

    print_subheader "3.3 Disable/Enable Nodes (Maintenance Mode)"
    echo "You can temporarily disable a node for maintenance:"
    echo ""
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Disable a node:${NC}                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   curl -X POST http://localhost:$PROXY_METRICS_PORT/nodes/{address}/disable   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Re-enable a node:${NC}                                                   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   curl -X POST http://localhost:$PROXY_METRICS_PORT/nodes/{address}/enable    ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""
    print_info "Disabled nodes are excluded from routing until re-enabled."

    pause
}

# ============================================================================
# Section 4: Configuration
# ============================================================================

section_config() {
    print_header "Section 4: Configuration Inspection"

    echo "View the proxy's current configuration settings."
    echo ""

    print_subheader "4.1 View Current Configuration"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/config"
    api_call GET /config
    echo ""

    print_info "Configuration parameters:"
    echo "  • listen_address:       Client connection endpoint"
    echo "  • admin_address:        Admin API endpoint"
    echo "  • tr_enabled:           Transaction Replay enabled"
    echo "  • tr_mode:              Transaction Replay mode"
    echo "  • pool_min_connections: Minimum pool size per node"
    echo "  • pool_max_connections: Maximum pool size per node"
    echo "  • nodes:                Configured backend nodes"

    print_subheader "4.2 Version Information"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/version"
    api_call GET /version

    pause
}

# ============================================================================
# Section 5: Sessions
# ============================================================================

section_sessions() {
    print_header "Section 5: Session Monitoring"

    echo "Monitor active client sessions through the proxy."
    echo ""

    print_subheader "5.1 View Active Sessions"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/sessions"
    api_call GET /sessions
    echo ""

    print_info "Shows the number of currently connected clients."

    print_subheader "5.2 Connection Pool Status"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/pools"
    api_call GET /pools
    echo ""

    print_info "Pool stats show connection utilization per backend node."

    pause
}

# ============================================================================
# Section 6: Read/Write Splitting & Load Balancing
# ============================================================================

section_routing() {
    print_header "Section 6: Read/Write Splitting & Load Balancing"

    echo "HeliosProxy automatically routes queries based on type:"
    echo "  • WRITE queries (INSERT, UPDATE, DELETE) → Primary"
    echo "  • READ queries (SELECT) → Load-balanced across standbys"
    echo ""

    print_subheader "6.1 Connect via Proxy (PostgreSQL Protocol)"
    echo "Connect through the proxy just like a normal PostgreSQL server:"
    echo ""
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Connect via psql:${NC}                                                   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   PGPASSWORD=helios psql -h localhost -p $PROXY_PORT -U helios -d heliosdb ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Connection string:${NC}                                                  ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   postgresql://helios:helios@localhost:$PROXY_PORT/heliosdb           ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    print_subheader "6.2 Test Write Routing (goes to Primary)"
    print_command "psql via proxy: INSERT INTO test_proxy (name) VALUES ('test');"

    # Create test table if not exists
    PGPASSWORD=$HELIOS_PASSWORD psql -h $PROXY_HOST -p $PROXY_PORT -U $HELIOS_USER -d $HELIOS_DATABASE -c "
    CREATE TABLE IF NOT EXISTS test_proxy (id INTEGER, name TEXT, created_at TEXT);
    " 2>/dev/null || true

    # Insert via proxy
    local test_id=$(($(date +%s) % 100000))
    if PGPASSWORD=$HELIOS_PASSWORD psql -h $PROXY_HOST -p $PROXY_PORT -U $HELIOS_USER -d $HELIOS_DATABASE -c "
    INSERT INTO test_proxy (id, name, created_at) VALUES ($test_id, 'proxy_write_test', '$(date -Iseconds)');
    " 2>/dev/null; then
        print_success "Write via proxy: SUCCESS (routed to primary)"
    else
        print_error "Write via proxy: FAILED"
    fi
    echo ""

    print_subheader "6.3 Test Read Routing (load-balanced to standbys)"
    print_command "psql via proxy: SELECT * FROM test_proxy;"

    echo "Reading via proxy (may go to any healthy node):"
    PGPASSWORD=$HELIOS_PASSWORD psql -h $PROXY_HOST -p $PROXY_PORT -U $HELIOS_USER -d $HELIOS_DATABASE -c "
    SELECT id, name, created_at FROM test_proxy ORDER BY id DESC LIMIT 3;
    " 2>/dev/null || print_warning "Query failed"
    echo ""

    print_subheader "6.4 Verify Data on All Nodes"
    echo "Data should be replicated to all nodes:"
    echo ""

    echo "Primary (port $PRIMARY_PORT):"
    PGPASSWORD=$HELIOS_PASSWORD psql -h localhost -p $PRIMARY_PORT -U $HELIOS_USER -d $HELIOS_DATABASE -c "
    SELECT COUNT(*) as rows FROM test_proxy;
    " 2>/dev/null || echo "  (connection failed)"

    echo "Standby-Sync (port $STANDBY_SYNC_PORT):"
    PGPASSWORD=$HELIOS_PASSWORD psql -h localhost -p $STANDBY_SYNC_PORT -U $HELIOS_USER -d $HELIOS_DATABASE -c "
    SELECT COUNT(*) as rows FROM test_proxy;
    " 2>/dev/null || echo "  (connection failed)"

    echo "Standby-Async (port $STANDBY_ASYNC_PORT):"
    PGPASSWORD=$HELIOS_PASSWORD psql -h localhost -p $STANDBY_ASYNC_PORT -U $HELIOS_USER -d $HELIOS_DATABASE -c "
    SELECT COUNT(*) as rows FROM test_proxy;
    " 2>/dev/null || echo "  (connection failed)"

    pause
}

# ============================================================================
# Section 7: Failover Demonstration
# ============================================================================

section_failover() {
    print_header "Section 7: Automatic Failover"

    echo "HeliosProxy automatically handles backend failures:"
    echo "  • Detects unhealthy nodes via health checks"
    echo "  • Routes traffic away from failed nodes"
    echo "  • Re-enables nodes when they recover"
    echo ""

    print_subheader "7.1 Current Failover Count"
    print_command "curl http://localhost:$PROXY_METRICS_PORT/metrics | jq .failovers"
    local failovers=$(curl -s "http://${PROXY_HOST}:${PROXY_METRICS_PORT}/metrics" | jq '.failovers' 2>/dev/null)
    echo "Current failover count: $failovers"
    echo ""

    print_subheader "7.2 Failover Behavior"
    echo "When a backend fails:"
    echo "  1. Health check detects failure"
    echo "  2. Node is marked unhealthy after threshold (default: 3 failures)"
    echo "  3. Traffic is routed to remaining healthy nodes"
    echo "  4. Failover counter increments"
    echo "  5. When node recovers, it's automatically re-added to rotation"
    echo ""

    print_subheader "7.3 Test Failover (Optional)"
    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}To test failover manually:${NC}                                         ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   1. Stop a standby:                                                ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}      docker compose -f docker-compose.ha-cluster.yml stop standby-sync      ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   2. Check node status (should show unhealthy):                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}      curl http://localhost:$PROXY_METRICS_PORT/nodes                           ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   3. Queries via proxy should still work (routed elsewhere)         ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   4. Restart the standby:                                           ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}      docker compose -f docker-compose.ha-cluster.yml start standby-sync     ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"

    pause
}

# ============================================================================
# Section 8: Summary
# ============================================================================

section_summary() {
    print_header "Tutorial Complete: HeliosProxy API Reference"

    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}HEALTH ENDPOINTS${NC}                                                    ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   GET  /health        Basic health check                           ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   GET  /health/ready  Readiness probe (has healthy backends?)     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   GET  /health/live   Liveness probe (process alive?)             ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}METRICS ENDPOINTS${NC}                                                   ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   GET  /metrics            JSON metrics                            ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   GET  /metrics/prometheus Prometheus text format                  ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}NODE MANAGEMENT${NC}                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   GET  /nodes              List all backend nodes                  ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   GET  /nodes/{addr}       Get specific node details               ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   POST /nodes/{addr}/enable   Enable a node                        ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   POST /nodes/{addr}/disable  Disable a node                       ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}CONFIGURATION & STATUS${NC}                                              ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   GET  /config         View proxy configuration                    ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   GET  /version        Proxy version info                          ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   GET  /sessions       Active client sessions                      ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   GET  /pools          Connection pool statistics                  ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}CONNECTION ENDPOINTS${NC}                                                ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   PostgreSQL Wire Protocol: localhost:$PROXY_PORT                      ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Admin/Metrics API:        localhost:$PROXY_METRICS_PORT                    ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    print_success "Tutorial completed successfully!"
    echo ""
    print_info "Related resources:"
    echo "  • Cluster connection helper: ./cluster_connect.sh"
    echo "  • HA tutorial:               ./ha_interactive_tutorial.sh"
    echo "  • Monitor cluster:           ./monitor_cluster.sh"
}

# ============================================================================
# Main
# ============================================================================

main() {
    cd "$(dirname "$0")"

    case "${1:-all}" in
        health)
            section_health
            ;;
        metrics)
            section_metrics
            ;;
        nodes)
            section_nodes
            ;;
        config)
            section_config
            ;;
        sessions)
            section_sessions
            ;;
        routing)
            section_routing
            ;;
        failover)
            section_failover
            ;;
        summary)
            section_summary
            ;;
        all)
            print_header "HeliosProxy Functionality Tutorial"
            echo "This tutorial covers all HeliosProxy features:"
            echo ""
            echo "  1. Health Monitoring"
            echo "  2. Metrics & Prometheus Integration"
            echo "  3. Node Management"
            echo "  4. Configuration Inspection"
            echo "  5. Session Monitoring"
            echo "  6. Read/Write Splitting & Load Balancing"
            echo "  7. Automatic Failover"
            echo ""
            pause

            section_health
            section_metrics
            section_nodes
            section_config
            section_sessions
            section_routing
            section_failover
            section_summary
            ;;
        help|--help|-h)
            echo "HeliosProxy Functionality Tutorial"
            echo ""
            echo "Usage: $0 [section]"
            echo ""
            echo "Sections:"
            echo "  health     Health monitoring endpoints"
            echo "  metrics    Metrics and Prometheus integration"
            echo "  nodes      Node management"
            echo "  config     Configuration inspection"
            echo "  sessions   Session monitoring"
            echo "  routing    Read/write splitting demo"
            echo "  failover   Automatic failover"
            echo "  summary    API reference summary"
            echo "  all        Run full tutorial (default)"
            echo ""
            echo "Options:"
            echo "  INTERACTIVE=false ./heliosproxy_tutorial.sh  # Non-interactive mode"
            ;;
        *)
            echo "Unknown section: $1"
            echo "Use '$0 help' for available sections"
            exit 1
            ;;
    esac
}

main "$@"
