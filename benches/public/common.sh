# Shared helpers for benches/public/*.sh — source, don't execute.
#
#   source "$(dirname "$0")/common.sh"
#
# Provides: ROOT, PUB, TOOLS, RESULTS_DIR (per-invocation, timestamped),
# require_deps, build_perf_harness, run_gated_test, stamp_md_header.

set -euo pipefail

PUB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$PUB/../.." && pwd)"
TOOLS="$PUB/bench_tools.py"

require_deps() {
    local missing=0
    for dep in "$@"; do
        if ! command -v "$dep" >/dev/null 2>&1; then
            echo "ERROR: required dependency '$dep' not found in PATH" >&2
            missing=1
        fi
    done
    if [ "$missing" -ne 0 ]; then
        exit 1
    fi
    if printf '%s\n' "$@" | grep -qx python3; then
        python3 - <<'EOF'
import sqlite3, sys
print(f"python {sys.version.split()[0]}, sqlite {sqlite3.sqlite_version} (stdlib)")
EOF
    fi
}

# Build the TPS harness once with the perf profile (release-like, faster build).
build_perf_harness() {
    echo "== building tps_workloads harness (cargo --profile perf) =="
    (cd "$ROOT" && cargo test --profile perf --test tps_workloads --no-run --quiet)
}

# run_gated_test <test_name> <output_file> [ENV=val ...]
# Runs one env-gated bench from tests/tps_workloads.rs single-threaded with
# output captured to <output_file> and echoed to the console.
run_gated_test() {
    local test_name="$1" out="$2"
    shift 2
    # Only stdout goes into the raw log: cargo replays cached compiler
    # warnings on stderr (even with --quiet) and they would pollute it.
    # stderr still reaches the console for debugging.
    (cd "$ROOT" && env "$@" cargo test --quiet --profile perf --test tps_workloads \
        "$test_name" -- --nocapture --test-threads=1) | tee "$out"
}

# new_results_dir <script-short-name> — creates and echoes a timestamped dir.
new_results_dir() {
    local dir="$PUB/results/${1}_$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "$dir"
    echo "$dir"
}

# run_ci_gate_workloads <outdir> [reps]
# Runs the CI perf-gate workload set `reps` times (default 3) and parses the
# concatenated output into <outdir>/ci_measured.json. The parser keeps the
# BEST ops/s per workload across reps, which smooths CI-runner noise spikes.
# Workload set (keep in sync with regen_ci_baseline.sh + perf-gate.yml docs):
#   - run_tps_suite, mem, N=1000 M=200 (all 11 workloads)
#   - run_fk_bulk_insert_bench, parents=1000 children=2000 in one txn
#     (the v3.28.0 338x FK regression shape)
run_ci_gate_workloads() {
    local dir="$1" reps="${2:-3}"
    : > "$dir/ci_raw.txt"
    for rep in $(seq 1 "$reps"); do
        echo "--- gate rep $rep/$reps: tps suite (mem, N=1000, M=200) ---"
        (cd "$ROOT" && env HELIOS_TPS=1 HELIOS_TPS_MODE=mem \
            HELIOS_TPS_N=1000 HELIOS_TPS_M=200 \
            cargo test --quiet --profile perf --test tps_workloads run_tps_suite \
            -- --nocapture --test-threads=1) 2>&1 | tee -a "$dir/ci_raw.txt"
        echo "--- gate rep $rep/$reps: in-txn FK bulk insert (1000 parents, 2000 children) ---"
        (cd "$ROOT" && env HELIOS_FK_BULK=1 HELIOS_FK_PARENTS=1000 \
            HELIOS_FK_CHILDREN=2000 \
            cargo test --quiet --profile perf --test tps_workloads run_fk_bulk_insert_bench \
            -- --nocapture --test-threads=1) 2>&1 | tee -a "$dir/ci_raw.txt"
    done
    python3 "$TOOLS" parse "$dir/ci_raw.txt" > "$dir/ci_measured.json"
}

# stamp_md_header <title> — emits a markdown header with commit/host/date.
stamp_md_header() {
    local commit dirty cpu
    commit="$(git -C "$ROOT" rev-parse --short HEAD)"
    dirty=""
    [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ] && dirty="-dirty"
    cpu="$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ //' || echo unknown)"
    cat <<EOF
# $1

- Date (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Commit: \`${commit}${dirty}\`
- Host: ${cpu}, $(nproc) logical cores, $(awk '/MemTotal/{printf "%.0f GiB RAM", $2/1048576}' /proc/meminfo), $(uname -sr)
- rustc: $(rustc --version 2>/dev/null || echo unknown)
EOF
}
