#!/usr/bin/env bash
# Reproducible HeliosDB-Nano vs PostgreSQL engine benchmark.
#
# Measures, for PostgreSQL and each supplied Nano binary:
#   1. SELECT 1        — protocol + simple-query throughput / connection scaling
#   2. bulk-load COPY  — load 10k/50k/100k-row CSVs (ms)
#   3. indexed read    — point-read over a 50k-row indexed table (TPS sweep)
#   4. DROP TABLE      — drop a 100k-row table (ms; exposes the O(rows)-fsync stall)
#
# PostgreSQL runs as a container; each Nano version runs as a NATIVE host binary
# (so the engine — not Docker — is what's measured). pgbench (from the PG image)
# is the client over host networking.
#
# Usage:
#   ./bench-engines.sh <ver:nano-binary> [<ver:nano-binary> ...]
# Example:
#   ./bench-engines.sh 3.60.6:/path/heliosdb-nano-3.60.6 3.60.7:/path/heliosdb-nano-3.60.7
#
# Obtaining Nano binaries (per version), from the Nano source repo:
#   git worktree add --detach /tmp/nano-v3.60.7 v3.60.7
#   ( cd /tmp/nano-v3.60.7 && cargo build --release --bin heliosdb-nano )
#   cp /tmp/nano-v3.60.7/target/release/heliosdb-nano ./heliosdb-nano-3.60.7
#
# Env: PGHOST/PGPORT/PGDB/PGUSER/PGPASS (PostgreSQL), NANO_PORT0 (first Nano port),
#      DUR (seconds/cell), CLIENTS, IMG (pgbench/psql image).
set -u
IMG="${IMG:-postgres:18.4-bookworm}"
PGHOST="${PGHOST:-127.0.0.1}"; PGPORT="${PGPORT:-25433}"; PGDB="${PGDB:-benchdb}"
PGUSER="${PGUSER:-bench}"; PGPASS="${PGPASS:-benchpass}"
NANO_USER=bench; NANO_DB=heliosdb
DUR="${DUR:-8}"; CLIENTS="${CLIENTS:-1 8 16 32 64}"; NANO_PORT0="${NANO_PORT0:-5460}"
W="$(mktemp -d /tmp/bench-engines.XXXXXX)"; trap 'rm -rf "$W"' EXIT

# --- fixtures -----------------------------------------------------------------
printf 'SELECT 1;\n' > "$W/s1.sql"
printf '\\set aid random(1, 50000)\nSELECT abalance FROM t50 WHERE aid = :aid;\n' > "$W/s_read.sql"
for n in 10000 50000 100000; do
  awk -v n=$n 'BEGIN{for(i=1;i<=n;i++)printf "%d,%d\n",i,(i*7)%100000}' > "$W/ba_$n.csv"
done

# pgbench TPS+latency for a script. args: host port db passenv script
sweep(){ local h=$1 p=$2 d=$3 pe=$4 scr=$5 c j out
  for c in $CLIENTS; do j=$(( c<8?c:8 ))
    out=$(docker run --rm --network host $pe -v "$W":/w "$IMG" pgbench -h "$h" -p "$p" -U "$NANO_USER" -d "$d" -n -f "/w/$scr" -c "$c" -j "$j" -T "$DUR" 2>&1)
    printf '  c=%-3s tps=%-12s lat=%sms\n' "$c" \
      "$(printf '%s' "$out"|grep -oE 'tps = [0-9.]+'|head -1|grep -oE '[0-9.]+')" \
      "$(printf '%s' "$out"|grep -oE 'latency average = [0-9.]+'|head -1|grep -oE '[0-9.]+')"
  done; }

# bulk-load + read-setup + drop, one psql session each. args: host port db passenv label
storage(){ local h=$1 p=$2 d=$3 pe=$4
  echo "  bulk-load COPY (rows -> ms):"
  docker run --rm --network host $pe -v "$W":/w "$IMG" bash -c '
    P="psql -h '"$h"' -p '"$p"' -U '"$NANO_USER"' -d '"$d"'"
    for n in 10000 50000 100000; do
      tbl=t$n; $P -qc "DROP TABLE IF EXISTS $tbl" >/dev/null 2>&1
      $P -qc "CREATE TABLE $tbl(aid int,abalance int)" >/dev/null 2>&1
      s=$(date +%s%N); timeout 60 $P -c "\copy $tbl FROM /w/ba_$n.csv WITH (FORMAT csv)" >/dev/null 2>&1; r=$?; e=$(date +%s%N)
      m=$(((e-s)/1000000)); [ $r -eq 0 ] && echo "    $n -> ${m}" || echo "    $n -> ${m}_TIMEOUT"
    done
    $P -qc "DROP TABLE IF EXISTS t50; ALTER TABLE t50000 RENAME TO t50" >/dev/null 2>&1
    $P -qc "CREATE INDEX i50 ON t50(aid)" >/dev/null 2>&1'
  echo "  indexed point-read (50k rows):"; sweep "$h" "$p" "$d" "$pe" s_read.sql
  echo "  DROP TABLE (100k rows -> ms):"
  docker run --rm --network host $pe -v "$W":/w "$IMG" bash -c '
    P="psql -h '"$h"' -p '"$p"' -U '"$NANO_USER"' -d '"$d"'"
    $P -qc "DROP TABLE IF EXISTS big" >/dev/null 2>&1; $P -qc "CREATE TABLE big(aid int,abalance int)" >/dev/null 2>&1
    $P -c "\copy big FROM /w/ba_100000.csv WITH (FORMAT csv)" >/dev/null 2>&1
    s=$(date +%s%N); timeout 90 $P -qc "DROP TABLE big" >/dev/null 2>&1; r=$?; e=$(date +%s%N)
    m=$(((e-s)/1000000)); [ $r -eq 0 ] && echo "    100k -> ${m}" || echo "    100k -> ${m}_TIMEOUT"'; }

echo "=================================================================="
echo " HeliosDB-Nano vs PostgreSQL — $DUR s/cell, clients [$CLIENTS]"
echo "=================================================================="
echo; echo "### PostgreSQL ($PGHOST:$PGPORT)"
echo "  SELECT 1:"; sweep "$PGHOST" "$PGPORT" "$PGDB" "-e PGPASSWORD=$PGPASS" s1.sql
storage "$PGHOST" "$PGPORT" "$PGDB" "-e PGPASSWORD=$PGPASS"

port=$NANO_PORT0
for pair in "$@"; do
  ver="${pair%%:*}"; bin="${pair#*:}"; dir="$W/data-$port"; mkdir -p "$dir"
  RUST_LOG=error "$bin" start --listen 127.0.0.1 --port "$port" --auth trust --http-port 0 --data-dir "$dir" >"$W/nano-$port.log" 2>&1 &
  NPID=$!; ready=0
  for _ in $(seq 1 25); do
    docker run --rm --network host "$IMG" psql -h 127.0.0.1 -p "$port" -U "$NANO_USER" -d "$NANO_DB" -tAc "select 1" >/dev/null 2>&1 && { ready=1; break; }
    kill -0 "$NPID" 2>/dev/null || break; sleep 1
  done
  echo; echo "### HeliosDB-Nano $ver (127.0.0.1:$port)"
  if [ "$ready" = 1 ]; then
    echo "  SELECT 1:"; sweep 127.0.0.1 "$port" "$NANO_DB" "" s1.sql
    storage 127.0.0.1 "$port" "$NANO_DB" ""
  else echo "  FAILED to start"; tail -5 "$W/nano-$port.log"; fi
  kill -9 "$NPID" 2>/dev/null; wait "$NPID" 2>/dev/null
  port=$((port+1))
done
echo; echo "done."
