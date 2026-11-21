#!/usr/bin/env bash
# Run the public SQL Logic Test corpus against HeliosDB-Nano and write the results.
#
#     scripts/slt_run.sh [embedded] [pgwire] [mysqlwire]        (default: embedded)
#
# Writes docs/correctness/sqllogictest.<interface>.json and docs/correctness/sqllogictest.md
# (the page is rebuilt from every result document present).
#
# Environment:
#   SLT_CORPUS_DIR   the corpus's `test` directory; fetched with slt_fetch_corpus.sh if unset
#   SLT_BASELINE     off | check | update   (default check when a baseline for the interface
#                    exists in docs/correctness/sqllogictest_baseline.json, else off)
#   SLT_JOBS         worker processes at a time (default 4)
#   SLT_TIMEOUT_MS   limit per record (default 10000)
#   SLT_FILTER       run only files whose path contains this text
#   SLT_MAX_FILES    run only the first N files
#   SLT_OUT_DIR      where the results go (default docs/correctness)
#   SLT_TEST_BIN     a prebuilt `slt_runner` test binary; without it the script builds
#                    one with `$CARGO test --release` (CARGO defaults to cargo)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [ -z "${SLT_CORPUS_DIR:-}" ]; then
    SLT_CORPUS_DIR="$(scripts/slt_fetch_corpus.sh | tail -1)"
fi
export SLT_CORPUS_DIR
export SLT_CORPUS_NAME="${SLT_CORPUS_NAME:-SQL Logic Test corpus, revision $(basename "$(dirname "$SLT_CORPUS_DIR")")}"
export SLT_SOURCE_REVISION="${SLT_SOURCE_REVISION:-$(git rev-parse --short=12 HEAD)$(git diff --quiet HEAD -- src Cargo.toml Cargo.lock || echo '+local changes')}"
export SLT_OUT_DIR="${SLT_OUT_DIR:-$ROOT/docs/correctness}"
BASELINE_FILE="$SLT_OUT_DIR/sqllogictest_baseline.json"

[ $# -gt 0 ] || set -- embedded
for interface in "$@"; do
    case "$interface" in embedded|pgwire|mysqlwire) ;; *) echo "unknown interface: $interface" >&2; exit 2 ;; esac
    mode="${SLT_BASELINE:-}"
    if [ -z "$mode" ]; then
        if [ -f "$BASELINE_FILE" ] && grep -q "\"$interface\"" "$BASELINE_FILE"; then mode=check; else mode=off; fi
    fi
    echo "== $interface (baseline: $mode)" >&2
    if [ -n "${SLT_TEST_BIN:-}" ]; then
        SLT_INTERFACE="$interface" SLT_BASELINE="$mode" "$SLT_TEST_BIN" \
            --ignored --exact the_public_corpus --nocapture
    else
        SLT_INTERFACE="$interface" SLT_BASELINE="$mode" ${CARGO:-cargo} test --release --test slt_runner -- \
            --ignored --exact the_public_corpus --nocapture
    fi
done
echo "results: $SLT_OUT_DIR/sqllogictest.md" >&2
