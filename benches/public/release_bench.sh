#!/usr/bin/env bash
# The release benchmark: this tree against an earlier release, in one command.
#
#   benches/public/release_bench.sh <baseline-rev> [<out-dir>]
#
#   benches/public/release_bench.sh v4.41.1
#
# Two parts, each an A/B between the baseline revision and this tree:
#
#   pg35   35 SQL categories, embedded Nano against a real PostgreSQL 18.4,
#          300 timed iterations each, every result checked (pg35_run.py; it
#          names the PostgreSQL reference it needs and how to start one).
#   wire   a one-row primary-key UPDATE over the PostgreSQL wire against a
#          running server: prepared (extended protocol) and as text (simple
#          protocol), at 1 to 64 clients (wire_pk_ab.sh).
#
# It clones the baseline revision out of this repository into the work
# directory and copies this tree's five pg35 harness files over it (test files
# only; the baseline's product source is not touched), builds both sides with
# the same toolchain and the release profile, runs both parts in alternating
# order, and writes RESULTS.md with the identity of every binary it measured.
#
# Environment:
#   PHASES        steps to run                   (default "prepare build pg35 wire report")
#   WORK          clone and build directories     (default <out-dir>/work)
#   CANDIDATE_TARGET_DIR                          (default $CARGO_TARGET_DIR, else <tree>/target)
#   BASELINE_TARGET_DIR                           (default <WORK>/target-baseline)
#   PG35_ROUNDS   rounds of pg35                  (default 2)
#   WIRE_ROUNDS   rounds of the wire part         (default 4)
#   CLIENTS       client counts of the wire part  (default "1 2 4 8 16 32 64")
#   SECONDS_PER_RUN / WARMUP / ROWS               passed to wire_pk_ab.sh
#   PG35_CONNSTR / CARGO / MAX_LOAD / LOAD_WAIT   passed to pg35_run.py
#
# A phase can be rerun alone with the same <out-dir>, e.g. PHASES="wire report".
# Needs cargo, git and python3 (standard library).
set -euo pipefail

PUB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$PUB/../.." && pwd)"

[ $# -ge 1 ] || { sed -n '2,36p' "$0" >&2; exit 2; }
REV="$1"
OUT="${2:-$PUB/results/release_$(date -u +%Y%m%dT%H%M%SZ)}"
OUT="$(mkdir -p "$OUT" && cd "$OUT" && pwd)"
PHASES="${PHASES:-prepare build pg35 wire report}"
WORK="${WORK:-$OUT/work}"
CARGO="${CARGO:-cargo}"
PG35_ROUNDS="${PG35_ROUNDS:-2}"
WIRE_ROUNDS="${WIRE_ROUNDS:-4}"
CLIENTS="${CLIENTS:-1 2 4 8 16 32 64}"
MAX_LOAD="${MAX_LOAD:-6}"
LOAD_WAIT="${LOAD_WAIT:-1800}"
CAND_TARGET="${CANDIDATE_TARGET_DIR:-${CARGO_TARGET_DIR:-$ROOT/target}}"
BASE_TARGET="${BASELINE_TARGET_DIR:-$WORK/target-baseline}"
BASE_TREE="$WORK/src-baseline"
HARNESS="tests/pg35_benchmark.rs tests/pg35_support/mod.rs tests/pg35_support/cells.rs tests/pg35_support/model.rs tests/pg35_support/oracles.rs"

for dep in git python3 "${CARGO%% *}"; do
    command -v "$dep" >/dev/null 2>&1 || { echo "ERROR: '$dep' not found in PATH" >&2; exit 1; }
done

BASE_COMMIT="$(git -C "$ROOT" rev-parse --verify --quiet "$REV^{commit}")" \
    || { echo "ERROR: '$REV' is not a revision of this repository" >&2; exit 1; }
BASE_LABEL="$(printf '%s' "$REV" | tr -c 'A-Za-z0-9._-' '_')"
CAND_LABEL="v$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
[ "$BASE_LABEL" != "$CAND_LABEL" ] || CAND_LABEL="$CAND_LABEL-tree"
BASE_BIN="$OUT/bin/$BASE_LABEL/heliosdb-nano"
CAND_BIN="$OUT/bin/$CAND_LABEL/heliosdb-nano"
CLIENT_BIN="$OUT/bin/wire_pk_bench"

has_phase() { case " $PHASES " in *" $1 "*) return 0 ;; esac; return 1; }

if has_phase prepare; then
    echo "== prepare: $BASE_LABEL ($BASE_COMMIT) in $BASE_TREE =="
    mkdir -p "$WORK"
    if [ ! -d "$BASE_TREE/.git" ]; then
        git clone --quiet --local --no-hardlinks "$ROOT" "$BASE_TREE"
    fi
    git -C "$BASE_TREE" checkout --quiet --force --detach "$BASE_COMMIT"
    for file in $HARNESS; do
        mkdir -p "$BASE_TREE/$(dirname "$file")"
        cp "$ROOT/$file" "$BASE_TREE/$file"
    done
    git -C "$BASE_TREE" status --short > "$OUT/baseline-overlay.txt"
fi

if has_phase build; then
    echo "== build: release profile, both sides =="
    [ -d "$BASE_TREE" ] || { echo "ERROR: run the prepare phase first" >&2; exit 1; }
    mkdir -p "$OUT/bin/$BASE_LABEL" "$OUT/bin/$CAND_LABEL"
    # shellcheck disable=SC2086
    (cd "$ROOT" && CARGO_TARGET_DIR="$CAND_TARGET" $CARGO build --release --locked --bin heliosdb-nano)
    # shellcheck disable=SC2086
    (cd "$ROOT" && CARGO_TARGET_DIR="$CAND_TARGET" $CARGO build --profile perf --locked --example wire_pk_bench)
    # shellcheck disable=SC2086
    (cd "$BASE_TREE" && CARGO_TARGET_DIR="$BASE_TARGET" $CARGO build --release --locked --bin heliosdb-nano)
    cp "$CAND_TARGET/release/heliosdb-nano" "$CAND_BIN"
    cp "$CAND_TARGET/perf/examples/wire_pk_bench" "$CLIENT_BIN"
    cp "$BASE_TARGET/release/heliosdb-nano" "$BASE_BIN"
    CARGO="$CARGO" python3 "$PUB/pg35_run.py" --build-only "$OUT/pg35" \
        "$BASE_LABEL=$BASE_TREE@$BASE_TARGET" "$CAND_LABEL=$ROOT@$CAND_TARGET"
fi

if has_phase pg35; then
    echo "== pg35: $PG35_ROUNDS round(s) =="
    CARGO="$CARGO" ROUNDS="$PG35_ROUNDS" MAX_LOAD="$MAX_LOAD" LOAD_WAIT="$LOAD_WAIT" \
        python3 "$PUB/pg35_run.py" "$OUT/pg35" \
        "$BASE_LABEL=$BASE_TREE@$BASE_TARGET" "$CAND_LABEL=$ROOT@$CAND_TARGET"
fi

if has_phase wire; then
    echo "== wire: UPDATE by primary key, prepared and text, clients [$CLIENTS], $WIRE_ROUNDS round(s) =="
    [ ! -e "$OUT/wire/results.jsonl" ] || { echo "ERROR: $OUT/wire already holds results" >&2; exit 1; }
    python3 - "$MAX_LOAD" "$LOAD_WAIT" <<'PY'
import os, sys, time
limit, patience = float(sys.argv[1]), float(sys.argv[2])
deadline = time.monotonic() + patience
while os.getloadavg()[0] >= limit:
    if time.monotonic() > deadline:
        sys.exit(f"ERROR: the 1-minute load stayed at or above {limit}")
    time.sleep(10)
PY
    BENCH="$CLIENT_BIN" ROUNDS="$WIRE_ROUNDS" CLIENTS="$CLIENTS" MODES=update PROTOCOLS="extended simple" \
        "$PUB/wire_pk_ab.sh" "$OUT/wire" "$BASE_LABEL=$BASE_BIN" "$CAND_LABEL=$CAND_BIN"
fi

if has_phase report; then
    {
        echo "# Release benchmark: $CAND_LABEL against $BASE_LABEL"
        echo
        echo "Produced by \`benches/public/release_bench.sh $REV\`. Every number below is copied from this run's output."
        echo
        echo "| | |"
        echo "|---|---|"
        echo "| date (UTC) | $(date -u +%FT%TZ) |"
        # The platform only: a report meant to be published names no machine.
        echo "| platform | $(uname -sm) |"
        echo "| toolchain | $(cd "$ROOT" && rustc -V); $(cd "$ROOT" && ${CARGO} -V) |"
        echo "| $CAND_LABEL | commit \`$(git -C "$ROOT" rev-parse HEAD)\`, $(git -C "$ROOT" status --porcelain | wc -l) uncommitted file(s) |"
        echo "| $BASE_LABEL | commit \`$BASE_COMMIT\`, pg35 harness files copied from $CAND_LABEL |"
        for binary in "$CAND_BIN" "$BASE_BIN" "$CLIENT_BIN"; do
            [ -x "$binary" ] && echo "| \`${binary#"$OUT"/}\` | sha256 \`$(sha256sum "$binary" | cut -d' ' -f1)\` |"
        done
        echo
        echo "Both server binaries and both pg35 harnesses are built with \`--release --locked\` (LTO, one codegen unit)."
        if [ -f "$OUT/pg35/summary.md" ]; then
            echo
            echo "## pg35: 35 SQL categories against PostgreSQL 18.4"
            echo
            cat "$OUT/pg35/summary.md"
        fi
        if [ -f "$OUT/wire/summary.md" ]; then
            echo
            echo "## Wire: one-row UPDATE by primary key against a running server"
            echo
            echo "\`extended\` is a prepared statement bound per execution; \`simple\` is the statement sent as text."
            echo "Client and server share the host. Each client has its own connection and its own rows."
            echo
            cat "$OUT/wire/summary.md"
        fi
        echo
        echo "## Files"
        echo
        echo "- \`pg35/build/<label>/\`: build receipt, source manifest and the measured test binary"
        echo "- \`pg35/arms/<label>-r<round>/\`: the harness's strict receipt and log of every run"
        echo "- \`pg35/postgres-preflight.json\`: identity and settings of the PostgreSQL reference"
        echo "- \`wire/results.jsonl\`, \`wire/provenance.txt\`: every wire run with its latency percentiles"
    } > "$OUT/RESULTS.md"
    echo "wrote $OUT/RESULTS.md"
fi
