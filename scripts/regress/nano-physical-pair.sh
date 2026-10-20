#!/usr/bin/env bash
#
# nano-physical-pair.sh — a HeliosDB-Nano primary and its PHYSICAL standby on
# loopback (spec P1.3 §5, docs/plans/specs/P1_3_standby_start_posture_pgwire.md).
#
# The pair HeliosDB Proxy's lab and the live probes run against:
#   primary  PostgreSQL 127.0.0.1:27721  (writes; physical source 127.0.0.1:27742)
#   standby  PostgreSQL 127.0.0.1:27722  (read-only physical replica)
# both with trust auth, so Proxy's health and topology logins (postgres /
# postgres, application_name heliosdb-proxy-health / helios-topology) are
# accepted in every posture. The standby is bootstrapped from the primary into
# a fresh directory, follows it, and survives SIGKILL / restart of either side
# (it reconnects, and a restarted standby resumes from its durable cursor).
#
# Usage:
#   nano-physical-pair.sh --binary PATH [--dir DIR] [options] [ACTION ...]
#
# Actions (in the order given; default --start):
#   --start                    start both sides (whichever is not running) and wait for /ready
#   --load N [--duration S]    N single-row INSERTs per second on the primary for S seconds
#                              (default 10); prints the measured ops/s
#   --kill primary|standby     SIGKILL one side
#   --restart primary|standby|both
#                              start a side that is not running (after --kill), and wait for it
#   --stop                     SIGTERM both sides
#   --status                   print the summary only
#
# Options:
#   --dir DIR                  state directory: data, logs, token, pids (default ./nano-physical-pair)
#   --primary-port P           default 27721      --standby-port P        default 27722
#   --replication-port P       default 27741      --physical-port P       default 27742
#   --primary-http-port P      default 27751      --standby-http-port P   default 27752
#   --max-standby-lag D        the standby's [replication] max_standby_lag (default 30s)
#   --timeout S                how long --start / --restart wait for readiness (default 120)
#
# Every invocation ends with one JSON line on stdout:
#   {"ports":{...},"pids":{"primary":N|null,"standby":N|null},"ops_per_s":X|null,
#    "ready":{"primary":CODE|null,"standby":CODE|null}}
# where ready.* is the HTTP status of GET /ready (200 ready, 503 not ready).
# Proxy's lag threshold: lag_bytes is "write operations behind" (the physical
# sequence delta), so at the measured ops/s a max_lag_bytes of ops_per_s * 30
# allows about 30 s of lag (1000 ops/s -> 30000).
#
# Requires bash and python3 (standard library only).

set -euo pipefail
# The physical source refuses a group/world-writable primary directory and
# the receiver a group/world-writable standby parent: everything this pair
# creates is owner-only.
umask 077

BIN=""
DIR="./nano-physical-pair"
PG_P=27721
PG_S=27722
REPL_P=27741
PHYS_P=27742
HTTP_P=27751
HTTP_S=27752
MAX_LAG="30s"
TIMEOUT=120
DURATION=10
ACTIONS=()

usage() {
    sed -n '3,40p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
    echo "nano-physical-pair: $*" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) BIN="${2:?--binary needs a path}"; shift 2 ;;
        --dir) DIR="${2:?--dir needs a path}"; shift 2 ;;
        --primary-port) PG_P="${2:?}"; shift 2 ;;
        --standby-port) PG_S="${2:?}"; shift 2 ;;
        --replication-port) REPL_P="${2:?}"; shift 2 ;;
        --physical-port) PHYS_P="${2:?}"; shift 2 ;;
        --primary-http-port) HTTP_P="${2:?}"; shift 2 ;;
        --standby-http-port) HTTP_S="${2:?}"; shift 2 ;;
        --max-standby-lag) MAX_LAG="${2:?}"; shift 2 ;;
        --timeout) TIMEOUT="${2:?}"; shift 2 ;;
        --duration) DURATION="${2:?}"; shift 2 ;;
        --start | --stop | --status) ACTIONS+=("${1#--}"); shift ;;
        --load) ACTIONS+=("load:${2:?--load needs a rate}"); shift 2 ;;
        --kill)
            case "${2:-}" in primary | standby) ;; *) die "--kill primary|standby" ;; esac
            ACTIONS+=("kill:$2"); shift 2 ;;
        --restart)
            case "${2:-}" in primary | standby | both) ;; *) die "--restart primary|standby|both" ;; esac
            ACTIONS+=("restart:$2"); shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done
[ -n "$BIN" ] || die "--binary PATH is required (a built heliosdb-nano)"
[ -x "$BIN" ] || die "not an executable: $BIN"
command -v python3 >/dev/null || die "python3 is required"
[ ${#ACTIONS[@]} -gt 0 ] || ACTIONS=(start)

mkdir -p "$DIR"
# The receiver installs the standby by renaming a verified copy into place:
# its parent must be owner-only (no group/world write).
chmod 700 "$DIR"
DIR="$(cd "$DIR" && pwd)"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
TOKEN="$DIR/replication.token"
HELPER="$DIR/pair_helper.py"
OPS_FILE="$DIR/ops_per_s"

if [ ! -f "$TOKEN" ]; then
    (umask 077 && head -c 32 /dev/urandom >"$TOKEN")
fi
chmod 600 "$TOKEN"
printf '[replication]\nmax_standby_lag = "%s"\n' "$MAX_LAG" >"$DIR/standby.toml"

cat >"$HELPER" <<'PY'
"""Wire helpers for nano-physical-pair.sh: HTTP status, PostgreSQL simple query, paced load."""
import json, socket, struct, sys, time, urllib.error, urllib.request


def http_code(port, path):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=3) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code
    except Exception:
        return None


def recv_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError("connection closed")
        data += chunk
    return data


def read_message(sock):
    header = recv_exact(sock, 5)
    (length,) = struct.unpack("!i", header[1:])
    return header[:1], recv_exact(sock, length - 4)


def until_ready(sock):
    error = None
    while True:
        kind, body = read_message(sock)
        if kind == b"E":
            fields = dict((f[:1], f[1:].decode("utf-8", "replace")) for f in body.split(b"\0") if f)
            error = (fields.get(b"C"), fields.get(b"M"))
        elif kind == b"Z":
            return error


def connect(port, application_name="nano-physical-pair"):
    sock = socket.create_connection(("127.0.0.1", port), timeout=10)
    params = b"user\0postgres\0database\0postgres\0application_name\0" + application_name.encode() + b"\0\0"
    sock.sendall(struct.pack("!ii", len(params) + 8, 196608) + params)
    error = until_ready(sock)
    if error:
        raise RuntimeError(f"startup refused: {error}")
    return sock


def query(sock, sql):
    payload = sql.encode() + b"\0"
    sock.sendall(b"Q" + struct.pack("!i", len(payload) + 4) + payload)
    return until_ready(sock)


def scalar(sock, sql):
    payload = sql.encode() + b"\0"
    sock.sendall(b"Q" + struct.pack("!i", len(payload) + 4) + payload)
    value = None
    while True:
        kind, body = read_message(sock)
        if kind == b"D":
            (length,) = struct.unpack("!i", body[2:6])
            value = None if length < 0 else body[6 : 6 + length].decode()
        elif kind == b"E":
            raise RuntimeError(body.decode("utf-8", "replace"))
        elif kind == b"Z":
            return value


def wait_http(port, path, want, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if http_code(port, path) == want:
            return 0
        time.sleep(0.25)
    return 1


def wait_pg(port, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            connect(port).close()
            return 0
        except Exception:
            time.sleep(0.25)
    return 1


def load(port, rate, duration):
    sock = connect(port)
    error = query(sock, "CREATE TABLE IF NOT EXISTS pair_load (id BIGINT PRIMARY KEY, written_at TEXT)")
    if error:
        raise RuntimeError(f"create pair_load: {error}")
    next_id = int(scalar(sock, "SELECT coalesce(max(id), 0) FROM pair_load") or 0) + 1
    interval = 1.0 / rate
    done = failed = 0
    start = time.monotonic()
    end = start + duration
    while True:
        due = start + (done + failed) * interval
        now = time.monotonic()
        if due >= end:
            break
        if due > now:
            time.sleep(due - now)
        error = query(sock, f"INSERT INTO pair_load VALUES ({next_id}, '{time.time():.6f}')")
        next_id += 1
        if error:
            failed += 1
        else:
            done += 1
    elapsed = max(time.monotonic() - start, 1e-9)
    print(json.dumps({"requested_per_s": rate, "ops": done, "failed": failed,
                      "seconds": round(elapsed, 3), "ops_per_s": round(done / elapsed, 1)}), file=sys.stderr)
    print(round(done / elapsed, 1))
    return 0


if __name__ == "__main__":
    command, args = sys.argv[1], sys.argv[2:]
    if command == "http-code":
        code = http_code(int(args[0]), args[1])
        print("null" if code is None else code)
    elif command == "wait-http":
        sys.exit(wait_http(int(args[0]), args[1], int(args[2]), float(args[3])))
    elif command == "wait-pg":
        sys.exit(wait_pg(int(args[0]), float(args[1])))
    elif command == "load":
        sys.exit(load(int(args[0]), float(args[1]), float(args[2])))
    else:
        sys.exit(f"unknown helper command {command}")
PY

pid_of() {
    local file="$DIR/$1.pid"
    [ -f "$file" ] || return 1
    local pid
    pid="$(cat "$file")"
    kill -0 "$pid" 2>/dev/null || return 1
    echo "$pid"
}

start_primary() {
    pid_of primary >/dev/null && return 0
    nohup "$BIN" start --data-dir "$DIR/primary" --listen 127.0.0.1 --port "$PG_P" --auth trust \
        --replication-role primary --replication-port "$REPL_P" \
        --physical-replication-listen "127.0.0.1:$PHYS_P" --replication-token-file "$TOKEN" \
        --http-port "$HTTP_P" </dev/null >>"$DIR/primary.log" 2>&1 &
    echo $! >"$DIR/primary.pid"
    python3 "$HELPER" wait-pg "$PG_P" "$TIMEOUT" || die "primary did not accept connections; see $DIR/primary.log"
}

start_standby() {
    pid_of standby >/dev/null && return 0
    nohup "$BIN" start --data-dir "$DIR/standby" --listen 127.0.0.1 --port "$PG_S" --auth trust \
        --replication-role standby --primary-host "127.0.0.1:$REPL_P" \
        --physical-replication-listen "127.0.0.1:$PHYS_P" --replication-token-file "$TOKEN" \
        --http-port "$HTTP_S" --config "$DIR/standby.toml" </dev/null >>"$DIR/standby.log" 2>&1 &
    echo $! >"$DIR/standby.pid"
}

wait_standby_ready() {
    python3 "$HELPER" wait-http "$HTTP_S" /ready 200 "$TIMEOUT" ||
        echo "nano-physical-pair: the standby is not ready after ${TIMEOUT}s; see $DIR/standby.log" >&2
}

kill_side() {
    local pid
    if pid="$(pid_of "$1")"; then
        kill -KILL "$pid"
        while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
    fi
    rm -f "$DIR/$1.pid"
}

stop_side() {
    local pid
    if pid="$(pid_of "$1")"; then
        kill -TERM "$pid"
        for _ in $(seq 1 200); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -0 "$pid" 2>/dev/null && kill -KILL "$pid"
    fi
    rm -f "$DIR/$1.pid"
}

summary() {
    local p s ops rp rs
    p="$(pid_of primary || echo null)"
    s="$(pid_of standby || echo null)"
    ops="$(cat "$OPS_FILE" 2>/dev/null || echo null)"
    rp="$(python3 "$HELPER" http-code "$HTTP_P" /ready)"
    rs="$(python3 "$HELPER" http-code "$HTTP_S" /ready)"
    printf '{"ports":{"primary":%s,"standby":%s,"replication":%s,"physical":%s,"primary_http":%s,"standby_http":%s},"pids":{"primary":%s,"standby":%s},"ops_per_s":%s,"ready":{"primary":%s,"standby":%s},"dir":"%s"}\n' \
        "$PG_P" "$PG_S" "$REPL_P" "$PHYS_P" "$HTTP_P" "$HTTP_S" "$p" "$s" "$ops" "$rp" "$rs" "$DIR"
}

for action in "${ACTIONS[@]}"; do
    case "$action" in
        start)
            start_primary
            start_standby
            wait_standby_ready
            ;;
        stop)
            stop_side standby
            stop_side primary
            ;;
        status) ;;
        load:*)
            rate="${action#load:}"
            python3 "$HELPER" load "$PG_P" "$rate" "$DURATION" >"$OPS_FILE.tmp" && mv "$OPS_FILE.tmp" "$OPS_FILE"
            ;;
        kill:*) kill_side "${action#kill:}" ;;
        restart:primary) start_primary ;;
        restart:standby)
            start_standby
            wait_standby_ready
            ;;
        restart:both)
            start_primary
            start_standby
            wait_standby_ready
            ;;
    esac
done
summary
