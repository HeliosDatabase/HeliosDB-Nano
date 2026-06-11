#!/usr/bin/env bash
# CI perf smoke gate — catches order-of-magnitude regressions, not drift.
#
# Runs the gate workload set (best-of-3):
#   - run_tps_suite, mem, N=1000 M=200 — all 11 single-threaded workloads
#   - run_fk_bulk_insert_bench — 2000 FK-bearing INSERTs in one transaction
#     (the exact shape of the v3.28.0 338x FK regression, which shipped
#     ~2 weeks undetected; see PROPOSAL_FK_VALIDATION_OPTIMIZATION.md W1)
# and FAILS if any workload is more than THRESHOLD x slower than the stored
# baseline in benches/public/ci_baseline.json.
#
# The threshold is deliberately generous (default 2.5x): CI runners are noisy
# and slower than the baseline workstation; this gate exists to catch
# 10x-300x cliffs, not 20% drift. Baseline rates are additionally pre-scaled
# down at generation time (see regen_ci_baseline.sh).
#
# Usage:
#   benches/public/ci_perf_smoke.sh                 # used by .github/workflows/perf-gate.yml
#   PERF_GATE_THRESHOLD=4 benches/public/ci_perf_smoke.sh
#   PERF_GATE_REPS=1 benches/public/ci_perf_smoke.sh   # single rep (faster, noisier)
#
# Baseline mismatch on new runner hardware? Regenerate and commit:
#   benches/public/regen_ci_baseline.sh

source "$(dirname "$0")/common.sh"

require_deps cargo rustc python3 git
build_perf_harness
DIR="$(new_results_dir ci_smoke)"
echo "results dir: $DIR"

run_ci_gate_workloads "$DIR" "${PERF_GATE_REPS:-3}"

python3 "$TOOLS" check \
    --baseline "$PUB/ci_baseline.json" \
    --results "$DIR/ci_measured.json" \
    ${PERF_GATE_THRESHOLD:+--threshold "$PERF_GATE_THRESHOLD"}
