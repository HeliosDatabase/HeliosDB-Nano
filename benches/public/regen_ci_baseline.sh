#!/usr/bin/env bash
# Regenerate benches/public/ci_baseline.json from a fresh run on THIS machine.
#
# Runs the same workload set as ci_perf_smoke.sh (best-of-3 by default), then
# writes the baseline with each rate scaled DOWN by BASELINE_SCALE (default
# 0.5) so that slower runner hardware does not trip the gate spuriously.
# Effective failure line = measured_here * BASELINE_SCALE / threshold(2.5),
# i.e. by default a workload must drop ~5x below this machine's number to fail.
#
# When to regenerate:
#   - CI runner hardware class changed and the gate fails across the board
#   - an intentional, reviewed perf trade-off moved a workload's floor
#   - a new workload was added to the gate set
# Then commit the updated ci_baseline.json together with an explanation in the
# PR description. NEVER regenerate to silence a failure you can't explain.
#
# Usage:
#   benches/public/regen_ci_baseline.sh
#   BASELINE_SCALE=0.3 PERF_GATE_REPS=5 benches/public/regen_ci_baseline.sh

source "$(dirname "$0")/common.sh"

require_deps cargo rustc python3 git
build_perf_harness
DIR="$(new_results_dir ci_baseline_regen)"
echo "results dir: $DIR"

run_ci_gate_workloads "$DIR" "${PERF_GATE_REPS:-3}"

python3 "$TOOLS" baseline \
    --results "$DIR/ci_measured.json" \
    --out "$PUB/ci_baseline.json" \
    --scale "${BASELINE_SCALE:-0.5}" \
    --threshold 2.5 \
    --note "CI perf-gate baseline: best-of-${PERF_GATE_REPS:-3} of run_tps_suite (mem, N=1000, M=200) + run_fk_bulk_insert_bench (1000 parents / 2000 children, one txn), rates pre-scaled by ${BASELINE_SCALE:-0.5} for slower runners. Regenerate with benches/public/regen_ci_baseline.sh."

echo
echo "Updated $PUB/ci_baseline.json — review the diff and commit it with your PR."
