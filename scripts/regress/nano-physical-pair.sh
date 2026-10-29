#!/usr/bin/env bash
#
# nano-physical-pair.sh — a HeliosDB-Nano primary and its PHYSICAL standby on
# loopback (spec P1.3 §5, docs/plans/specs/P1_3_standby_start_posture_pgwire.md;
# spec P2.1: one replication connection; spec P2.2: --tls,
# docs/guides/physical-standby.md).
#
# The pair HeliosDB Proxy's lab and the live probes run against:
#   primary  PostgreSQL 127.0.0.1:27721  (writes; replication port 127.0.0.1:27741)
#   standby  PostgreSQL 127.0.0.1:27722  (read-only physical replica of it)
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
#   --pause primary|standby    SIGSTOP one side (a paused standby holds up sync commits)
#   --resume primary|standby   SIGCONT one side
#   --restart primary|standby|both
#                              start a side that is not running (after --kill), and wait for it
#   --stop                     SIGTERM both sides
#   --promote standby          spec P5.1: stop the standby, promote it with `heliosdb-nano promote`
#                              (the old primary is FENCED through the live session and stays up:
#                              reads only, /ready 503, writes refused 25006, pre-fence sessions
#                              ended FATAL 57P01), and start the promoted node as the PRIMARY on
#                              the standby's own ports (PG --standby-port, HTTP --standby-http-port,
#                              replication --standby-replication-port). From then on --restart
#                              standby starts it as a primary again (its directory carries the
#                              primary's dataset identity), and --restart primary starts the old
#                              primary fenced.
#   --status                   print the summary only
#
# Options:
#   --dir DIR                  state directory: data, logs, token, pids (default ./nano-physical-pair)
#   --primary-port P           default 27721      --standby-port P        default 27722
#   --replication-port P       default 27741 (the primary's ONE replication port)
#   --standby-replication-port P
#                              default 27742 (the port the standby serves on once promoted)
#   --primary-http-port P      default 27751      --standby-http-port P   default 27752
#   --max-standby-lag D        the standby's [replication] max_standby_lag (default 30s)
#   --sync-mode M              the primary's [replication] sync_mode: async | semi-sync | sync
#                              (default async); --min-acks N, --sync-timeout-ms MS and
#                              --sync-timeout-action warn|fail tune it
#   --timeout S                how long --start / --restart wait for readiness (default 120)
#   --tls                      replicate over TLS (spec P2.2): a throwaway CA and a server
#                              certificate for 127.0.0.1 are generated with openssl under
#                              DIR/tls, the primary serves --replication-tls and the standby
#                              verifies it with --primary-ca. Remembered for DIR (later
#                              invocations without --tls keep it). TLS is on the replication
#                              port only: the PostgreSQL and HTTP ports are unchanged.
#
# Every invocation ends with one JSON line on stdout:
#   {"ports":{...},"pids":{"primary":N|null,"standby":N|null},"ops_per_s":X|null,
#    "ready":{"primary":CODE|null,"standby":CODE|null},"standbys":N|null,
#    "tls":true|false|null,"archive":{"segments":N,"bytes":N}|null,
#    "nodes":{"primary":{"role":R,"state":S,"epoch":E}|null,"standby":{...}|null},...}
# where ready.* is the HTTP status of GET /ready (200 ready, 503 not ready),
# standbys is how many standbys the primary's /health lists, tls is whether
# the standby's replication session is on TLS (its /health; null when it does
# not answer), and archive is the primary's archived WAL (spec P3.1: sealed
# segments and their bytes, from its /health; null when it does not answer).
# nodes.* is each side's /health replication block (role primary|standby, state
# streaming|fenced|..., epoch = the replication epoch = pg_control_checkpoint().timeline_id).
# A fenced old primary keeps its old epoch, answers pg_is_in_recovery() = false,
# raises 55000 from heliosdb_require_ready(), and SHOW transaction_read_only = on.
# Proxy's lag threshold: lag_bytes is "write operations behind" (the physical
# sequence delta), so at the measured ops/s a max_lag_bytes of ops_per_s * 30
# allows about 30 s of lag (1000 ops/s -> 30000).
#
# Requires bash and python3 (standard library only); --tls also needs openssl.

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
REPL_S=27742
HTTP_P=27751
HTTP_S=27752
MAX_LAG="30s"
SYNC_MODE="async"
MIN_ACKS=1
SYNC_TIMEOUT_MS=10000
SYNC_TIMEOUT_ACTION="warn"
TIMEOUT=120
DURATION=10
TLS=0
ACTIONS=()

usage() {
    sed -n '3,/^# Requires/p' "$0" | sed 's/^# \{0,1\}//'
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
        --standby-replication-port) REPL_S="${2:?}"; shift 2 ;;
        --primary-http-port) HTTP_P="${2:?}"; shift 2 ;;
        --standby-http-port) HTTP_S="${2:?}"; shift 2 ;;
        --max-standby-lag) MAX_LAG="${2:?}"; shift 2 ;;
        --sync-mode) SYNC_MODE="${2:?}"; shift 2 ;;
        --min-acks) MIN_ACKS="${2:?}"; shift 2 ;;
        --sync-timeout-ms) SYNC_TIMEOUT_MS="${2:?}"; shift 2 ;;
        --sync-timeout-action) SYNC_TIMEOUT_ACTION="${2:?}"; shift 2 ;;
        --timeout) TIMEOUT="${2:?}"; shift 2 ;;
        --duration) DURATION="${2:?}"; shift 2 ;;
        --tls) TLS=1; shift ;;
        --start | --stop | --status) ACTIONS+=("${1#--}"); shift ;;
        --load) ACTIONS+=("load:${2:?--load needs a rate}"); shift 2 ;;
        --kill | --pause | --resume)
            case "${2:-}" in primary | standby) ;; *) die "$1 primary|standby" ;; esac
            ACTIONS+=("${1#--}:$2"); shift 2 ;;
        --restart)
            case "${2:-}" in primary | standby | both) ;; *) die "--restart primary|standby|both" ;; esac
            ACTIONS+=("restart:$2"); shift 2 ;;
        --promote)
            case "${2:-}" in standby) ;; *) die "--promote standby" ;; esac
            ACTIONS+=("promote:$2"); shift 2 ;;
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

# Spec P2.2: TLS on the replication port, remembered for this state directory.
TLS_DIR="$DIR/tls"
if [ "$TLS" = 1 ]; then
    : >"$DIR/tls.enabled"
elif [ -f "$DIR/tls.enabled" ]; then
    TLS=1
fi
PRIMARY_TLS_ARGS=()
STANDBY_TLS_ARGS=()
if [ "$TLS" = 1 ]; then
    command -v openssl >/dev/null || die "--tls needs openssl"
    if [ ! -f "$TLS_DIR/server.crt" ]; then
        mkdir -p "$TLS_DIR"
        chmod 700 "$TLS_DIR"
        openssl req -x509 -newkey rsa:2048 -nodes -keyout "$TLS_DIR/ca.key" -out "$TLS_DIR/ca.crt" \
            -days 30 -subj "/CN=nano-physical-pair test CA" >/dev/null 2>&1 || die "openssl: CA"
        openssl req -newkey rsa:2048 -nodes -keyout "$TLS_DIR/server.key" -out "$TLS_DIR/server.csr" \
            -subj "/CN=nano-physical-pair primary" >/dev/null 2>&1 || die "openssl: server key"
        printf 'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1,DNS:localhost\n' \
            >"$TLS_DIR/server.ext"
        openssl x509 -req -in "$TLS_DIR/server.csr" -CA "$TLS_DIR/ca.crt" -CAkey "$TLS_DIR/ca.key" \
            -CAcreateserial -out "$TLS_DIR/server.crt" -days 30 -extfile "$TLS_DIR/server.ext" \
            >/dev/null 2>&1 || die "openssl: server certificate"
        chmod 600 "$TLS_DIR/ca.key" "$TLS_DIR/server.key"
    fi
    PRIMARY_TLS_ARGS=(--replication-tls --replication-tls-cert "$TLS_DIR/server.crt"
        --replication-tls-key "$TLS_DIR/server.key")
    STANDBY_TLS_ARGS=(--primary-tls --primary-ca "$TLS_DIR/ca.crt")
fi
printf '[replication]\nmax_standby_lag = "%s"\n' "$MAX_LAG" >"$DIR/standby.toml"
printf '[replication]\nsync_mode = "%s"\nmin_acks = %s\nsync_timeout_ms = %s\nsync_timeout_action = "%s"\n' \
    "$SYNC_MODE" "$MIN_ACKS" "$SYNC_TIMEOUT_MS" "$SYNC_TIMEOUT_ACTION" >"$DIR/primary.toml"

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


def standby_count(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=3) as response:
            return len(json.load(response).get("standbys", []))
    except Exception:
        return None


def replication_tls(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=3) as response:
            value = json.load(response).get("replication", {}).get("tls")
            return value if isinstance(value, bool) else None
    except Exception:
        return None


def archive_summary(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=3) as response:
            archive = json.load(response).get("archive")
            if not isinstance(archive, dict):
                return None
            return {"segments": archive.get("segments"), "bytes": archive.get("bytes_retained")}
    except Exception:
        return None


def posture(port):
    """The node's /health replication block, reduced to role/state/epoch (null when silent)."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=3) as response:
            block = json.load(response).get("replication")
            if not isinstance(block, dict):
                return None
            return {"role": block.get("role"), "state": block.get("state"), "epoch": block.get("epoch")}
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
    elif command == "standby-count":
        count = standby_count(int(args[0]))
        print("null" if count is None else count)
    elif command == "replication-tls":
        tls = replication_tls(int(args[0]))
        print("null" if tls is None else ("true" if tls else "false"))
    elif command == "archive":
        print(json.dumps(archive_summary(int(args[0])), separators=(",", ":")))
    elif command == "posture":
        print(json.dumps(posture(int(args[0])), separators=(",", ":")))
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
        --replication-role primary --replication-port "$REPL_P" --replication-token-file "$TOKEN" \
        ${PRIMARY_TLS_ARGS[@]+"${PRIMARY_TLS_ARGS[@]}"} \
        --http-port "$HTTP_P" --config "$DIR/primary.toml" </dev/null >>"$DIR/primary.log" 2>&1 &
    echo $! >"$DIR/primary.pid"
    python3 "$HELPER" wait-pg "$PG_P" "$TIMEOUT" || die "primary did not accept connections; see $DIR/primary.log"
}

# A promoted standby's directory carries the primary's dataset identity
# (spec P5.1): it is started as the PRIMARY on the standby's ports from then on.
standby_is_promoted() {
    [ -f "$DIR/standby/NANO-PRIMARY-HISTORY.json" ]
}

start_standby() {
    pid_of standby >/dev/null && return 0
    if standby_is_promoted; then
        nohup "$BIN" start --data-dir "$DIR/standby" --listen 127.0.0.1 --port "$PG_S" --auth trust \
            --replication-role primary --replication-port "$REPL_S" --replication-token-file "$TOKEN" \
            ${PRIMARY_TLS_ARGS[@]+"${PRIMARY_TLS_ARGS[@]}"} \
            --http-port "$HTTP_S" --config "$DIR/primary.toml" </dev/null >>"$DIR/standby.log" 2>&1 &
        echo $! >"$DIR/standby.pid"
        python3 "$HELPER" wait-pg "$PG_S" "$TIMEOUT" || die "the promoted node did not accept connections; see $DIR/standby.log"
        return 0
    fi
    nohup "$BIN" start --data-dir "$DIR/standby" --listen 127.0.0.1 --port "$PG_S" --auth trust \
        --replication-role standby --primary-host "127.0.0.1:$REPL_P" --replication-token-file "$TOKEN" \
        ${STANDBY_TLS_ARGS[@]+"${STANDBY_TLS_ARGS[@]}"} \
        --http-port "$HTTP_S" --config "$DIR/standby.toml" </dev/null >>"$DIR/standby.log" 2>&1 &
    echo $! >"$DIR/standby.pid"
}

# Spec P5.1: promote the standby. It is stopped first; the fence goes through
# the live session to the old primary, which stays up fenced; the promoted
# node then serves as the primary on the standby's ports.
promote_standby() {
    standby_is_promoted && die "the standby is already promoted (its directory carries NANO-PRIMARY-HISTORY.json)"
    stop_side standby
    "$BIN" promote --data-dir "$DIR/standby" --primary-host "127.0.0.1:$REPL_P" --replication-token-file "$TOKEN" \
        ${STANDBY_TLS_ARGS[@]+"${STANDBY_TLS_ARGS[@]}"} \
        --config "$DIR/standby.toml" --fence-timeout "$TIMEOUT" >>"$DIR/promote.log" 2>&1 ||
        die "promote failed; see $DIR/promote.log"
    start_standby
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

signal_side() {
    local pid
    if pid="$(pid_of "$2")"; then
        kill "-$1" "$pid"
    fi
}

stop_side() {
    local pid
    if pid="$(pid_of "$1")"; then
        # A paused side handles SIGTERM only once it runs again.
        kill -CONT "$pid" 2>/dev/null || true
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
    local p s ops rp rs count tls archive
    p="$(pid_of primary || echo null)"
    s="$(pid_of standby || echo null)"
    ops="$(cat "$OPS_FILE" 2>/dev/null || echo null)"
    rp="$(python3 "$HELPER" http-code "$HTTP_P" /ready)"
    rs="$(python3 "$HELPER" http-code "$HTTP_S" /ready)"
    count="$(python3 "$HELPER" standby-count "$HTTP_P")"
    tls="$(python3 "$HELPER" replication-tls "$HTTP_S")"
    archive="$(python3 "$HELPER" archive "$HTTP_P")"
    local np ns
    np="$(python3 "$HELPER" posture "$HTTP_P")"
    ns="$(python3 "$HELPER" posture "$HTTP_S")"
    printf '{"ports":{"primary":%s,"standby":%s,"replication":%s,"standby_replication":%s,"primary_http":%s,"standby_http":%s},"pids":{"primary":%s,"standby":%s},"ops_per_s":%s,"ready":{"primary":%s,"standby":%s},"standbys":%s,"tls":%s,"archive":%s,"nodes":{"primary":%s,"standby":%s},"sync_mode":"%s","dir":"%s"}\n' \
        "$PG_P" "$PG_S" "$REPL_P" "$REPL_S" "$HTTP_P" "$HTTP_S" "$p" "$s" "$ops" "$rp" "$rs" "$count" "$tls" "$archive" "$np" "$ns" "$SYNC_MODE" "$DIR"
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
        pause:*) signal_side STOP "${action#pause:}" ;;
        resume:*) signal_side CONT "${action#resume:}" ;;
        restart:primary) start_primary ;;
        promote:standby) promote_standby ;;
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
