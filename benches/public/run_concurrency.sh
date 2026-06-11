#!/usr/bin/env bash
# Concurrency benchmarks — HeliosDB-Nano embedded, multi-threaded.
#
# Three env-gated benches from tests/tps_workloads.rs:
#   run_cache_concurrency_bench  hot-row point lookups at 1/4/16 threads
#                                (row-cache read-lock path; 2k-row hot set)
#   run_session_txn_bench        BEGIN;INSERT;COMMIT cycles — global txn slot
#                                vs per-session txns at 1/4/16 threads (R0.1),
#                                plus session autocommit and writer-while-txn-open
#   run_conflict_bench           write-write conflict detection (R0.2):
#                                disjoint-key update txns at 1/4/16 threads and
#                                an 8-thread contended counter with
#                                retry-on-serialization-failure; asserts ZERO
#                                lost updates
#
# All run on the IN-MEMORY engine (no WAL/fsync): these isolate locking,
# session and conflict-detection overhead, not durability. For durable-write
# scaling see run_durable_writes.sh. There is no SQLite column here on
# purpose — SQLite serializes writers and its Python mirror is single-threaded.
#
# Usage:
#   benches/public/run_concurrency.sh
#   SESSION_M=5000 CONFLICT_M=1000 benches/public/run_concurrency.sh
#
# Output: benches/public/results/concurrency_<UTC>/ with raw logs,
# results.json (commit/host-stamped) and RESULTS.md.

source "$(dirname "$0")/common.sh"

SESSION_M="${SESSION_M:-2000}"
CONFLICT_M="${CONFLICT_M:-500}"

require_deps cargo rustc python3 git
build_perf_harness
DIR="$(new_results_dir concurrency)"
echo "results dir: $DIR"

echo; echo "===== cache concurrency (hot-row lookups, 1/4/16 threads) ====="
run_gated_test run_cache_concurrency_bench "$DIR/cache_concurrency.txt" \
    HELIOS_CACHE_CONC=1

echo; echo "===== session transactions (global vs per-session, 1/4/16 threads) ====="
run_gated_test run_session_txn_bench "$DIR/session_txn.txt" \
    HELIOS_SESSION_TXN=1 HELIOS_SESSION_TXN_M="$SESSION_M"

echo; echo "===== write-write conflict detection (disjoint + contended) ====="
run_gated_test run_conflict_bench "$DIR/conflict.txt" \
    HELIOS_CONFLICT=1 HELIOS_CONFLICT_M="$CONFLICT_M"

for f in cache_concurrency session_txn conflict; do
    python3 "$TOOLS" parse "$DIR/$f.txt" > "$DIR/$f.parsed.json"
done

python3 "$TOOLS" assemble --script run_concurrency.sh --out "$DIR/results.json" \
    --param "session_m=$SESSION_M" --param "conflict_m=$CONFLICT_M" \
    cache_concurrency="$DIR/cache_concurrency.parsed.json" \
    session_txn="$DIR/session_txn.parsed.json" \
    conflict="$DIR/conflict.parsed.json"

MD="$DIR/RESULTS.md"
{
    stamp_md_header "Concurrency benchmarks — HeliosDB-Nano embedded (in-memory)"
    cat <<EOF

In-memory engine: isolates locking / session / conflict-detection overhead
(no WAL or fsync — see run_durable_writes.sh for durable scaling). No SQLite
comparison column: SQLite serializes writers. Repro:

\`\`\`bash
benches/public/run_concurrency.sh
\`\`\`
EOF
    for f in cache_concurrency session_txn conflict; do
        echo
        echo "## $f"
        echo '```text'
        cat "$DIR/$f.txt"
        echo '```'
    done
} > "$MD"

echo
echo "wrote $MD"
echo "wrote $DIR/results.json"
