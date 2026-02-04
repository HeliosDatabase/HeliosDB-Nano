#!/bin/bash
#
# HeliosDB-Lite HA Cluster Cleanup Script
#
# This script performs a complete cleanup:
# 1. Stops all containers
# 2. Removes containers
# 3. Removes data volumes
# 4. Optionally removes Docker images
# 5. Removes any leftover networks
#
# Usage:
#   ./cleanup_cluster.sh              # Stop and remove containers + volumes
#   ./cleanup_cluster.sh --images     # Also remove Docker images
#   ./cleanup_cluster.sh --all        # Remove everything including images
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
REMOVE_IMAGES=false
QUIET=false
FORCE=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --images|--all)
            REMOVE_IMAGES=true
            shift
            ;;
        --quiet|-q)
            QUIET=true
            shift
            ;;
        --force|-f)
            FORCE=true
            shift
            ;;
        --help|-h)
            echo "HeliosDB-Lite HA Cluster Cleanup Script"
            echo ""
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  --images, --all   Also remove Docker images"
            echo "  --force, -f       Skip confirmation prompts"
            echo "  --quiet, -q       Minimal output"
            echo "  --help, -h        Show this help"
            echo ""
            echo "This script will:"
            echo "  1. Stop all running containers"
            echo "  2. Remove containers"
            echo "  3. Remove data volumes (all data will be lost!)"
            echo "  4. Remove networks"
            echo "  5. Optionally remove Docker images (with --images)"
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

# Confirmation prompt
confirm() {
    if [ "$FORCE" = true ]; then
        return 0
    fi

    echo ""
    echo -e "${YELLOW}WARNING: This will permanently delete:${NC}"
    echo "  - All HeliosDB cluster containers"
    echo "  - All data volumes (PRIMARY DATA, STANDBY DATA)"
    if [ "$REMOVE_IMAGES" = true ]; then
        echo "  - All HeliosDB Docker images"
    fi
    echo ""
    read -p "Are you sure you want to continue? [y/N] " -n 1 -r
    echo ""
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo "Cleanup cancelled."
        exit 0
    fi
}

# Main execution
main() {
    log ""
    log "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    log "${BOLD}${BLUE}║          HeliosDB-Lite HA Cluster Cleanup                            ║${NC}"
    log "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    log ""

    confirm

    # Step 1: Stop containers
    log_step "Stopping containers..."
    docker compose -f "$COMPOSE_FILE" stop 2>/dev/null || true

    # Step 2: Remove containers and volumes
    log_step "Removing containers and volumes..."
    docker compose -f "$COMPOSE_FILE" down -v --remove-orphans 2>/dev/null || true

    # Step 3: Remove any orphaned containers
    log_step "Removing any orphaned containers..."
    for container in heliosdb-primary heliosdb-standby-sync heliosdb-standby-semisync heliosdb-standby-async heliosdb-observer heliosdb-proxy heliosdb-test-runner; do
        docker rm -f "$container" 2>/dev/null || true
    done

    # Step 4: Remove orphaned volumes
    log_step "Removing orphaned volumes..."
    docker volume rm docker_primary-data docker_standby-sync-data docker_standby-semisync-data docker_standby-async-data 2>/dev/null || true

    # Step 5: Remove networks
    log_step "Removing networks..."
    docker network rm docker_heliosdb-ha 2>/dev/null || true

    # Step 6: Optionally remove images
    if [ "$REMOVE_IMAGES" = true ]; then
        log_step "Removing Docker images..."
        # Get image names from compose file
        docker compose -f "$COMPOSE_FILE" down --rmi all 2>/dev/null || true
        # Also try to remove by name pattern
        docker images --format '{{.Repository}}:{{.Tag}}' | grep -E "docker-(primary|standby|observer|proxy|test-runner)" | xargs -r docker rmi 2>/dev/null || true
    fi

    # Step 7: Clean up any test artifacts
    log_step "Cleaning up test artifacts..."
    rm -f /tmp/heliosdb_*.log /tmp/failover_test.log /tmp/cascade_test.log /tmp/load_test_*.log 2>/dev/null || true

    log ""
    log_success "Cleanup complete!"
    log ""

    # Show remaining Docker resources
    log_step "Remaining Docker resources:"
    echo ""
    echo "Containers:"
    docker ps -a --format "table {{.Names}}\t{{.Status}}" | grep -E "heliosdb|NAMES" || echo "  (none)"
    echo ""
    echo "Volumes:"
    docker volume ls --format "table {{.Name}}" | grep -E "heliosdb|primary|standby|NAME" || echo "  (none)"
    echo ""
    echo "Networks:"
    docker network ls --format "table {{.Name}}" | grep -E "heliosdb|NAME" || echo "  (none)"
    echo ""

    log ""
    log "${BOLD}${GREEN}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    log "${BOLD}${GREEN}║          Cleanup Complete!                                           ║${NC}"
    log "${BOLD}${GREEN}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    log ""
    log "  To recreate the cluster: ${BOLD}./recreate_cluster.sh${NC}"
    if [ "$REMOVE_IMAGES" = true ]; then
        log "  Note: Images were removed. Use ${BOLD}./recreate_cluster.sh --rebuild${NC} to rebuild."
    fi
    log ""
}

main "$@"
