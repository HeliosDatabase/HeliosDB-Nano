#!/bin/sh
#
# binary-size-check.sh — guard against unbounded growth of the
# default-feature `heliosdb-nano` binary.  Run from CI on every
# PR; fails when the size exceeds the committed baseline by
# more than `MAX_GROWTH_PCT` (default 5 %).
#
# Re-baseline by overwriting docs/followups/binary-size-baseline.json
# in a deliberate commit — that's a knob, not an automated bump.

set -eu

REPO_ROOT="$(git rev-parse --show-toplevel)"
BASELINE="$REPO_ROOT/docs/followups/binary-size-baseline.json"
MAX_GROWTH_PCT="${MAX_GROWTH_PCT:-5}"

if [ ! -f "$BASELINE" ]; then
  echo "fatal: $BASELINE missing — commit a baseline first" >&2
  exit 1
fi

echo "==> building default-feature release binary"
( cd "$REPO_ROOT" && cargo build --release --quiet )
DEFAULT_BIN="$REPO_ROOT/target/release/heliosdb-nano"
DEFAULT_SIZE=$(stat -c %s "$DEFAULT_BIN" 2>/dev/null || stat -f %z "$DEFAULT_BIN")

echo "==> building all-features release binary"
( cd "$REPO_ROOT" && cargo build --release --quiet \
    --features code-graph,graph-rag,mcp-endpoint,code-embed )
ALL_BIN="$REPO_ROOT/target/release/heliosdb-nano"
ALL_SIZE=$(stat -c %s "$ALL_BIN" 2>/dev/null || stat -f %z "$ALL_BIN")

# Naive JSON parse: pull `default_size` and `all_features_size`
# from the baseline.  Avoid jq so the script runs in minimal CI
# images.
extract_field() {
  field="$1"
  python3 -c "import json,sys; d=json.load(open('$BASELINE')); print(d['$field'])"
}

BASE_DEFAULT=$(extract_field default_size)
BASE_ALL=$(extract_field all_features_size)

printf 'default-feature size:   %s bytes (baseline %s)\n' "$DEFAULT_SIZE" "$BASE_DEFAULT"
printf 'all-features size:      %s bytes (baseline %s)\n' "$ALL_SIZE" "$BASE_ALL"

check_growth() {
  label="$1"
  current="$2"
  baseline="$3"
  if [ "$current" -le "$baseline" ]; then
    echo "  $label: under baseline ✓"
    return 0
  fi
  growth=$(awk "BEGIN{printf \"%.1f\", ($current - $baseline) * 100 / $baseline}")
  printf '  %s: +%s%% (limit %s%%)' "$label" "$growth" "$MAX_GROWTH_PCT"
  if awk "BEGIN{exit !($growth > $MAX_GROWTH_PCT)}"; then
    echo " ✗"
    return 1
  fi
  echo " ✓"
  return 0
}

set +e
fail=0
check_growth default "$DEFAULT_SIZE" "$BASE_DEFAULT" || fail=1
check_growth all-features "$ALL_SIZE" "$BASE_ALL" || fail=1
set -e

if [ "$fail" -ne 0 ]; then
  echo "binary size guard FAILED — re-baseline only with sign-off." >&2
  exit 1
fi
echo "binary size guard PASSED."
