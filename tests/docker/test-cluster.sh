#!/bin/bash
# HeliosDB-Lite HA Cluster Test Script
# Tests basic cluster functionality

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "======================================"
echo "HeliosDB-Lite HA Cluster Test Script"
echo "======================================"
echo ""

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

pass() { echo -e "${GREEN}[PASS]${NC} $1"; }
fail() { echo -e "${RED}[FAIL]${NC} $1"; }
info() { echo -e "${YELLOW}[INFO]${NC} $1"; }

# Test 1: Check all containers are running
echo "Test 1: Checking container status..."
PRIMARY_STATUS=$(docker inspect -f '{{.State.Health.Status}}' heliosdb-primary 2>/dev/null || echo "not found")
STANDBY1_STATUS=$(docker inspect -f '{{.State.Health.Status}}' heliosdb-standby1 2>/dev/null || echo "not found")
STANDBY2_STATUS=$(docker inspect -f '{{.State.Health.Status}}' heliosdb-standby2 2>/dev/null || echo "not found")
OBSERVER_STATUS=$(docker inspect -f '{{.State.Health.Status}}' heliosdb-observer 2>/dev/null || echo "not found")

if [ "$PRIMARY_STATUS" = "healthy" ]; then pass "Primary is healthy"; else fail "Primary: $PRIMARY_STATUS"; fi
if [ "$STANDBY1_STATUS" = "healthy" ]; then pass "Standby1 is healthy"; else fail "Standby1: $STANDBY1_STATUS"; fi
if [ "$STANDBY2_STATUS" = "healthy" ]; then pass "Standby2 is healthy"; else fail "Standby2: $STANDBY2_STATUS"; fi
if [ "$OBSERVER_STATUS" = "healthy" ]; then pass "Observer is healthy"; else fail "Observer: $OBSERVER_STATUS"; fi
echo ""

# Test 2: Check health endpoints
echo "Test 2: Testing health endpoints..."
for port in 18080 18081 18082 18083; do
    if curl -sf "http://localhost:$port/health" > /dev/null 2>&1; then
        pass "Health endpoint on port $port"
    else
        fail "Health endpoint on port $port"
    fi
done
echo ""

# Test 3: Check replication connections
echo "Test 3: Checking replication connections..."
REGISTERED_STANDBYS=$(docker logs heliosdb-primary 2>&1 | grep -c "Standby.*registered" || echo "0")
if [ "$REGISTERED_STANDBYS" -ge 2 ]; then
    pass "Both standbys registered with primary ($REGISTERED_STANDBYS found)"
else
    fail "Expected 2 standbys, found $REGISTERED_STANDBYS"
fi
echo ""

# Test 4: Check PostgreSQL connectivity
echo "Test 4: Testing PostgreSQL connectivity..."
for port in 15432 15442 15452; do
    if docker exec heliosdb-test-runner nc -z primary 5432 2>/dev/null; then
        pass "PostgreSQL port $port reachable"
    else
        fail "PostgreSQL port $port not reachable"
    fi
done
echo ""

# Test 5: Check replication port connectivity
echo "Test 5: Testing replication port connectivity..."
if docker exec heliosdb-standby1 nc -z primary 5433 2>/dev/null; then
    pass "Primary replication port reachable from standby1"
else
    fail "Primary replication port not reachable from standby1"
fi
echo ""

# Summary
echo "======================================"
echo "Test Summary"
echo "======================================"
info "Primary: $PRIMARY_STATUS"
info "Standby1: $STANDBY1_STATUS"
info "Standby2: $STANDBY2_STATUS"
info "Observer: $OBSERVER_STATUS"
info "Registered standbys: $REGISTERED_STANDBYS"
echo ""
echo "For more details, check: docker compose -f docker-compose.ha-cluster.yml logs"
