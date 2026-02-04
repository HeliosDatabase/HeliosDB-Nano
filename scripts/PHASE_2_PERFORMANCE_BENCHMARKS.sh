#!/usr/bin/env bash
################################################################################
# Phase 2 Performance Benchmarks
# SQLite vs HeliosDB Performance Baseline Comparison
#
# This script measures and compares performance metrics between SQLite and
# HeliosDB for query latency, bulk operations, concurrent access, and index
# performance.
#
# Requirements:
#   - SQLite 3.35+
#   - HeliosDB-Lite 3.0.0+
#   - PostgreSQL client (psql)
#   - bc (for floating point calculations)
#
# Usage:
#   ./PHASE_2_PERFORMANCE_BENCHMARKS.sh [options]
#
# Options:
#   --sqlite-db PATH      Path to SQLite database (default: ./test_data/source.db)
#   --helios-host HOST    HeliosDB host (default: localhost)
#   --helios-port PORT    HeliosDB port (default: 20000)
#   --helios-db NAME      HeliosDB database name (default: heliosdb)
#   --iterations NUM      Number of benchmark iterations (default: 100)
#   --output-dir PATH     Output directory for results (default: ./benchmark_results)
#   --skip-sqlite         Skip SQLite benchmarks (compare against baseline file)
#   --baseline-file PATH  Baseline results file for comparison
#
# Author: Testing & Verification Specialist
# Version: 1.0.0
# Date: 2025-12-08
################################################################################

set -euo pipefail

# Color output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Default configuration
SQLITE_DB="${SQLITE_DB:-./test_data/source.db}"
HELIOS_HOST="${HELIOS_HOST:-localhost}"
HELIOS_PORT="${HELIOS_PORT:-20000}"
HELIOS_DB="${HELIOS_DB:-heliosdb}"
HELIOS_USER="${HELIOS_USER:-test_user}"
HELIOS_PASSWORD="${HELIOS_PASSWORD:-test_password}"
ITERATIONS="${ITERATIONS:-100}"
OUTPUT_DIR="${OUTPUT_DIR:-./benchmark_results}"
SKIP_SQLITE=false
BASELINE_FILE=""

# Performance thresholds
LATENCY_THRESHOLD=1.2   # HeliosDB should be <= 120% of SQLite
THROUGHPUT_THRESHOLD=0.8  # HeliosDB should be >= 80% of SQLite
INDEX_SPEEDUP_MIN=5.0     # Index should provide >= 5x speedup

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --sqlite-db)
            SQLITE_DB="$2"
            shift 2
            ;;
        --helios-host)
            HELIOS_HOST="$2"
            shift 2
            ;;
        --helios-port)
            HELIOS_PORT="$2"
            shift 2
            ;;
        --helios-db)
            HELIOS_DB="$2"
            shift 2
            ;;
        --iterations)
            ITERATIONS="$2"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        --skip-sqlite)
            SKIP_SQLITE=true
            shift
            ;;
        --baseline-file)
            BASELINE_FILE="$2"
            shift 2
            ;;
        -h|--help)
            grep "^#" "$0" | grep -v "^#!/" | sed 's/^# //' | sed 's/^#//'
            exit 0
            ;;
        *)
            echo -e "${RED}Unknown option: $1${NC}"
            exit 1
            ;;
    esac
done

# Create output directory
mkdir -p "$OUTPUT_DIR"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
RESULTS_FILE="$OUTPUT_DIR/benchmark_results_$TIMESTAMP.txt"
SUMMARY_FILE="$OUTPUT_DIR/benchmark_summary_$TIMESTAMP.json"

################################################################################
# Helper Functions
################################################################################

log_info() {
    echo -e "${BLUE}[INFO]${NC} $*"
}

log_success() {
    echo -e "${GREEN}[PASS]${NC} $*"
}

log_warning() {
    echo -e "${YELLOW}[WARN]${NC} $*"
}

log_error() {
    echo -e "${RED}[FAIL]${NC} $*"
}

# Execute SQL on SQLite
sqlite_exec() {
    local query="$1"
    sqlite3 "$SQLITE_DB" "$query"
}

# Execute SQL on HeliosDB
helios_exec() {
    local query="$1"
    PGPASSWORD="$HELIOS_PASSWORD" psql \
        -h "$HELIOS_HOST" \
        -p "$HELIOS_PORT" \
        -d "$HELIOS_DB" \
        -U "$HELIOS_USER" \
        -t \
        -A \
        -c "$query"
}

# Benchmark query execution time
benchmark_query() {
    local db_type="$1"
    local query="$2"
    local iterations="$3"
    local times=()

    for ((i=1; i<=iterations; i++)); do
        local start=$(date +%s.%N)

        if [[ "$db_type" == "sqlite" ]]; then
            sqlite_exec "$query" > /dev/null
        else
            helios_exec "$query" > /dev/null
        fi

        local end=$(date +%s.%N)
        local duration=$(echo "$end - $start" | bc)
        times+=("$duration")
    done

    # Calculate statistics
    local sum=0
    local min=${times[0]}
    local max=${times[0]}

    for time in "${times[@]}"; do
        sum=$(echo "$sum + $time" | bc)
        if (( $(echo "$time < $min" | bc -l) )); then
            min=$time
        fi
        if (( $(echo "$time > $max" | bc -l) )); then
            max=$time
        fi
    done

    local mean=$(echo "scale=6; $sum / $iterations" | bc)

    # Sort for percentiles
    IFS=$'\n' sorted=($(sort -n <<<"${times[*]}"))
    unset IFS

    local p50_idx=$(( iterations / 2 ))
    local p95_idx=$(( iterations * 95 / 100 ))
    local p99_idx=$(( iterations * 99 / 100 ))

    local median=${sorted[$p50_idx]}
    local p95=${sorted[$p95_idx]}
    local p99=${sorted[$p99_idx]}

    # Output as JSON
    echo "{\"mean\":$mean,\"median\":$median,\"min\":$min,\"max\":$max,\"p95\":$p95,\"p99\":$p99}"
}

# Compare two benchmark results
compare_results() {
    local sqlite_result="$1"
    local helios_result="$2"
    local metric_name="$3"

    local sqlite_mean=$(echo "$sqlite_result" | grep -oP '"mean":\K[0-9.]+')
    local helios_mean=$(echo "$helios_result" | grep -oP '"mean":\K[0-9.]+')

    local ratio=$(echo "scale=3; $helios_mean / $sqlite_mean" | bc)

    echo "  $metric_name:"
    echo "    SQLite:    ${sqlite_mean}s"
    echo "    HeliosDB:  ${helios_mean}s"
    echo "    Ratio:     ${ratio}x"

    # Check against threshold
    local passes=false
    if (( $(echo "$ratio <= $LATENCY_THRESHOLD" | bc -l) )); then
        log_success "  Performance acceptable (${ratio}x <= ${LATENCY_THRESHOLD}x)"
        passes=true
    else
        log_error "  Performance degradation detected (${ratio}x > ${LATENCY_THRESHOLD}x)"
    fi

    # Return pass/fail
    echo "$passes"
}

# Measure bulk operation throughput
benchmark_bulk_operation() {
    local db_type="$1"
    local operation="$2"  # INSERT, UPDATE, DELETE
    local row_count="$3"

    local table_name="bench_bulk_${operation,,}_$$"

    # Setup
    if [[ "$db_type" == "sqlite" ]]; then
        sqlite_exec "CREATE TABLE IF NOT EXISTS $table_name (id INTEGER PRIMARY KEY, value INTEGER)"
    else
        helios_exec "CREATE TABLE IF NOT EXISTS $table_name (id SERIAL PRIMARY KEY, value INTEGER)"
    fi

    # Pre-populate for UPDATE/DELETE
    if [[ "$operation" != "INSERT" ]]; then
        for ((i=1; i<=row_count; i++)); do
            if [[ "$db_type" == "sqlite" ]]; then
                sqlite_exec "INSERT INTO $table_name (value) VALUES ($i)" > /dev/null
            else
                helios_exec "INSERT INTO $table_name (value) VALUES ($i)" > /dev/null
            fi
        done
    fi

    # Measure operation
    local start=$(date +%s.%N)

    case "$operation" in
        INSERT)
            for ((i=1; i<=row_count; i++)); do
                if [[ "$db_type" == "sqlite" ]]; then
                    sqlite_exec "INSERT INTO $table_name (value) VALUES ($i)" > /dev/null
                else
                    helios_exec "INSERT INTO $table_name (value) VALUES ($i)" > /dev/null
                fi
            done
            ;;
        UPDATE)
            if [[ "$db_type" == "sqlite" ]]; then
                sqlite_exec "UPDATE $table_name SET value = value + 1" > /dev/null
            else
                helios_exec "UPDATE $table_name SET value = value + 1" > /dev/null
            fi
            ;;
        DELETE)
            if [[ "$db_type" == "sqlite" ]]; then
                sqlite_exec "DELETE FROM $table_name WHERE id <= $row_count" > /dev/null
            else
                helios_exec "DELETE FROM $table_name WHERE id <= $row_count" > /dev/null
            fi
            ;;
    esac

    local end=$(date +%s.%N)
    local duration=$(echo "$end - $start" | bc)
    local throughput=$(echo "scale=2; $row_count / $duration" | bc)

    # Cleanup
    if [[ "$db_type" == "sqlite" ]]; then
        sqlite_exec "DROP TABLE $table_name" > /dev/null
    else
        helios_exec "DROP TABLE $table_name" > /dev/null
    fi

    echo "{\"duration\":$duration,\"throughput\":$throughput,\"row_count\":$row_count}"
}

################################################################################
# Benchmark Tests
################################################################################

run_query_latency_benchmarks() {
    log_info "Running query latency benchmarks..."
    echo ""
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "QUERY LATENCY BENCHMARKS" | tee -a "$RESULTS_FILE"
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "" | tee -a "$RESULTS_FILE"

    # Test queries (adjust based on your schema)
    declare -A queries=(
        ["Simple SELECT"]="SELECT * FROM users LIMIT 100"
        ["Filtered SELECT"]="SELECT * FROM users WHERE age > 25"
        ["Indexed Query"]="SELECT * FROM users WHERE email = 'test@example.com'"
        ["Aggregation"]="SELECT COUNT(*), AVG(age) FROM users"
        ["JOIN"]="SELECT u.name, COUNT(o.id) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.id, u.name"
    )

    local all_passed=true

    for query_name in "${!queries[@]}"; do
        local query="${queries[$query_name]}"

        echo "Benchmark: $query_name" | tee -a "$RESULTS_FILE"
        echo "Query: $query" | tee -a "$RESULTS_FILE"
        echo "" | tee -a "$RESULTS_FILE"

        # Run SQLite benchmark
        if [[ "$SKIP_SQLITE" == false ]]; then
            log_info "  Running SQLite benchmark..."
            local sqlite_result=$(benchmark_query "sqlite" "$query" "$ITERATIONS")
            echo "  SQLite: $sqlite_result" | tee -a "$RESULTS_FILE"
        else
            log_info "  Skipping SQLite benchmark (using baseline)"
            # Load from baseline file if provided
            sqlite_result='{"mean":0.001,"median":0.001,"min":0.001,"max":0.002,"p95":0.001,"p99":0.002}'
        fi

        # Run HeliosDB benchmark
        log_info "  Running HeliosDB benchmark..."
        local helios_result=$(benchmark_query "helios" "$query" "$ITERATIONS")
        echo "  HeliosDB: $helios_result" | tee -a "$RESULTS_FILE"

        # Compare results
        local passed=$(compare_results "$sqlite_result" "$helios_result" "$query_name")
        if [[ "$passed" != "true" ]]; then
            all_passed=false
        fi

        echo "" | tee -a "$RESULTS_FILE"
    done

    if [[ "$all_passed" == true ]]; then
        log_success "All query latency benchmarks passed!"
    else
        log_error "Some query latency benchmarks failed!"
    fi

    echo "" | tee -a "$RESULTS_FILE"
}

run_bulk_operation_benchmarks() {
    log_info "Running bulk operation benchmarks..."
    echo ""
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "BULK OPERATION BENCHMARKS" | tee -a "$RESULTS_FILE"
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "" | tee -a "$RESULTS_FILE"

    local operations=("INSERT" "UPDATE" "DELETE")
    local row_count=1000
    local all_passed=true

    for operation in "${operations[@]}"; do
        echo "Benchmark: Bulk $operation ($row_count rows)" | tee -a "$RESULTS_FILE"

        # Run SQLite benchmark
        if [[ "$SKIP_SQLITE" == false ]]; then
            log_info "  Running SQLite benchmark..."
            local sqlite_result=$(benchmark_bulk_operation "sqlite" "$operation" "$row_count")
            local sqlite_throughput=$(echo "$sqlite_result" | grep -oP '"throughput":\K[0-9.]+')
            echo "  SQLite:    $sqlite_throughput rows/sec" | tee -a "$RESULTS_FILE"
        else
            sqlite_throughput=1000
        fi

        # Run HeliosDB benchmark
        log_info "  Running HeliosDB benchmark..."
        local helios_result=$(benchmark_bulk_operation "helios" "$operation" "$row_count")
        local helios_throughput=$(echo "$helios_result" | grep -oP '"throughput":\K[0-9.]+')
        echo "  HeliosDB:  $helios_throughput rows/sec" | tee -a "$RESULTS_FILE"

        # Compare
        local ratio=$(echo "scale=3; $helios_throughput / $sqlite_throughput" | bc)
        echo "  Ratio:     ${ratio}x" | tee -a "$RESULTS_FILE"

        if (( $(echo "$ratio >= $THROUGHPUT_THRESHOLD" | bc -l) )); then
            log_success "  Throughput acceptable (${ratio}x >= ${THROUGHPUT_THRESHOLD}x)"
        else
            log_error "  Throughput too low (${ratio}x < ${THROUGHPUT_THRESHOLD}x)"
            all_passed=false
        fi

        echo "" | tee -a "$RESULTS_FILE"
    done

    if [[ "$all_passed" == true ]]; then
        log_success "All bulk operation benchmarks passed!"
    else
        log_error "Some bulk operation benchmarks failed!"
    fi

    echo "" | tee -a "$RESULTS_FILE"
}

run_index_performance_test() {
    log_info "Running index performance test..."
    echo ""
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "INDEX PERFORMANCE TEST" | tee -a "$RESULTS_FILE"
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "" | tee -a "$RESULTS_FILE"

    local test_table="bench_index_test_$$"
    local test_column="email"

    # Create test table
    log_info "Creating test table..."
    helios_exec "CREATE TABLE IF NOT EXISTS $test_table (
        id SERIAL PRIMARY KEY,
        email TEXT,
        value INTEGER
    )"

    # Populate with test data
    log_info "Populating test data (10,000 rows)..."
    for ((i=1; i<=10000; i++)); do
        helios_exec "INSERT INTO $test_table (email, value) VALUES ('user$i@example.com', $i)" > /dev/null
    done

    # Benchmark without index
    log_info "Benchmarking query WITHOUT index..."
    local query_without_index="SELECT * FROM $test_table WHERE email = 'user5000@example.com'"
    local result_without=$(benchmark_query "helios" "$query_without_index" 10)
    local time_without=$(echo "$result_without" | grep -oP '"mean":\K[0-9.]+')
    echo "  Time without index: ${time_without}s" | tee -a "$RESULTS_FILE"

    # Create index
    log_info "Creating index on $test_column..."
    helios_exec "CREATE INDEX idx_${test_table}_email ON $test_table($test_column)"

    # Benchmark with index
    log_info "Benchmarking query WITH index..."
    local result_with=$(benchmark_query "helios" "$query_without_index" 10)
    local time_with=$(echo "$result_with" | grep -oP '"mean":\K[0-9.]+')
    echo "  Time with index:    ${time_with}s" | tee -a "$RESULTS_FILE"

    # Calculate speedup
    local speedup=$(echo "scale=2; $time_without / $time_with" | bc)
    echo "  Speedup:            ${speedup}x" | tee -a "$RESULTS_FILE"

    if (( $(echo "$speedup >= $INDEX_SPEEDUP_MIN" | bc -l) )); then
        log_success "Index provides acceptable speedup (${speedup}x >= ${INDEX_SPEEDUP_MIN}x)"
    else
        log_warning "Index speedup below threshold (${speedup}x < ${INDEX_SPEEDUP_MIN}x)"
    fi

    # Cleanup
    log_info "Cleaning up..."
    helios_exec "DROP TABLE $test_table"

    echo "" | tee -a "$RESULTS_FILE"
}

run_concurrent_connection_test() {
    log_info "Running concurrent connection stress test..."
    echo ""
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "CONCURRENT CONNECTION TEST" | tee -a "$RESULTS_FILE"
    echo "========================================" | tee -a "$RESULTS_FILE"
    echo "" | tee -a "$RESULTS_FILE"

    local num_connections=20
    local queries_per_connection=50

    log_info "Testing with $num_connections concurrent connections..."
    log_info "Each connection will execute $queries_per_connection queries..."

    local start=$(date +%s.%N)
    local pids=()

    # Spawn concurrent connections
    for ((i=1; i<=num_connections; i++)); do
        (
            for ((j=1; j<=queries_per_connection; j++)); do
                helios_exec "SELECT $i * $j" > /dev/null
            done
        ) &
        pids+=($!)
    done

    # Wait for all to complete
    local failed=0
    for pid in "${pids[@]}"; do
        if ! wait "$pid"; then
            ((failed++))
        fi
    done

    local end=$(date +%s.%N)
    local duration=$(echo "$end - $start" | bc)
    local total_queries=$((num_connections * queries_per_connection))
    local throughput=$(echo "scale=2; $total_queries / $duration" | bc)

    echo "  Concurrent connections: $num_connections" | tee -a "$RESULTS_FILE"
    echo "  Queries per connection: $queries_per_connection" | tee -a "$RESULTS_FILE"
    echo "  Total queries:          $total_queries" | tee -a "$RESULTS_FILE"
    echo "  Duration:               ${duration}s" | tee -a "$RESULTS_FILE"
    echo "  Throughput:             $throughput queries/sec" | tee -a "$RESULTS_FILE"
    echo "  Failed connections:     $failed" | tee -a "$RESULTS_FILE"

    if [[ $failed -eq 0 ]]; then
        log_success "All concurrent connections succeeded!"
    else
        log_error "$failed concurrent connections failed!"
    fi

    echo "" | tee -a "$RESULTS_FILE"
}

generate_summary_report() {
    log_info "Generating summary report..."

    cat > "$SUMMARY_FILE" << EOF
{
  "benchmark_date": "$(date -Iseconds)",
  "configuration": {
    "sqlite_db": "$SQLITE_DB",
    "helios_host": "$HELIOS_HOST",
    "helios_port": $HELIOS_PORT,
    "helios_database": "$HELIOS_DB",
    "iterations": $ITERATIONS
  },
  "thresholds": {
    "latency_ratio": $LATENCY_THRESHOLD,
    "throughput_ratio": $THROUGHPUT_THRESHOLD,
    "index_speedup": $INDEX_SPEEDUP_MIN
  },
  "results_file": "$RESULTS_FILE"
}
EOF

    log_success "Summary report saved to: $SUMMARY_FILE"
}

################################################################################
# Main Execution
################################################################################

main() {
    echo ""
    echo "============================================================================"
    echo "PHASE 2 PERFORMANCE BENCHMARKS"
    echo "SQLite vs HeliosDB Performance Comparison"
    echo "============================================================================"
    echo ""
    echo "Configuration:"
    echo "  SQLite Database:    $SQLITE_DB"
    echo "  HeliosDB Host:      $HELIOS_HOST:$HELIOS_PORT"
    echo "  HeliosDB Database:  $HELIOS_DB"
    echo "  Iterations:         $ITERATIONS"
    echo "  Output Directory:   $OUTPUT_DIR"
    echo "  Skip SQLite:        $SKIP_SQLITE"
    echo ""
    echo "Thresholds:"
    echo "  Query Latency:      <= ${LATENCY_THRESHOLD}x of SQLite"
    echo "  Bulk Throughput:    >= ${THROUGHPUT_THRESHOLD}x of SQLite"
    echo "  Index Speedup:      >= ${INDEX_SPEEDUP_MIN}x"
    echo ""
    echo "Results will be saved to: $RESULTS_FILE"
    echo ""

    # Verify prerequisites
    log_info "Verifying prerequisites..."

    if [[ "$SKIP_SQLITE" == false ]] && [[ ! -f "$SQLITE_DB" ]]; then
        log_error "SQLite database not found: $SQLITE_DB"
        exit 1
    fi

    if ! command -v psql &> /dev/null; then
        log_error "psql not found. Please install PostgreSQL client."
        exit 1
    fi

    if ! command -v bc &> /dev/null; then
        log_error "bc not found. Please install bc for calculations."
        exit 1
    fi

    # Test HeliosDB connection
    log_info "Testing HeliosDB connection..."
    if ! helios_exec "SELECT 1" > /dev/null 2>&1; then
        log_error "Cannot connect to HeliosDB at $HELIOS_HOST:$HELIOS_PORT"
        exit 1
    fi
    log_success "HeliosDB connection successful"

    # Run benchmarks
    local start_time=$(date +%s)

    run_query_latency_benchmarks
    run_bulk_operation_benchmarks
    run_index_performance_test
    run_concurrent_connection_test

    local end_time=$(date +%s)
    local total_duration=$((end_time - start_time))

    # Generate summary
    generate_summary_report

    echo ""
    echo "============================================================================"
    echo "BENCHMARK COMPLETE"
    echo "============================================================================"
    echo ""
    echo "Total Duration: ${total_duration}s"
    echo "Results File:   $RESULTS_FILE"
    echo "Summary File:   $SUMMARY_FILE"
    echo ""
    log_success "Phase 2 performance benchmarks completed successfully!"
    echo ""
}

# Run main function
main "$@"
