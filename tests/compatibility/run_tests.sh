#!/bin/bash
# HeliosDB SQLite Compatibility Test Runner
# Comprehensive script to run all compatibility tests and benchmarks

set -e  # Exit on error

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELIOSDB_BINARY="$PROJECT_ROOT/target/release/heliosdb-lite"
TEST_RESULTS_DIR="$SCRIPT_DIR/test-results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

# Parse command line arguments
RUN_UNIT_TESTS=true
RUN_BENCHMARKS=false
RUN_COVERAGE=false
PARALLEL=false
VERBOSE=false
ITERATIONS=100

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  -h, --help              Show this help message"
    echo "  -b, --benchmarks        Run benchmark suite"
    echo "  -c, --coverage          Generate coverage report"
    echo "  -p, --parallel          Run tests in parallel"
    echo "  -v, --verbose           Verbose output"
    echo "  -i, --iterations N      Number of benchmark iterations (default: 100)"
    echo "  --unit-only             Run only unit tests (skip benchmarks)"
    echo "  --bench-only            Run only benchmarks (skip unit tests)"
    echo ""
    echo "Examples:"
    echo "  $0                      # Run all unit tests"
    echo "  $0 -b                   # Run unit tests and benchmarks"
    echo "  $0 -c -p                # Run with coverage and parallelism"
    echo "  $0 --bench-only -i 1000 # Run only benchmarks with 1000 iterations"
}

while [[ $# -gt 0 ]]; do
    case $1 in
        -h|--help)
            usage
            exit 0
            ;;
        -b|--benchmarks)
            RUN_BENCHMARKS=true
            shift
            ;;
        -c|--coverage)
            RUN_COVERAGE=true
            shift
            ;;
        -p|--parallel)
            PARALLEL=true
            shift
            ;;
        -v|--verbose)
            VERBOSE=true
            shift
            ;;
        -i|--iterations)
            ITERATIONS="$2"
            shift 2
            ;;
        --unit-only)
            RUN_UNIT_TESTS=true
            RUN_BENCHMARKS=false
            shift
            ;;
        --bench-only)
            RUN_UNIT_TESTS=false
            RUN_BENCHMARKS=true
            shift
            ;;
        *)
            echo "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

# Print header
print_header() {
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}$1${NC}"
    echo -e "${BLUE}========================================${NC}"
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_error() {
    echo -e "${RED}✗ $1${NC}"
}

print_info() {
    echo -e "${YELLOW}ℹ $1${NC}"
}

# Check prerequisites
check_prerequisites() {
    print_header "Checking Prerequisites"

    # Check Python
    if ! command -v python3 &> /dev/null; then
        print_error "Python 3 not found"
        exit 1
    fi
    print_success "Python 3 found: $(python3 --version)"

    # Check pip
    if ! command -v pip3 &> /dev/null; then
        print_error "pip3 not found"
        exit 1
    fi
    print_success "pip3 found"

    # Check HeliosDB binary
    if [ ! -f "$HELIOSDB_BINARY" ]; then
        print_info "HeliosDB binary not found at $HELIOSDB_BINARY"
        print_info "Building HeliosDB..."
        cd "$PROJECT_ROOT"
        cargo build --release
        if [ ! -f "$HELIOSDB_BINARY" ]; then
            print_error "Failed to build HeliosDB"
            exit 1
        fi
    fi
    print_success "HeliosDB binary found: $HELIOSDB_BINARY"

    echo ""
}

# Install dependencies
install_dependencies() {
    print_header "Installing Dependencies"

    cd "$SCRIPT_DIR"

    if [ -f "requirements.txt" ]; then
        pip3 install -r requirements.txt --quiet
        print_success "Dependencies installed"
    else
        print_error "requirements.txt not found"
        exit 1
    fi

    echo ""
}

# Run unit tests
run_unit_tests() {
    print_header "Running SQLite Compatibility Tests"

    cd "$SCRIPT_DIR"

    # Build pytest command
    PYTEST_CMD="pytest HELIOSDB_SQLITE_COMPATIBILITY_TESTS.py"

    if [ "$VERBOSE" = true ]; then
        PYTEST_CMD="$PYTEST_CMD -vv"
    else
        PYTEST_CMD="$PYTEST_CMD -v"
    fi

    if [ "$PARALLEL" = true ]; then
        PYTEST_CMD="$PYTEST_CMD -n auto"
        print_info "Running tests in parallel"
    fi

    if [ "$RUN_COVERAGE" = true ]; then
        PYTEST_CMD="$PYTEST_CMD --cov=. --cov-report=term-missing --cov-report=html"
        print_info "Coverage reporting enabled"
    fi

    # Add JUnit XML output
    mkdir -p "$TEST_RESULTS_DIR"
    PYTEST_CMD="$PYTEST_CMD --junitxml=$TEST_RESULTS_DIR/junit_${TIMESTAMP}.xml"

    # Run tests
    echo -e "${YELLOW}Command: $PYTEST_CMD${NC}"
    echo ""

    if eval $PYTEST_CMD; then
        print_success "All tests passed!"
        TEST_EXIT_CODE=0
    else
        print_error "Some tests failed"
        TEST_EXIT_CODE=1
    fi

    echo ""
    return $TEST_EXIT_CODE
}

# Run benchmarks
run_benchmarks() {
    print_header "Running Performance Benchmarks"

    cd "$SCRIPT_DIR"

    # Create results directory
    mkdir -p "$TEST_RESULTS_DIR"

    # Run benchmarks
    BENCHMARK_REPORT="$TEST_RESULTS_DIR/benchmark_${TIMESTAMP}.md"
    BENCHMARK_JSON="$TEST_RESULTS_DIR/benchmark_${TIMESTAMP}.json"

    print_info "Iterations: $ITERATIONS"
    print_info "Output: $BENCHMARK_REPORT"

    if python3 HELIOSDB_SQLITE_BENCHMARK_SUITE.py \
        --iterations "$ITERATIONS" \
        --report-format markdown \
        --output "$BENCHMARK_REPORT"; then

        print_success "Benchmarks completed successfully"

        # Also generate JSON report
        python3 HELIOSDB_SQLITE_BENCHMARK_SUITE.py \
            --iterations "$ITERATIONS" \
            --report-format json \
            --output "$BENCHMARK_JSON" 2>/dev/null

        print_success "Reports generated:"
        echo "  - Markdown: $BENCHMARK_REPORT"
        echo "  - JSON: $BENCHMARK_JSON"

        BENCH_EXIT_CODE=0
    else
        print_error "Benchmarks failed"
        BENCH_EXIT_CODE=1
    fi

    echo ""
    return $BENCH_EXIT_CODE
}

# Generate summary report
generate_summary() {
    print_header "Test Execution Summary"

    if [ -d "$TEST_RESULTS_DIR" ]; then
        print_info "Results saved to: $TEST_RESULTS_DIR"

        # List result files
        echo ""
        echo "Generated files:"
        ls -lh "$TEST_RESULTS_DIR"/*_${TIMESTAMP}* 2>/dev/null || true
    fi

    if [ "$RUN_COVERAGE" = true ] && [ -d "htmlcov" ]; then
        print_success "Coverage report: htmlcov/index.html"
    fi

    echo ""
}

# Main execution
main() {
    print_header "HeliosDB SQLite Compatibility Test Suite"
    echo "Timestamp: $TIMESTAMP"
    echo ""

    check_prerequisites
    install_dependencies

    OVERALL_EXIT_CODE=0

    # Run unit tests
    if [ "$RUN_UNIT_TESTS" = true ]; then
        if ! run_unit_tests; then
            OVERALL_EXIT_CODE=1
        fi
    fi

    # Run benchmarks
    if [ "$RUN_BENCHMARKS" = true ]; then
        if ! run_benchmarks; then
            OVERALL_EXIT_CODE=1
        fi
    fi

    generate_summary

    # Final result
    if [ $OVERALL_EXIT_CODE -eq 0 ]; then
        print_header "✅ All Tests Completed Successfully"
    else
        print_header "❌ Some Tests Failed"
    fi

    exit $OVERALL_EXIT_CODE
}

# Run main function
main
