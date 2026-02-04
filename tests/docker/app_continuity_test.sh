#!/bin/bash
# Application Continuity Test
# Simulates a real application with mixed read/write workload
# Tests HA behavior under various failure scenarios

set -e

# Configuration
PROXY_HOST="${PROXY_HOST:-localhost}"
PROXY_PORT="${PROXY_PORT:-15400}"
TEST_DURATION="${TEST_DURATION:-180}"
READ_WEIGHT="${READ_WEIGHT:-70}"  # 70% reads, 30% writes
OPERATION_DELAY="${OPERATION_DELAY:-0.5}"

# Counters
ITERATIONS=0
SUCCESS=0
FAILED=0
WRITES=0
READS=0
WRITE_LATENCY_TOTAL=0
READ_LATENCY_TOTAL=0
MAX_LATENCY=0

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m'

cleanup() {
    echo ""
    echo ""
    print_results
    exit 0
}

trap cleanup INT TERM

print_results() {
    echo -e "${BLUE}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║           Application Continuity Test Results                  ║${NC}"
    echo -e "${BLUE}╚════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo -e "  ${YELLOW}Test Configuration:${NC}"
    echo "    Duration:        ${TEST_DURATION}s"
    local write_weight=$((100 - READ_WEIGHT))
    echo "    Read/Write:      ${READ_WEIGHT}/${write_weight}%"
    echo "    Operation delay: ${OPERATION_DELAY}s"
    echo ""
    echo -e "  ${YELLOW}Results:${NC}"
    echo "    Total operations:  $ITERATIONS"
    echo "    Successful:        $SUCCESS"
    echo "    Failed:            $FAILED"
    echo "    Read operations:   $READS"
    echo "    Write operations:  $WRITES"
    echo ""

    if [ $ITERATIONS -gt 0 ]; then
        SUCCESS_RATE=$(echo "scale=2; $SUCCESS * 100 / $ITERATIONS" | bc)
        echo -e "    Success rate:      ${GREEN}${SUCCESS_RATE}%${NC}"
    fi

    if [ $READS -gt 0 ]; then
        AVG_READ_LATENCY=$(echo "scale=2; $READ_LATENCY_TOTAL / $READS" | bc)
        echo "    Avg read latency:  ${AVG_READ_LATENCY}ms"
    fi

    if [ $WRITES -gt 0 ]; then
        AVG_WRITE_LATENCY=$(echo "scale=2; $WRITE_LATENCY_TOTAL / $WRITES" | bc)
        echo "    Avg write latency: ${AVG_WRITE_LATENCY}ms"
    fi

    echo "    Max latency:       ${MAX_LATENCY}ms"
    echo ""

    # Analysis
    if [ $FAILED -eq 0 ]; then
        echo -e "  ${GREEN}PASS: Zero failures - application continuity maintained${NC}"
    elif [ $SUCCESS_RATE -gt 95 ]; then
        echo -e "  ${YELLOW}WARN: Some failures occurred but >95% success rate${NC}"
    else
        echo -e "  ${RED}FAIL: Significant failures - review cluster health${NC}"
    fi
}

setup_test_table() {
    echo -e "${BLUE}[Setup]${NC} Creating test table..."
    PGPASSWORD=helios psql -h $PROXY_HOST -p $PROXY_PORT -U helios -d heliosdb <<EOF 2>/dev/null
DROP TABLE IF EXISTS app_orders;
CREATE TABLE app_orders (
    id INTEGER PRIMARY KEY,
    customer TEXT,
    amount REAL,
    status TEXT DEFAULT 'pending',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_orders_status ON app_orders(status);
EOF
    echo -e "${GREEN}[Setup]${NC} Test table ready"
}

perform_read() {
    local start_ms=$(date +%s%3N)

    local result=$(PGPASSWORD=helios psql -h $PROXY_HOST -p $PROXY_PORT -U helios -d heliosdb -t -c \
        "SELECT COUNT(*) FROM app_orders WHERE status = 'completed'" 2>&1)

    local end_ms=$(date +%s%3N)
    local latency=$((end_ms - start_ms))

    if [[ "$result" =~ ^[[:space:]]*[0-9]+[[:space:]]*$ ]]; then
        SUCCESS=$((SUCCESS + 1))
        READS=$((READS + 1))
        READ_LATENCY_TOTAL=$((READ_LATENCY_TOTAL + latency))
        [ $latency -gt $MAX_LATENCY ] && MAX_LATENCY=$latency
        return 0
    else
        FAILED=$((FAILED + 1))
        return 1
    fi
}

perform_write() {
    local order_id=$1
    local start_ms=$(date +%s%3N)

    local customer="customer_$((RANDOM % 100))"
    local amount="$((RANDOM % 1000)).$((RANDOM % 100))"

    local result=$(PGPASSWORD=helios psql -h $PROXY_HOST -p $PROXY_PORT -U helios -d heliosdb -t -c \
        "INSERT INTO app_orders (id, customer, amount) VALUES ($order_id, '$customer', $amount)
         ON CONFLICT (id) DO UPDATE SET amount = $amount, status = 'updated'" 2>&1)

    local end_ms=$(date +%s%3N)
    local latency=$((end_ms - start_ms))

    if [[ -z "$(echo $result | grep -i error)" ]]; then
        SUCCESS=$((SUCCESS + 1))
        WRITES=$((WRITES + 1))
        WRITE_LATENCY_TOTAL=$((WRITE_LATENCY_TOTAL + latency))
        [ $latency -gt $MAX_LATENCY ] && MAX_LATENCY=$latency
        return 0
    else
        FAILED=$((FAILED + 1))
        return 1
    fi
}

main() {
    echo -e "${BLUE}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║           Application Continuity Test                          ║${NC}"
    echo -e "${BLUE}╚════════════════════════════════════════════════════════════════╝${NC}"
    echo ""

    setup_test_table

    echo ""
    echo -e "${BLUE}[Test]${NC} Starting workload (${TEST_DURATION}s)..."
    echo -e "${BLUE}[Test]${NC} Press Ctrl+C to stop and see results"
    echo ""

    START_TIME=$(date +%s)

    while true; do
        CURRENT_TIME=$(date +%s)
        ELAPSED=$((CURRENT_TIME - START_TIME))

        if [ $ELAPSED -ge $TEST_DURATION ]; then
            break
        fi

        ITERATIONS=$((ITERATIONS + 1))

        # Determine operation type based on read weight
        RANDOM_VAL=$((RANDOM % 100))

        if [ $RANDOM_VAL -lt $READ_WEIGHT ]; then
            # Perform READ
            if perform_read; then
                OP_STATUS="${GREEN}OK${NC}"
            else
                OP_STATUS="${RED}FAIL${NC}"
            fi
            OP_TYPE="READ "
        else
            # Perform WRITE
            if perform_write $ITERATIONS; then
                OP_STATUS="${GREEN}OK${NC}"
            else
                OP_STATUS="${RED}FAIL${NC}"
            fi
            OP_TYPE="WRITE"
        fi

        # Progress display
        REMAINING=$((TEST_DURATION - ELAPSED))
        printf "\r  [%3ds remaining] #%-5d %s %b | R:%-4d W:%-4d F:%-3d" \
            $REMAINING $ITERATIONS "$OP_TYPE" "$OP_STATUS" $READS $WRITES $FAILED

        sleep $OPERATION_DELAY
    done

    echo ""
    print_results
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --duration)
            TEST_DURATION="$2"
            shift 2
            ;;
        --read-weight)
            READ_WEIGHT="$2"
            shift 2
            ;;
        --delay)
            OPERATION_DELAY="$2"
            shift 2
            ;;
        --help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --duration N      Test duration in seconds (default: 180)"
            echo "  --read-weight N   Percentage of read operations (default: 70)"
            echo "  --delay N         Delay between operations in seconds (default: 0.5)"
            echo ""
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

main
