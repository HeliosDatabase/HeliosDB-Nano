#!/bin/bash
# ============================================================================
# HeliosDB-Lite HA Cluster Connection Helper
# ============================================================================
#
# This script provides ready-to-use connection commands for the HA cluster.
#
# Usage:
#   source ./cluster_connect.sh     # Load variables into current shell
#   ./cluster_connect.sh            # Show connection examples
#   ./cluster_connect.sh primary    # Connect to primary
#   ./cluster_connect.sh sync       # Connect to standby-sync
#   ./cluster_connect.sh semi       # Connect to standby-semisync
#   ./cluster_connect.sh async      # Connect to standby-async
#   ./cluster_connect.sh proxy      # Connect via proxy
#   ./cluster_connect.sh all        # Show status of all nodes
#
# ============================================================================

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

# ============================================================================
# Cluster Configuration Variables
# ============================================================================

# Database credentials
export HELIOS_USER="helios"
export HELIOS_PASSWORD="helios"
export HELIOS_DATABASE="heliosdb"

# Primary node
export PRIMARY_HOST="localhost"
export PRIMARY_PORT="15432"
export PRIMARY_REPL_PORT="15433"
export PRIMARY_HTTP_PORT="18080"

# Standby-Sync node (full sync replication, supports TWR)
export STANDBY_SYNC_HOST="localhost"
export STANDBY_SYNC_PORT="15442"
export STANDBY_SYNC_REPL_PORT="15443"
export STANDBY_SYNC_HTTP_PORT="18081"

# Standby-Semi node (semi-sync replication, supports TWR)
export STANDBY_SEMI_HOST="localhost"
export STANDBY_SEMI_PORT="15452"
export STANDBY_SEMI_REPL_PORT="15453"
export STANDBY_SEMI_HTTP_PORT="18082"

# Standby-Async node (async replication, read-only)
export STANDBY_ASYNC_HOST="localhost"
export STANDBY_ASYNC_PORT="15462"
export STANDBY_ASYNC_REPL_PORT="15463"
export STANDBY_ASYNC_HTTP_PORT="18084"

# Observer node
export OBSERVER_HOST="localhost"
export OBSERVER_REPL_PORT="15473"
export OBSERVER_HTTP_PORT="18083"

# Proxy node
export PROXY_HOST="localhost"
export PROXY_PORT="15400"
export PROXY_METRICS_PORT="19090"

# Docker compose file
export COMPOSE_FILE="docker-compose.ha-cluster.yml"

# ============================================================================
# Helper Functions
# ============================================================================

# Connect to a node via psql
connect_psql() {
    local host=$1
    local port=$2
    local label=$3
    echo -e "${CYAN}Connecting to ${label} (${host}:${port})...${NC}"
    PGPASSWORD=$HELIOS_PASSWORD psql -h $host -p $port -U $HELIOS_USER -d $HELIOS_DATABASE
}

# Execute a query on a node
query_node() {
    local host=$1
    local port=$2
    local query=$3
    PGPASSWORD=$HELIOS_PASSWORD psql -h $host -p $port -U $HELIOS_USER -d $HELIOS_DATABASE -c "$query" 2>/dev/null
}

# Check node status
check_node() {
    local host=$1
    local port=$2
    local label=$3
    if PGPASSWORD=$HELIOS_PASSWORD psql -h $host -p $port -U $HELIOS_USER -d $HELIOS_DATABASE -c "SELECT 1" &>/dev/null; then
        echo -e "  ${GREEN}✓${NC} ${label}: ${GREEN}UP${NC} (${host}:${port})"
    else
        echo -e "  ${RED}✗${NC} ${label}: ${RED}DOWN${NC} (${host}:${port})"
    fi
}

# Show all connection examples
show_examples() {
    echo -e "${BLUE}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║${NC} ${BOLD}HeliosDB-Lite HA Cluster Connection Helper${NC}"
    echo -e "${BLUE}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""

    echo -e "${BOLD}Quick Connect Commands:${NC}"
    echo -e "  ${CYAN}./cluster_connect.sh primary${NC}  - Connect to primary (read/write)"
    echo -e "  ${CYAN}./cluster_connect.sh sync${NC}     - Connect to standby-sync (read + TWR)"
    echo -e "  ${CYAN}./cluster_connect.sh semi${NC}     - Connect to standby-semi (read + TWR)"
    echo -e "  ${CYAN}./cluster_connect.sh async${NC}    - Connect to standby-async (read-only)"
    echo -e "  ${CYAN}./cluster_connect.sh proxy${NC}    - Connect via HeliosProxy"
    echo -e "  ${CYAN}./cluster_connect.sh all${NC}      - Show all node status"
    echo ""

    echo -e "${BOLD}Load Variables (for scripting):${NC}"
    echo -e "  ${CYAN}source ./cluster_connect.sh${NC}"
    echo ""

    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}PSQL Connection Commands (copy & paste):${NC}                          ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${YELLOW}Primary (Read/Write):${NC}                                              ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   PGPASSWORD=helios psql -h localhost -p 15432 -U helios -d heliosdb ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${YELLOW}Standby-Sync (Read + TWR):${NC}                                         ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   PGPASSWORD=helios psql -h localhost -p 15442 -U helios -d heliosdb ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${YELLOW}Standby-Semi (Read + TWR):${NC}                                         ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   PGPASSWORD=helios psql -h localhost -p 15452 -U helios -d heliosdb ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${YELLOW}Standby-Async (Read-only):${NC}                                         ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   PGPASSWORD=helios psql -h localhost -p 15462 -U helios -d heliosdb ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC} ${YELLOW}Proxy (Auto-routing):${NC}                                              ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   PGPASSWORD=helios psql -h localhost -p 15400 -U helios -d heliosdb ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}HTTP API Endpoints:${NC}                                                 ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   Primary:       curl http://localhost:18080/health                ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Standby-Sync:  curl http://localhost:18081/health                ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Standby-Semi:  curl http://localhost:18082/health                ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Standby-Async: curl http://localhost:18084/health                ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Observer:      curl http://localhost:18083/health                ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Proxy Metrics: curl http://localhost:19090/metrics               ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Docker Commands:${NC}                                                    ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   Logs (all):    docker compose -f $COMPOSE_FILE logs -f   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Logs (node):   docker logs -f heliosdb-primary                   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Stop primary:  docker compose -f $COMPOSE_FILE stop primary ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Start primary: docker compose -f $COMPOSE_FILE start primary ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   Restart all:   docker compose -f $COMPOSE_FILE restart    ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""

    echo -e "${CYAN}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC} ${BOLD}Useful Queries:${NC}                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}   ${YELLOW}Replication Status:${NC}                                               ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   SELECT * FROM helios_stat_replication;                           ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   ${YELLOW}Node Role:${NC}                                                        ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   SELECT * FROM helios_stat_role;                                   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   ${YELLOW}Cluster Health:${NC}                                                   ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}   SELECT * FROM helios_stat_cluster;                                ${CYAN}│${NC}"
    echo -e "${CYAN}└─────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""
}

# Show status of all nodes
show_all_status() {
    echo -e "${BLUE}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║${NC} ${BOLD}HeliosDB-Lite HA Cluster Status${NC}"
    echo -e "${BLUE}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""

    echo -e "${BOLD}Node Connectivity:${NC}"
    check_node $PRIMARY_HOST $PRIMARY_PORT "Primary"
    check_node $STANDBY_SYNC_HOST $STANDBY_SYNC_PORT "Standby-Sync"
    check_node $STANDBY_SEMI_HOST $STANDBY_SEMI_PORT "Standby-Semi"
    check_node $STANDBY_ASYNC_HOST $STANDBY_ASYNC_PORT "Standby-Async"
    check_node $PROXY_HOST $PROXY_PORT "Proxy"
    echo ""

    echo -e "${BOLD}Replication Status (from Primary):${NC}"
    query_node $PRIMARY_HOST $PRIMARY_PORT "SELECT node_id, sync_mode, state, lag_bytes FROM helios_stat_replication;"
    echo ""
}

# ============================================================================
# Main
# ============================================================================

# If sourced, just export variables
if [[ "${BASH_SOURCE[0]}" != "${0}" ]]; then
    echo -e "${GREEN}Cluster variables loaded!${NC}"
    echo "  PRIMARY_PORT=$PRIMARY_PORT"
    echo "  STANDBY_SYNC_PORT=$STANDBY_SYNC_PORT"
    echo "  STANDBY_SEMI_PORT=$STANDBY_SEMI_PORT"
    echo "  STANDBY_ASYNC_PORT=$STANDBY_ASYNC_PORT"
    echo "  PROXY_PORT=$PROXY_PORT"
    return 0
fi

# Change to script directory
cd "$(dirname "$0")"

# Handle command line arguments
case "${1:-help}" in
    primary|p)
        connect_psql $PRIMARY_HOST $PRIMARY_PORT "Primary"
        ;;
    sync|s)
        connect_psql $STANDBY_SYNC_HOST $STANDBY_SYNC_PORT "Standby-Sync"
        ;;
    semi|m)
        connect_psql $STANDBY_SEMI_HOST $STANDBY_SEMI_PORT "Standby-Semi"
        ;;
    async|a)
        connect_psql $STANDBY_ASYNC_HOST $STANDBY_ASYNC_PORT "Standby-Async"
        ;;
    proxy|x)
        connect_psql $PROXY_HOST $PROXY_PORT "Proxy"
        ;;
    all|status)
        show_all_status
        ;;
    help|--help|-h|"")
        show_examples
        ;;
    *)
        echo -e "${RED}Unknown command: $1${NC}"
        echo "Use './cluster_connect.sh help' for usage"
        exit 1
        ;;
esac
