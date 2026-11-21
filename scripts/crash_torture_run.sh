#!/usr/bin/env bash
# The long crash-recovery torture run (process crash).
#
#     scripts/crash_torture_run.sh <iterations> [first-seed] [time-budget-seconds]
#
# Each iteration is one seed: set up a database, then three times run a seeded workload
# in a worker process, kill it with SIGKILL, reopen and check. Writes
# docs/correctness/crash-recovery.json and docs/correctness/crash-recovery.md, and exits
# non-zero when a seed fails. A failing seed is replayed with
#
#     CRASH_TORTURE_SEED=<seed> scripts/crash_torture_run.sh 1
#
# Environment:
#   CRASH_TORTURE_JOBS      seeds at a time (default 4)
#   CRASH_TORTURE_OUT_DIR   where the report goes (default docs/correctness)
#   CRASH_TORTURE_TEST_BIN  a prebuilt `crash_torture` test binary; without it the script
#                           builds one with `$CARGO test --release` (CARGO defaults to cargo)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
[ $# -ge 1 ] || { sed -n '2,18p' "$0" >&2; exit 2; }

export CRASH_TORTURE_ITERATIONS="$1"
export CRASH_TORTURE_SEED="${2:-${CRASH_TORTURE_SEED:-1}}"
export CRASH_TORTURE_BUDGET_S="${3:-${CRASH_TORTURE_BUDGET_S:-3600}}"
export CRASH_TORTURE_OUT_DIR="${CRASH_TORTURE_OUT_DIR:-$ROOT/docs/correctness}"
export CRASH_TORTURE_REVISION="${CRASH_TORTURE_REVISION:-$(git rev-parse --short=12 HEAD)$(git diff --quiet HEAD -- src Cargo.toml Cargo.lock || echo '+local changes')}"
export CRASH_TORTURE_TOOLCHAIN="${CRASH_TORTURE_TOOLCHAIN:-$(rustc -V 2>/dev/null || echo unknown)}"

if [ -n "${CRASH_TORTURE_TEST_BIN:-}" ]; then
    "$CRASH_TORTURE_TEST_BIN" --exact the_long_run_when_asked --nocapture
else
    ${CARGO:-cargo} test --release --test crash_torture -- --exact the_long_run_when_asked --nocapture
fi
echo "report: $CRASH_TORTURE_OUT_DIR/crash-recovery.md" >&2
