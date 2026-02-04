#!/bin/bash
#
# HeliosDB-Lite HA Cluster Recreation Script
#
# This script performs a clean cluster recreation:
# 1. Stops and removes all containers
# 2. Removes all data volumes (clean slate)
# 3. Optionally rebuilds Docker images
# 4. Starts the cluster fresh
# 5. Waits for all nodes to be healthy
#
# Usage:
#   ./recreate_cluster.sh              # Recreate with cached images
#   ./recreate_cluster.sh --rebuild    # Rebuild images from scratch
#   ./recreate_cluster.sh --no-cache   # Rebuild with --no-cache
#

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

COMPOSE_FILE="docker-compose.ha-cluster.yml"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Parse arguments
REBUILD=false
NO_CACHE=false
QUIET=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --rebuild)
            REBUILD=true
            shift
            ;;
        --no-cache)
            REBUILD=true
            NO_CACHE=true
            shift
            ;;
        --quiet|-q)
            QUIET=true
            shift
            ;;
        --help|-h)
            echo "HeliosDB-Lite HA Cluster Recreation Script"
            echo ""
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  --rebuild     Rebuild Docker images before starting"
            echo "  --no-cache    Rebuild images with --no-cache (implies --rebuild)"
            echo "  --quiet, -q   Minimal output"
            echo "  --help, -h    Show this help"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

log() {
    if [ "$QUIET" != true ]; then
        echo -e "$1"
    fi
}

log_step() {
    log "${BLUE}[$(date +%H:%M:%S)]${NC} $1"
}

log_success() {
    log "${GREEN}[$(date +%H:%M:%S)]${NC} $1"
}

log_warning() {
    log "${YELLOW}[$(date +%H:%M:%S)]${NC} $1"
}

log_error() {
    echo -e "${RED}[$(date +%H:%M:%S)]${NC} $1" >&2
}

# Check if docker compose is available
check_docker() {
    if ! command -v docker &> /dev/null; then
        log_error "Docker is not installed"
        exit 1
    fi
    if ! docker compose version &> /dev/null; then
        log_error "Docker Compose is not available"
        exit 1
    fi
}

# Wait for a node to be healthy
wait_for_node() {
    local port=$1
    local name=$2
    local max_attempts=${3:-60}
    local attempt=0

    while [ $attempt -lt $max_attempts ]; do
        if PGPASSWORD=helios psql -h localhost -p $port -U helios -d heliosdb -t -c "SELECT 1" 2>/dev/null | grep -q "1"; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 1
    done
    return 1
}

# Main execution
main() {
    log ""
    log "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    log "${BOLD}${BLUE}║          HeliosDB-Lite HA Cluster Recreation                         ║${NC}"
    log "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    log ""

    check_docker

    # Step 1: Stop and remove existing containers
    log_step "Stopping and removing existing containers..."
    docker compose -f "$COMPOSE_FILE" down --remove-orphans 2>/dev/null || true

    # Step 2: Remove volumes (clean data)
    log_step "Removing data volumes for clean slate..."
    docker compose -f "$COMPOSE_FILE" down -v 2>/dev/null || true

    # Also remove any dangling volumes from previous runs
    docker volume rm docker_primary-data docker_standby-sync-data docker_standby-semisync-data docker_standby-async-data 2>/dev/null || true

    # Step 3: Optionally rebuild images
    if [ "$REBUILD" = true ]; then
        log_step "Rebuilding Docker images..."
        if [ "$NO_CACHE" = true ]; then
            log_step "  (using --no-cache for fresh build)"
            docker compose -f "$COMPOSE_FILE" build --no-cache
        else
            docker compose -f "$COMPOSE_FILE" build
        fi
    fi

    # Step 4: Start the cluster
    log_step "Starting the cluster..."
    docker compose -f "$COMPOSE_FILE" up -d

    # Step 5: Wait for health checks
    log_step "Waiting for nodes to become healthy..."
    log ""

    # Wait for primary first (others depend on it)
    log_step "  Waiting for Primary (port 15432)..."
    if wait_for_node 15432 "Primary" 90; then
        log_success "  Primary: READY"
    else
        log_error "  Primary: FAILED TO START"
        log_error "Check logs with: docker logs heliosdb-primary"
        exit 1
    fi

    # Wait for standbys in parallel (they start after primary is healthy)
    log_step "  Waiting for Standby-Sync (port 15442)..."
    if wait_for_node 15442 "Standby-Sync" 90; then
        log_success "  Standby-Sync: READY"
    else
        log_warning "  Standby-Sync: NOT READY (may need more time)"
    fi

    log_step "  Waiting for Standby-Semi (port 15452)..."
    if wait_for_node 15452 "Standby-Semi" 90; then
        log_success "  Standby-Semi: READY"
    else
        log_warning "  Standby-Semi: NOT READY (may need more time)"
    fi

    log_step "  Waiting for Standby-Async (port 15462)..."
    if wait_for_node 15462 "Standby-Async" 90; then
        log_success "  Standby-Async: READY"
    else
        log_warning "  Standby-Async: NOT READY (may need more time)"
    fi

    # Wait for proxy
    log_step "  Waiting for Proxy (port 15400)..."
    if wait_for_node 15400 "Proxy" 60; then
        log_success "  Proxy: READY"
    else
        log_warning "  Proxy: NOT READY (may need more time)"
    fi

    # Step 6: Verify cluster status
    log ""
    log_step "Verifying cluster status..."
    log ""

    docker compose -f "$COMPOSE_FILE" ps

    log ""
    log_step "Checking replication status..."
    sleep 2  # Brief pause for replication to stabilize

    PGPASSWORD=helios psql -h localhost -p 15432 -U helios -d heliosdb -c "
SELECT * FROM pg_replication_standbys;
" 2>/dev/null || log_warning "Could not query replication status (this is normal on first startup)"

    log ""
    log "${BOLD}${GREEN}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    log "${BOLD}${GREEN}║          Cluster Recreation Complete!                                ║${NC}"
    log "${BOLD}${GREEN}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    log ""
    log "  ${CYAN}Primary:${NC}       localhost:15432"
    log "  ${CYAN}Standby-Sync:${NC}  localhost:15442"
    log "  ${CYAN}Standby-Semi:${NC}  localhost:15452"
    log "  ${CYAN}Standby-Async:${NC} localhost:15462"
    log "  ${CYAN}Proxy:${NC}         localhost:15400"
    log ""
    log "  ${YELLOW}Next steps:${NC}"
    log "    Run the interactive tutorial: ${BOLD}./ha_interactive_tutorial.sh${NC}"
    log "    Monitor the cluster:          ${BOLD}./monitor_cluster.sh${NC}"
    log "    View logs:                    ${BOLD}docker compose -f $COMPOSE_FILE logs -f${NC}"
    log ""
}

main "$@"
