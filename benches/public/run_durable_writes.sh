#!/usr/bin/env bash
# Durable Writes Gauntlet — HeliosDB-Nano vs SQLite at HONEST durability tiers.
#
# Tier disclosure (read before quoting any number):
#   Tier 1  power-loss durable: fsync per commit
#           Nano:   on-disk, durable_commit=true, EXPLICIT transaction commits
#                   (BEGIN;INSERT;COMMIT per cycle — run_durable_commit_bench).
#                   See the caveat below about autocommit statements.
#           SQLite: journal_mode=DELETE, synchronous=FULL (its durable default);
#                   each autocommit statement is a durable commit.
#   Tier 2  WAL'd, process-crash durable; recent commits MAY be lost on power loss
#           Nano:   on-disk DEFAULT config (wal_enabled, wal_sync_mode=Sync,
#                   durable_commit=false)
#           SQLite: journal_mode=WAL, synchronous=NORMAL (common production shape)
#   Tier 3  no durability (pure engine CPU path, for reference only)
#           Nano mem / SQLite :memory: — see run_sqlite_mirror.sh
# Never compare numbers ACROSS tiers: that is the dishonest-multiplier trap.
#
# KNOWN CAVEAT (verified 2026-06-11 on a ~16ms-fsync md-RAID array): Nano's
# storage.durable_commit=true applies to explicit transaction commits. The
# autocommit INSERT/UPDATE/DELETE fast path measures IDENTICAL throughput with
# the flag on and off (~60k ops/s, physically impossible if each op fsynced),
# i.e. autocommit single statements are NOT power-loss durable even at
# durable_commit=true. This script runs a "tier1_probe" section that
# demonstrates exactly that, so the gap stays visible until fixed. Tier-1
# numbers therefore use the explicit-transaction shape only.
#
# Sections:
#   1. Tier 1: Nano run_durable_commit_bench (durable=true, 1/8/16/24/32
#      sessions) vs SQLite synchronous=FULL single-op writes. Comparable pair
#      at one writer: Nano "durable=true 1T" vs SQLite "autocommit_insert"
#      (both are one durable single-row-INSERT commit per cycle). Nano's
#      multi-session rows show group-commit scaling; SQLite has a single
#      writer, so it has no scaling column.
#   2. Tier 2: Nano disk-default suite write workloads vs SQLite WAL/NORMAL
#      (single-threaded mirror, identical workloads).
#   3. Tier-1 probe: Nano disk + HELIOS_TPS_DURABLE=1 suite writes — should
#      match SQLite FULL if autocommit honored durable_commit; today it
#      matches Tier 2 instead (the caveat above).
#
# Usage:
#   benches/public/run_durable_writes.sh            # N=10000 rows, M=500 single ops
#   DW_N=20000 DW_M=1000 benches/public/run_durable_writes.sh
#
# Output: benches/public/results/durable_writes_<UTC>/ with raw logs,
# results.json (commit/host-stamped) and RESULTS.md.

source "$(dirname "$0")/common.sh"

# Both harnesses place their on-disk databases under $TMPDIR (default /tmp).
# On distros where /tmp is tmpfs (RAM), fsync is a no-op and every "durable"
# number would be fake — redirect to a real-disk path inside the repo.
TMP_FSTYPE="$(df --output=fstype "${TMPDIR:-/tmp}" 2>/dev/null | tail -1 | tr -d ' ')"
if [ "$TMP_FSTYPE" = "tmpfs" ] || [ "$TMP_FSTYPE" = "ramfs" ]; then
    export TMPDIR="$ROOT/target/bench_tmp"
    mkdir -p "$TMPDIR"
    echo "WARNING: ${TMPDIR:-/tmp} default was $TMP_FSTYPE (RAM-backed)."
    echo "         Durable tiers need real fsync -> using TMPDIR=$TMPDIR"
    echo "         (ensure the repo itself is on a real disk)"
fi

DW_N="${DW_N:-10000}"
DW_M="${DW_M:-500}"
DW_WORKLOADS="bulk_insert,autocommit_insert,update,delete"

require_deps cargo rustc python3 git
build_perf_harness
DIR="$(new_results_dir durable_writes)"
echo "results dir: $DIR"

echo; echo "===== Tier 1: Nano durable txn commits, 1..32 sessions (durable off vs on) ====="
run_gated_test run_durable_commit_bench "$DIR/nano_durable_scaling.txt" \
    HELIOS_DURABLE=1 HELIOS_DURABLE_M="${DW_SCALING_M:-100}"

echo; echo "===== Tier 1: SQLite journal=DELETE synchronous=FULL ====="
(cd "$ROOT" && SQLITE_TPS_MODE=disk SQLITE_TPS_BINDINGS=literal \
    SQLITE_TPS_N="$DW_N" SQLITE_TPS_M="$DW_M" \
    python3 benches/external/sqlite_tps_mirror.py) 2>&1 \
    | tee "$DIR/sqlite_tier1_full.txt"

echo; echo "===== Tier 2: Nano disk default (WAL on, durable_commit=false) ====="
run_gated_test run_tps_suite "$DIR/nano_tier2_disk_default.txt" \
    HELIOS_TPS=1 HELIOS_TPS_MODE=disk \
    HELIOS_TPS_N="$DW_N" HELIOS_TPS_M="$DW_M" HELIOS_TPS_WORKLOADS="$DW_WORKLOADS"

echo; echo "===== Tier 2: SQLite WAL synchronous=NORMAL ====="
(cd "$ROOT" && SQLITE_TPS_MODE=wal SQLITE_TPS_BINDINGS=literal \
    SQLITE_TPS_N="$DW_N" SQLITE_TPS_M="$DW_M" \
    python3 benches/external/sqlite_tps_mirror.py) 2>&1 \
    | tee "$DIR/sqlite_tier2_wal.txt"

echo; echo "===== Tier-1 probe: Nano disk + durable_commit=true, AUTOCOMMIT statements ====="
run_gated_test run_tps_suite "$DIR/nano_tier1_probe_autocommit.txt" \
    HELIOS_TPS=1 HELIOS_TPS_MODE=disk HELIOS_TPS_DURABLE=1 \
    HELIOS_TPS_N="$DW_N" HELIOS_TPS_M="$DW_M" HELIOS_TPS_WORKLOADS="$DW_WORKLOADS"

for f in nano_durable_scaling sqlite_tier1_full nano_tier2_disk_default \
         sqlite_tier2_wal nano_tier1_probe_autocommit; do
    python3 "$TOOLS" parse "$DIR/$f.txt" > "$DIR/$f.parsed.json"
done

python3 "$TOOLS" assemble --script run_durable_writes.sh --out "$DIR/results.json" \
    --param "n=$DW_N" --param "m=$DW_M" \
    nano_durable_scaling="$DIR/nano_durable_scaling.parsed.json" \
    sqlite_tier1_full="$DIR/sqlite_tier1_full.parsed.json" \
    nano_tier2_disk_default="$DIR/nano_tier2_disk_default.parsed.json" \
    sqlite_tier2_wal="$DIR/sqlite_tier2_wal.parsed.json" \
    nano_tier1_probe_autocommit="$DIR/nano_tier1_probe_autocommit.parsed.json"

MD="$DIR/RESULTS.md"
{
    stamp_md_header "Durable Writes Gauntlet — honest durability tiers"
    cat <<EOF

Write workloads at N=$DW_N rows / M=$DW_M single-op statements. TIER LABELS
MATTER — see the header of benches/public/run_durable_writes.sh for exactly
what each tier guarantees, and for the durable_commit-vs-autocommit caveat.
Never compare across tiers. Repro:

\`\`\`bash
benches/public/run_durable_writes.sh
\`\`\`

## Tier 1 — power-loss durable (fsync per commit)

Comparable single-writer pair: Nano \`durable=true 1T\` (BEGIN;INSERT;COMMIT
cycles) vs SQLite \`autocommit_insert\` under synchronous=FULL. Nano's
multi-session rows show group-commit scaling; SQLite writes are serialized.

Nano (run_durable_commit_bench; durable=false rows are Tier 2):

\`\`\`text
$(cat "$DIR/nano_durable_scaling.txt" | grep -E 'durable=' || true)
\`\`\`

SQLite synchronous=FULL (every autocommit statement is a durable commit):

\`\`\`text
$(grep -E 'ops/s' "$DIR/sqlite_tier1_full.txt" || true)
\`\`\`

## Tier 2 — WAL'd, process-crash durable (NOT guaranteed power-loss durable)

Nano: disk DEFAULT (wal_sync_mode=Sync, durable_commit=false).
SQLite: WAL, synchronous=NORMAL. Single-threaded mirror, identical workloads.

EOF
    python3 "$TOOLS" compare --results "$DIR/results.json" \
        --left nano_tier2_disk_default --right sqlite_tier2_wal \
        --left-name "Nano(T2)" --right-name "SQLite(WAL)"
    cat <<'EOF'

## Tier-1 probe — autocommit statements do NOT honor durable_commit (known gap)

Same suite as Tier 2 but with durable_commit=true. If autocommit honored the
flag, these rates would be fsync-bound (tens of ops/s on this class of disk);
matching the Tier-2 numbers instead demonstrates the gap:

EOF
    python3 "$TOOLS" compare --results "$DIR/results.json" \
        --left nano_tier1_probe_autocommit --right nano_tier2_disk_default \
        --left-name "Nano(durable_commit=on)" --right-name "Nano(default)"
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
