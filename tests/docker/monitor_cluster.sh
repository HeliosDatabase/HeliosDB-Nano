#!/bin/bash
# HeliosDB-Lite Cluster Monitor
# Real-time health monitoring for HA cluster

# Configuration (Docker defaults)
PROXY_ADMIN="${PROXY_ADMIN:-localhost:19090}"
PRIMARY_PG="${PRIMARY_PG:-15432}"
STANDBY_SYNC_PG="${STANDBY_SYNC_PG:-15442}"
STANDBY_ASYNC_PG="${STANDBY_ASYNC_PG:-15462}"
PRIMARY_HTTP="${PRIMARY_HTTP:-localhost:18080}"
STANDBY_SYNC_HTTP="${STANDBY_SYNC_HTTP:-localhost:18081}"
STANDBY_ASYNC_HTTP="${STANDBY_ASYNC_HTTP:-localhost:18084}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

check_pg() {
    local port=$1
    local result=$(PGPASSWORD=helios psql -h localhost -p $port -U helios -d heliosdb -t -c "SELECT 1" 2>&1)
    if [[ "$result" == *"1"* ]]; then
        echo -e "${GREEN}UP${NC}"
    else
        echo -e "${RED}DOWN${NC}"
    fi
}

check_http() {
    local url=$1
    local result=$(curl -s -o /dev/null -w "%{http_code}" "$url/health" 2>/dev/null)
    if [[ "$result" == "200" ]]; then
        echo -e "${GREEN}UP${NC}"
    else
        echo -e "${RED}DOWN${NC}"
    fi
}

get_latency() {
    local port=$1
    local start=$(date +%s%3N)
    PGPASSWORD=helios psql -h localhost -p $port -U helios -d heliosdb -t -c "SELECT 1" > /dev/null 2>&1
    local end=$(date +%s%3N)
    local latency=$((end - start))
    if [ $latency -lt 50 ]; then
        echo -e "${GREEN}${latency}ms${NC}"
    elif [ $latency -lt 200 ]; then
        echo -e "${YELLOW}${latency}ms${NC}"
    else
        echo -e "${RED}${latency}ms${NC}"
    fi
}

count_connections() {
    local port=$1
    # This would query pg_stat_activity equivalent
    echo "N/A"
}

while true; do
    clear
    echo -e "${BLUE}╔══════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║              HeliosDB-Lite Cluster Monitor                           ║${NC}"
    echo -e "${BLUE}╚══════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo -e "  ${CYAN}Timestamp:${NC} $(date '+%Y-%m-%d %H:%M:%S')"
    echo ""

    # Node Status
    echo -e "  ${YELLOW}Node Health Status:${NC}"
    echo -e "  ┌─────────────────┬────────────┬────────────┬────────────┬──────────────┐"
    echo -e "  │ Node            │ PostgreSQL │ HTTP API   │ Latency    │ Role         │"
    echo -e "  ├─────────────────┼────────────┼────────────┼────────────┼──────────────┤"
    printf "  │ %-15s │ %-10s │ %-10s │ %-10s │ %-12s │\n" \
        "Primary" "$(check_pg $PRIMARY_PG)" "$(check_http $PRIMARY_HTTP)" "$(get_latency $PRIMARY_PG)" "Primary (R/W)"
    printf "  │ %-15s │ %-10s │ %-10s │ %-10s │ %-12s │\n" \
        "Standby-Sync" "$(check_pg $STANDBY_SYNC_PG)" "$(check_http $STANDBY_SYNC_HTTP)" "$(get_latency $STANDBY_SYNC_PG)" "Standby (R)"
    printf "  │ %-15s │ %-10s │ %-10s │ %-10s │ %-12s │\n" \
        "Standby-Async" "$(check_pg $STANDBY_ASYNC_PG)" "$(check_http $STANDBY_ASYNC_HTTP)" "$(get_latency $STANDBY_ASYNC_PG)" "Standby (R)"
    echo -e "  └─────────────────┴────────────┴────────────┴────────────┴──────────────┘"
    echo ""

    # Proxy Status
    echo -e "  ${YELLOW}Proxy Status:${NC}"
    PROXY_STATUS=$(curl -s "http://$PROXY_ADMIN/health" 2>/dev/null)
    if [[ "$PROXY_STATUS" == *"ok"* ]]; then
        echo -e "  ┌─────────────────┬────────────┐"
        echo -e "  │ HeliosProxy     │ ${GREEN}HEALTHY${NC}    │"
        echo -e "  └─────────────────┴────────────┘"
    else
        echo -e "  ┌─────────────────┬────────────┐"
        echo -e "  │ HeliosProxy     │ ${RED}UNHEALTHY${NC}  │"
        echo -e "  └─────────────────┴────────────┘"
    fi
    echo ""

    # Cluster Overview
    HEALTHY_COUNT=0
    [[ "$(check_pg $PRIMARY_PG)" == *"UP"* ]] && HEALTHY_COUNT=$((HEALTHY_COUNT + 1))
    [[ "$(check_pg $STANDBY_SYNC_PG)" == *"UP"* ]] && HEALTHY_COUNT=$((HEALTHY_COUNT + 1))
    [[ "$(check_pg $STANDBY_ASYNC_PG)" == *"UP"* ]] && HEALTHY_COUNT=$((HEALTHY_COUNT + 1))

    echo -e "  ${YELLOW}Cluster Summary:${NC}"
    echo -e "  ┌─────────────────────────────────────────┐"
    if [ $HEALTHY_COUNT -eq 3 ]; then
        echo -e "  │ Status: ${GREEN}FULLY OPERATIONAL${NC} (3/3 nodes)   │"
    elif [ $HEALTHY_COUNT -ge 1 ]; then
        echo -e "  │ Status: ${YELLOW}DEGRADED${NC} ($HEALTHY_COUNT/3 nodes)            │"
    else
        echo -e "  │ Status: ${RED}OFFLINE${NC} (0/3 nodes)              │"
    fi
    PRIMARY_STATUS=$(check_pg $PRIMARY_PG)
    if [[ "$PRIMARY_STATUS" == *"UP"* ]]; then
        echo -e "  │ Write capability: ${GREEN}AVAILABLE${NC}             │"
    else
        echo -e "  │ Write capability: ${RED}UNAVAILABLE${NC}           │"
    fi
    if [ $HEALTHY_COUNT -ge 1 ]; then
        echo -e "  │ Read capability:  ${GREEN}AVAILABLE${NC}             │"
    else
        echo -e "  │ Read capability:  ${RED}UNAVAILABLE${NC}           │"
    fi
    echo -e "  └─────────────────────────────────────────┘"
    echo ""
    echo -e "  ${BLUE}Refresh: 2s | Press Ctrl+C to exit${NC}"

    sleep 2
done
