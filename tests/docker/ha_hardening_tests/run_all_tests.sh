#!/bin/bash
# HeliosDB-Lite HA Hardening Tests - Master Runner
# Executes all Phase 1-4 tests autonomously

set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOCKER_DIR="$(dirname "$SCRIPT_DIR")"
PROJECT_ROOT="$(dirname "$(dirname "$DOCKER_DIR")")"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
MAGENTA='\033[0;35m'
CYAN='\033[0;36m'
NC='\033[0m'

# Configuration
export LOG_DIR="${LOG_DIR:-/tmp/ha-hardening-tests-$(date +%Y%m%d-%H%M%S)}"
COMPOSE_FILE="$DOCKER_DIR/docker-compose.ha-cluster.yml"

# ============================================================================
# Banner
# ============================================================================

print_banner() {
    echo ""
    echo -e "${CYAN}╔══════════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║                                                                          ║${NC}"
    echo -e "${CYAN}║     ${MAGENTA}██╗  ██╗███████╗██╗     ██╗ ██████╗ ███████╗██████╗ ██████╗${CYAN}          ║${NC}"
    echo -e "${CYAN}║     ${MAGENTA}██║  ██║██╔════╝██║     ██║██╔═══██╗██╔════╝██╔══██╗██╔══██╗${CYAN}         ║${NC}"
    echo -e "${CYAN}║     ${MAGENTA}███████║█████╗  ██║     ██║██║   ██║███████╗██║  ██║██████╔╝${CYAN}         ║${NC}"
    echo -e "${CYAN}║     ${MAGENTA}██╔══██║██╔══╝  ██║     ██║██║   ██║╚════██║██║  ██║██╔══██╗${CYAN}         ║${NC}"
    echo -e "${CYAN}║     ${MAGENTA}██║  ██║███████╗███████╗██║╚██████╔╝███████║██████╔╝██████╔╝${CYAN}         ║${NC}"
    echo -e "${CYAN}║     ${MAGENTA}╚═╝  ╚═╝╚══════╝╚══════╝╚═╝ ╚═════╝ ╚══════╝╚═════╝ ╚═════╝${CYAN}          ║${NC}"
    echo -e "${CYAN}║                                                                          ║${NC}"
    echo -e "${CYAN}║              ${YELLOW}HA HARDENING TEST SUITE${CYAN}                                    ║${NC}"
    echo -e "${CYAN}║              ${GREEN}75 Comprehensive Tests${CYAN}                                      ║${NC}"
    echo -e "${CYAN}║                                                                          ║${NC}"
    echo -e "${CYAN}╚══════════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
}

# ============================================================================
# Cluster Management
# ============================================================================

start_cluster() {
    echo -e "${BLUE}[SETUP]${NC} Starting HA cluster..."

    cd "$DOCKER_DIR"

    # Build and start
    docker compose -f "$COMPOSE_FILE" up -d --build 2>&1 | tee -a "$LOG_DIR/setup.log"

    if [[ $? -ne 0 ]]; then
        echo -e "${RED}[ERROR]${NC} Failed to start cluster"
        return 1
    fi

    echo -e "${BLUE}[SETUP]${NC} Waiting for cluster to be healthy..."

    # Wait for all services
    local max_wait=180
    local waited=0
    local all_healthy=false

    while [[ $waited -lt $max_wait ]]; do
        local healthy_count=0

        curl -sf "http://localhost:18080/health" >/dev/null 2>&1 && healthy_count=$((healthy_count + 1))
        curl -sf "http://localhost:18081/health" >/dev/null 2>&1 && healthy_count=$((healthy_count + 1))
        curl -sf "http://localhost:18082/health" >/dev/null 2>&1 && healthy_count=$((healthy_count + 1))
        curl -sf "http://localhost:18084/health" >/dev/null 2>&1 && healthy_count=$((healthy_count + 1))

        if [[ $healthy_count -ge 4 ]]; then
            all_healthy=true
            break
        fi

        echo -e "${YELLOW}[SETUP]${NC} $healthy_count/4 nodes healthy... waiting ($waited/${max_wait}s)"
        sleep 10
        waited=$((waited + 10))
    done

    if $all_healthy; then
        echo -e "${GREEN}[SETUP]${NC} Cluster is healthy!"
        return 0
    else
        echo -e "${RED}[ERROR]${NC} Cluster not healthy after ${max_wait}s"
        docker compose -f "$COMPOSE_FILE" logs 2>&1 | tail -100
        return 1
    fi
}

stop_cluster() {
    echo -e "${BLUE}[CLEANUP]${NC} Stopping HA cluster..."
    cd "$DOCKER_DIR"
    docker compose -f "$COMPOSE_FILE" down -v 2>&1 | tee -a "$LOG_DIR/cleanup.log"
}

# ============================================================================
# Test Execution
# ============================================================================

run_phase() {
    local phase="$1"
    local script="$2"
    local description="$3"

    echo ""
    echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${MAGENTA}  PHASE $phase: $description${NC}"
    echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""

    local start_time=$(date +%s)

    # Source and run
    source "$SCRIPT_DIR/lib/test_harness.sh"
    source "$SCRIPT_DIR/$script"

    # Get the run function name
    local run_func="run_phase${phase}_tests"
    $run_func

    local status=$?
    local end_time=$(date +%s)
    local duration=$((end_time - start_time))

    echo ""
    echo -e "${BLUE}Phase $phase completed in ${duration}s${NC}"

    return $status
}

generate_report() {
    local total_passed=0
    local total_failed=0
    local total_skipped=0
    local total_tests=0

    echo ""
    echo -e "${CYAN}╔══════════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║                      FINAL TEST REPORT                                   ║${NC}"
    echo -e "${CYAN}╚══════════════════════════════════════════════════════════════════════════╝${NC}"
    echo ""

    # Aggregate results from all phases
    for phase_file in "$LOG_DIR"/phase*_results.json; do
        if [[ -f "$phase_file" ]]; then
            local passed=$(jq -r '.summary.passed // 0' "$phase_file")
            local failed=$(jq -r '.summary.failed // 0' "$phase_file")
            local skipped=$(jq -r '.summary.skipped // 0' "$phase_file")
            local total=$(jq -r '.summary.total // 0' "$phase_file")

            total_passed=$((total_passed + passed))
            total_failed=$((total_failed + failed))
            total_skipped=$((total_skipped + skipped))
            total_tests=$((total_tests + total))
        fi
    done

    # If no phase files, use current RESULTS_FILE
    if [[ $total_tests -eq 0 ]] && [[ -f "$RESULTS_FILE" ]]; then
        total_passed=$(jq -r '.summary.passed // 0' "$RESULTS_FILE")
        total_failed=$(jq -r '.summary.failed // 0' "$RESULTS_FILE")
        total_skipped=$(jq -r '.summary.skipped // 0' "$RESULTS_FILE")
        total_tests=$(jq -r '.summary.total // 0' "$RESULTS_FILE")
    fi

    local pass_rate=0
    if [[ $total_tests -gt 0 ]]; then
        pass_rate=$(( (total_passed * 100) / total_tests ))
    fi

    echo -e "  ${GREEN}Passed:${NC}   $total_passed"
    echo -e "  ${RED}Failed:${NC}   $total_failed"
    echo -e "  ${YELLOW}Skipped:${NC}  $total_skipped"
    echo -e "  ${BLUE}Total:${NC}    $total_tests"
    echo ""
    echo -e "  ${CYAN}Pass Rate:${NC} ${pass_rate}%"
    echo ""
    echo -e "  ${BLUE}Log Directory:${NC} $LOG_DIR"
    echo ""

    # List failed tests
    if [[ $total_failed -gt 0 ]]; then
        echo -e "${RED}Failed Tests:${NC}"
        if [[ -f "$RESULTS_FILE" ]]; then
            jq -r '.tests[] | select(.status == "FAILED") | "  - \(.id): \(.message)"' "$RESULTS_FILE" 2>/dev/null
        fi
        echo ""
    fi

    # Final status
    if [[ $total_failed -eq 0 ]]; then
        echo -e "${GREEN}╔══════════════════════════════════════════════════════════════════════════╗${NC}"
        echo -e "${GREEN}║                     ALL TESTS PASSED!                                    ║${NC}"
        echo -e "${GREEN}╚══════════════════════════════════════════════════════════════════════════╝${NC}"
        return 0
    else
        echo -e "${RED}╔══════════════════════════════════════════════════════════════════════════╗${NC}"
        echo -e "${RED}║                     SOME TESTS FAILED                                    ║${NC}"
        echo -e "${RED}╚══════════════════════════════════════════════════════════════════════════╝${NC}"
        return 1
    fi
}

# ============================================================================
# Main
# ============================================================================

main() {
    local start_time=$(date +%s)

    print_banner

    # Create log directory
    mkdir -p "$LOG_DIR"
    echo "Test run started at $(date)" > "$LOG_DIR/run.log"

    # Parse arguments
    local phases="${1:-all}"
    local skip_cluster_setup="${SKIP_CLUSTER_SETUP:-false}"

    # Start cluster if needed
    if [[ "$skip_cluster_setup" != "true" ]]; then
        start_cluster || { echo -e "${RED}Failed to start cluster${NC}"; exit 1; }
    fi

    # Initialize harness
    source "$SCRIPT_DIR/lib/test_harness.sh"
    init_test_harness

    # Run phases
    local exit_code=0

    if [[ "$phases" == "all" ]] || [[ "$phases" == *"1"* ]]; then
        run_phase "1" "phase1_critical_foundation.sh" "Critical Foundation"
        [[ $? -ne 0 ]] && exit_code=1
    fi

    if [[ "$phases" == "all" ]] || [[ "$phases" == *"2"* ]]; then
        run_phase "2" "phase2_transaction_integrity.sh" "Transaction Integrity"
        [[ $? -ne 0 ]] && exit_code=1
    fi

    if [[ "$phases" == "all" ]] || [[ "$phases" == *"3"* ]]; then
        run_phase "3" "phase3_resilience.sh" "Resilience"
        [[ $? -ne 0 ]] && exit_code=1
    fi

    if [[ "$phases" == "all" ]] || [[ "$phases" == *"4"* ]]; then
        run_phase "4" "phase4_hardening.sh" "Hardening"
        [[ $? -ne 0 ]] && exit_code=1
    fi

    # Generate final report
    generate_report
    local report_status=$?

    # Calculate total time
    local end_time=$(date +%s)
    local total_duration=$((end_time - start_time))
    local minutes=$((total_duration / 60))
    local seconds=$((total_duration % 60))

    echo ""
    echo -e "${BLUE}Total execution time: ${minutes}m ${seconds}s${NC}"
    echo ""

    # Cleanup (optional - comment out to keep cluster running)
    # stop_cluster

    exit $exit_code
}

# Usage
usage() {
    echo "Usage: $0 [phases]"
    echo ""
    echo "  phases: all | 1 | 2 | 3 | 4 | 1,2 | 1,3,4 | etc."
    echo ""
    echo "Examples:"
    echo "  $0           # Run all phases"
    echo "  $0 1         # Run only Phase 1"
    echo "  $0 1,2       # Run Phases 1 and 2"
    echo "  $0 all       # Run all phases"
    echo ""
    echo "Environment variables:"
    echo "  LOG_DIR              - Custom log directory"
    echo "  SKIP_CLUSTER_SETUP   - Set to 'true' to skip cluster start/stop"
    echo ""
}

if [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
    usage
    exit 0
fi

main "$@"
