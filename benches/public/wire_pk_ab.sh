#!/usr/bin/env bash
# Primary-key write throughput over the PostgreSQL wire: several server
# binaries, one client (examples/wire_pk_bench.rs), ABBA order.
#
#   benches/public/wire_pk_ab.sh <out-dir> <label>=<server-binary>[@ENV=VALUE...] ...
#
# The first variant is the baseline of the summary's ratios. A variant may set
# environment variables for its server, e.g. to measure a switch:
#
#   benches/public/wire_pk_ab.sh out \
#       v4.41.1=/builds/v4411/heliosdb-nano \
#       tip=/builds/tip/heliosdb-nano \
#       tip-nooff=/builds/tip/heliosdb-nano@HELIOSDB_NANO_STATEMENT_OFFLOAD=off
#
# Each round starts every variant's server on a fresh data directory (trust
# authentication, loopback, HTTP off), runs every mode x protocol x client
# count against it and stops it; odd rounds run the variants in the order
# given, even rounds in reverse, so drift in the host's load falls on both
# sides. The load average is recorded with every run.
#
# Environment:
#   BENCH      the client binary                  (default: built from this tree, perf profile)
#   ROUNDS     rounds                             (default 2)
#   CLIENTS    client counts                      (default "1 16 64")
#   MODES      update | delete-insert | select    (default "update")
#   PROTOCOLS  extended | simple                  (default "extended simple")
#   SECONDS_PER_RUN / WARMUP / ROWS               (default 20 / 3 / 64000)
#   SERVER_ARGS  extra arguments for `start`      (default none)
#
# Needs python3 (standard library) for a free port and for the summary.
set -euo pipefail

PUB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$PUB/../.." && pwd)"

[ $# -ge 2 ] || { sed -n '2,29p' "$0" >&2; exit 2; }
OUT="$(mkdir -p "$1" && cd "$1" && pwd)"; shift
VARIANTS=("$@")
ROUNDS="${ROUNDS:-2}"
CLIENTS="${CLIENTS:-1 16 64}"
MODES="${MODES:-update}"
PROTOCOLS="${PROTOCOLS:-extended simple}"
SECONDS_PER_RUN="${SECONDS_PER_RUN:-20}"
WARMUP="${WARMUP:-3}"
ROWS="${ROWS:-64000}"
SERVER_ARGS="${SERVER_ARGS:-}"

if [ -z "${BENCH:-}" ]; then
    (cd "$ROOT" && cargo build --profile perf --example wire_pk_bench --quiet)
    BENCH="${CARGO_TARGET_DIR:-$ROOT/target}/perf/examples/wire_pk_bench"
fi
[ -x "$BENCH" ] || { echo "ERROR: client binary '$BENCH' not found" >&2; exit 1; }

free_port() {
    python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

{
    date -u +%FT%TZ
    uname -sm
    echo "cpus=$(nproc) rounds=$ROUNDS clients=[$CLIENTS] modes=[$MODES] protocols=[$PROTOCOLS]"
    echo "seconds=$SECONDS_PER_RUN warmup=$WARMUP rows=$ROWS server_args=[$SERVER_ARGS]"
    echo "client=$BENCH sha256=$(sha256sum "$BENCH" | cut -d' ' -f1)"
    for variant in "${VARIANTS[@]}"; do
        label="${variant%%=*}"; rest="${variant#*=}"; binary="${rest%%@*}"
        [ -x "$binary" ] || { echo "ERROR: server binary '$binary' ($label) not found" >&2; exit 1; }
        echo "variant $label: $rest sha256=$(sha256sum "$binary" | cut -d' ' -f1) version=$("$binary" --version)"
    done
} | tee "$OUT/provenance.txt"

SERVER_PID=""
stop_server() {
    if [ -n "$SERVER_PID" ]; then
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        SERVER_PID=""
    fi
}
trap stop_server EXIT

run_variant() {
    local round="$1" variant="$2"
    local label="${variant%%=*}" rest="${variant#*=}"
    local binary="${rest%%@*}" settings=()
    if [ "$rest" != "$binary" ]; then
        IFS='@' read -r -a settings <<< "${rest#*@}"
    fi
    local data port log
    data="$(mktemp -d "$OUT/data-$label-r$round-XXXXXX")"
    port="$(free_port)"
    log="$OUT/server-$label-r$round.log"
    # shellcheck disable=SC2086
    env "${settings[@]}" "$binary" start --data-dir "$data/db" --listen 127.0.0.1 --auth trust \
        --port "$port" --http-port 0 $SERVER_ARGS > "$log" 2>&1 &
    SERVER_PID=$!
    local mode protocol clients
    for mode in $MODES; do
        for protocol in $PROTOCOLS; do
            for clients in $CLIENTS; do
                if ! kill -0 "$SERVER_PID" 2>/dev/null; then
                    echo "ERROR: the server of '$label' exited; see $log" >&2
                    exit 1
                fi
                "$BENCH" --port "$port" --clients "$clients" --seconds "$SECONDS_PER_RUN" --warmup "$WARMUP" \
                    --rows "$ROWS" --mode "$mode" --protocol "$protocol" --label "$label" \
                    | sed "s/^{/{\"round\":$round,/" | tee -a "$OUT/results.jsonl"
            done
        done
    done
    stop_server
    rm -rf "$data"
}

for round in $(seq 1 "$ROUNDS"); do
    order=("${VARIANTS[@]}")
    if [ $((round % 2)) -eq 0 ]; then
        order=()
        for ((i = ${#VARIANTS[@]} - 1; i >= 0; i--)); do order+=("${VARIANTS[i]}"); done
    fi
    for variant in "${order[@]}"; do
        run_variant "$round" "$variant"
    done
done

python3 - "$OUT/results.jsonl" "${VARIANTS[0]%%=*}" <<'PY' | tee "$OUT/summary.md"
import json, statistics, sys

runs = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
baseline = sys.argv[2]
labels = list(dict.fromkeys(run["label"] for run in runs))
cells = {}
for run in runs:
    cells.setdefault((run["mode"], run["protocol"], run["clients"]), {}).setdefault(run["label"], []).append(run)

print(f"Median of {max(len(v) for c in cells.values() for v in c.values())} round(s); ratio is ops/s against `{baseline}`.")
print()
print("| mode | protocol | clients | variant | ops/s | ratio | p50 us | p99 us | errors | load |")
print("|---|---|---:|---|---:|---:|---:|---:|---:|---|")
for (mode, protocol, clients), by_label in sorted(cells.items()):
    base = statistics.median(r["ops_per_sec"] for r in by_label[baseline]) if baseline in by_label else None
    for label in labels:
        rows = by_label.get(label)
        if not rows:
            continue
        ops = statistics.median(r["ops_per_sec"] for r in rows)
        ratio = f"{ops / base:.2f}" if base else "-"
        p50 = statistics.median(r["p50_us"] for r in rows)
        p99 = statistics.median(r["p99_us"] for r in rows)
        errors = sum(r["errors"] for r in rows)
        load = "/".join(str(r["load_before"]) for r in rows)
        print(f"| {mode} | {protocol} | {clients} | {label} | {ops:,.0f} | {ratio} | {p50:,.0f} | {p99:,.0f} | {errors} | {load} |")
PY
