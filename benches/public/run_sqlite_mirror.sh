#!/usr/bin/env bash
# Head-to-head: HeliosDB-Nano embedded TPS suite vs SQLite, identical workloads.
#
# Runs tests/tps_workloads.rs::run_tps_suite (Nano, in-memory, literal SQL) and
# benches/external/sqlite_tps_mirror.py (SQLite :memory:, python stdlib) at two
# scales, in BOTH SQLite binding styles:
#   - literal  : interpolated SQL text — same statement shape the Nano harness
#                uses (apples-to-apples; both engines re-parse every statement)
#   - params   : bound parameters — SQLite's best embedded API shape (published
#                for honesty; Nano's literal harness competes against it anyway)
#
# IMPORTANT: both harnesses are SINGLE-THREADED — one client issuing one
# statement at a time. This measures per-statement engine latency, not
# multi-client throughput (see run_concurrency.sh for that).
#
# Usage:
#   benches/public/run_sqlite_mirror.sh           # full: N=10000/M=2000 and N=200000/M=40
#   benches/public/run_sqlite_mirror.sh --quick   # small scale only
#   SCALES="50000:10000" benches/public/run_sqlite_mirror.sh   # custom N:M pairs
#
# Output: benches/public/results/sqlite_mirror_<UTC>/ with raw logs,
# results.json (commit/host-stamped) and RESULTS.md.

source "$(dirname "$0")/common.sh"

SCALES="${SCALES:-10000:2000 200000:40}"
if [ "${1:-}" = "--quick" ]; then
    SCALES="10000:2000"
fi

require_deps cargo rustc python3 git
build_perf_harness
DIR="$(new_results_dir sqlite_mirror)"
echo "results dir: $DIR"

ASSEMBLE_ARGS=()
declare -a SCALE_LIST=()

for scale in $SCALES; do
    N="${scale%%:*}"
    M="${scale##*:}"
    SCALE_LIST+=("$N:$M")

    echo; echo "===== scale N=$N M=$M : HeliosDB-Nano (mem) ====="
    run_gated_test run_tps_suite "$DIR/helios_mem_n${N}.txt" \
        HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_N="$N" HELIOS_TPS_M="$M"
    python3 "$TOOLS" parse "$DIR/helios_mem_n${N}.txt" > "$DIR/helios_mem_n${N}.parsed.json"
    ASSEMBLE_ARGS+=("helios_mem_n${N}=$DIR/helios_mem_n${N}.parsed.json")

    for bindings in literal params; do
        echo; echo "===== scale N=$N M=$M : SQLite (mem, $bindings) ====="
        (cd "$ROOT" && SQLITE_TPS_MODE=mem SQLITE_TPS_N="$N" SQLITE_TPS_M="$M" \
            SQLITE_TPS_BINDINGS="$bindings" \
            python3 benches/external/sqlite_tps_mirror.py) 2>&1 \
            | tee "$DIR/sqlite_mem_${bindings}_n${N}.txt"
        python3 "$TOOLS" parse "$DIR/sqlite_mem_${bindings}_n${N}.txt" \
            > "$DIR/sqlite_mem_${bindings}_n${N}.parsed.json"
        ASSEMBLE_ARGS+=("sqlite_mem_${bindings}_n${N}=$DIR/sqlite_mem_${bindings}_n${N}.parsed.json")
    done
done

python3 "$TOOLS" assemble --script run_sqlite_mirror.sh --out "$DIR/results.json" \
    --param "scales=$SCALES" --param mode=mem "${ASSEMBLE_ARGS[@]}"

MD="$DIR/RESULTS.md"
{
    stamp_md_header "SQLite mirror — HeliosDB-Nano vs SQLite (in-memory, single-threaded)"
    cat <<'EOF'

Both harnesses run ONE client issuing ONE statement at a time (single-threaded);
this measures per-statement engine latency, not concurrent throughput.
Nano harness uses literal SQL; the apples-to-apples SQLite column is
`literal`. `params` (bound parameters, SQLite's best embedded shape) is in the
raw logs below. Repro:

```bash
benches/public/run_sqlite_mirror.sh
```
EOF
    for scale in "${SCALE_LIST[@]}"; do
        N="${scale%%:*}"; M="${scale##*:}"
        echo
        echo "## N=$N rows, M=$M single-op statements (SQLite bindings=literal)"
        echo
        python3 "$TOOLS" compare --results "$DIR/results.json" \
            --left "helios_mem_n${N}" --right "sqlite_mem_literal_n${N}" \
            --left-name "Nano" --right-name "SQLite"
        echo
        echo "### vs SQLite bound parameters (SQLite's best shape)"
        echo
        python3 "$TOOLS" compare --results "$DIR/results.json" \
            --left "helios_mem_n${N}" --right "sqlite_mem_params_n${N}" \
            --left-name "Nano" --right-name "SQLite(params)"
    done
    echo
    echo "## Raw output"
    for f in "$DIR"/*.txt; do
        echo
        echo "### $(basename "$f")"
        echo '```text'
        cat "$f"
        echo '```'
    done
} > "$MD"

echo
echo "wrote $MD"
echo "wrote $DIR/results.json"
