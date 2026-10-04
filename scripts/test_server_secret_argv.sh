#!/usr/bin/env bash
#
# test_server_secret_argv.sh — real-process P0-2 regression for daemon secret
# exposure on argv. GATED: it never runs in a normal test pass.
#
# WHAT IT DOES
#   For each supported secret source it starts an isolated
#   `heliosdb-nano start --daemon --memory` and reads the daemon child's
#   /proc/<pid>/cmdline (NUL-separated) to assert the secret VALUE is absent
#   while the process identity (the binary path + `start`) is present:
#
#     1. password file        (--password-file <path>)
#     2. env password         (HELIOSDB_PASSWORD)
#     3. legacy literal       (--password <value>)
#     4. MCP bearer token     (--mcp-token <value>)
#     5. trust plus password  (--auth trust --password <value>)
#
#   It also asserts startup FAILS closed (non-zero exit, no PID file) when the
#   password file is missing, empty, or was deleted.
#
# PREREQUISITES / WHY GATED
#   * Linux with /proc mounted — cmdline inspection is Linux-specific.
#   * A built binary: HELIOSDB_NANO_BIN, else target/release/heliosdb-nano,
#     else target/debug/heliosdb-nano.
#   * `timeout(1)` and `mktemp(1)`.
#   * It forks real daemons and binds real TCP ports, so it is deliberately
#     NOT part of `cargo test`/CI. Run it explicitly:
#
#         HELIOSDB_PROC_REGRESSION=1 scripts/test_server_secret_argv.sh
#
#   Ports are derived from the harness PID:
#   HELIOSDB_PROC_BASE_PORT overrides the base (default 51000 + PID mod 1000).
#
# SCOPE / NON-CLAIMS
#   Assertions cover process identity and argv only. This harness makes NO
#   claim about environment secrecy: a value handed to the child via the
#   environment is still readable by the process owner / privileged users in
#   /proc/<pid>/environ. It only proves the value is not on argv.
#
set -u

if [ "${HELIOSDB_PROC_REGRESSION:-0}" != "1" ]; then
  echo "skip: gated real-process harness; set HELIOSDB_PROC_REGRESSION=1 to run" >&2
  exit 0
fi

if [ "$(uname -s 2>/dev/null)" != "Linux" ] || [ ! -r /proc/self/cmdline ]; then
  echo "skip: requires Linux /proc" >&2
  exit 0
fi

for tool in timeout mktemp; do
  command -v "$tool" >/dev/null 2>&1 || { echo "skip: '$tool' not found" >&2; exit 0; }
done

# Do not let inherited secret env influence the cases.
unset HELIOSDB_PASSWORD HELIOSDB_PASSWORD_FILE HELIOSDB_MCP_TOKEN 2>/dev/null || true

BIN="${HELIOSDB_NANO_BIN:-}"
if [ -z "$BIN" ]; then
  for cand in target/release/heliosdb-nano target/debug/heliosdb-nano; do
    if [ -x "$cand" ]; then BIN="$cand"; break; fi
  done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "skip: set HELIOSDB_NANO_BIN to a built heliosdb-nano executable" >&2
  exit 0
fi
case "$BIN" in
  /*) : ;;
  *) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;;
esac

BASE_PORT="${HELIOSDB_PROC_BASE_PORT:-$(( 51000 + ($$ % 1000) ))}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hdb-proc-regress.XXXXXX")"
PIDS=""
FAILED=0
LAST_PID=""

cleanup() {
  local p
  for p in $PIDS; do kill "$p" 2>/dev/null || true; done
  sleep 0.3
  for p in $PIDS; do kill -9 "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

fail() { echo "FAIL: $*" >&2; FAILED=1; }
pass() { echo "ok: $*"; }

arg_contains() { # pid needle
  tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null | grep -qF -- "$2"
}
argv_has_exact() { # pid arg
  tr '\0' '\n' < "/proc/$1/cmdline" 2>/dev/null | grep -qxF -- "$2"
}
assert_identity() { # name pid
  if arg_contains "$2" "start" && arg_contains "$2" "$(basename "$BIN")"; then
    pass "$1: child identity present (binary + start)"
  else
    fail "$1: child argv does not look like the daemon (identity)"
  fi
}
assert_hides() { # name pid secret
  if arg_contains "$2" "$3"; then
    fail "$1: secret value is present on child argv (/proc/$2/cmdline)"
  else
    pass "$1: secret value absent from child argv"
  fi
}
assert_argv_has() { # name pid needle
  if arg_contains "$2" "$3"; then pass "$1: argv contains $3"; else fail "$1: argv missing $3"; fi
}
assert_argv_lacks() { # name pid arg
  if argv_has_exact "$2" "$3"; then fail "$1: argv contains forbidden $3"; else pass "$1: argv lacks $3"; fi
}

stop_pid() {
  local pid="$1" i=0
  kill "$pid" 2>/dev/null || true
  while [ "$i" -lt 50 ] && kill -0 "$pid" 2>/dev/null; do i=$((i + 1)); sleep 0.1; done
  kill -9 "$pid" 2>/dev/null || true
}

# Start a daemon; on success set LAST_PID. Extra args are appended after the
# common daemon flags.
run_start_ok() { # name pidfile log slot args...
  local name="$1" pidfile="$2" log="$3" slot="$4"; shift 4
  local port=$(( BASE_PORT + slot * 2 )) http
  http=$(( port + 1 ))
  LAST_PID=""
  rm -f "$pidfile"
  if ! timeout 30 "$BIN" start --daemon --memory --listen 127.0.0.1 \
        --port "$port" --http-port "$http" --pid-file "$pidfile" \
        "$@" >"$log" 2>&1; then
    fail "$name: daemon launcher exited non-zero (port $port); see $log"
    sed -n '1,40p' "$log" >&2 || true
    return 1
  fi
  if [ ! -s "$pidfile" ]; then
    fail "$name: launcher succeeded but wrote no PID file"
    return 1
  fi
  local pid
  pid="$(tr -d '[:space:]' < "$pidfile")"
  if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
    fail "$name: PID file does not name a live process ('$pid')"
    return 1
  fi
  PIDS="$PIDS $pid"
  LAST_PID="$pid"
  pass "$name: daemon child pid $pid is live"
  return 0
}

run_start_fail() { # name pidfile log slot args...
  local name="$1" pidfile="$2" log="$3" slot="$4"; shift 4
  local port=$(( BASE_PORT + slot * 2 )) http
  http=$(( port + 1 ))
  rm -f "$pidfile"
  if timeout 30 "$BIN" start --daemon --memory --listen 127.0.0.1 \
        --port "$port" --http-port "$http" --pid-file "$pidfile" \
        "$@" >"$log" 2>&1; then
    fail "$name: expected startup to fail, but it succeeded"
    [ -s "$pidfile" ] && stop_pid "$(tr -d '[:space:]' < "$pidfile")"
    return 1
  fi
  if [ -s "$pidfile" ]; then
    fail "$name: startup failed but a PID file was written ($(tr -d '[:space:]' < "$pidfile"))"
    stop_pid "$(tr -d '[:space:]' < "$pidfile")"
    return 1
  fi
  pass "$name: startup failed closed (non-zero, no PID file)"
  return 0
}

SECRET="hdb-regress-pw-$$"
TOKEN="hdb-regress-token-$$"

echo "== P0-2 real-process secret-argv regression =="
echo "binary: $BIN"
echo "work:   $WORK"
echo "ports:  $BASE_PORT..$(( BASE_PORT + 15 ))"
echo

# 1. password file -------------------------------------------------------
PW_FILE="$WORK/password.txt"
printf '%s' "$SECRET" > "$PW_FILE"
chmod 600 "$PW_FILE"
if run_start_ok "password-file" "$WORK/pid1" "$WORK/pw.log" 0 \
      --auth scram-sha-256 --password-file "$PW_FILE"; then
  assert_identity "password-file" "$LAST_PID"
  assert_hides "password-file" "$LAST_PID" "$SECRET"
  assert_argv_has "password-file" "$LAST_PID" "--password-file"
  assert_argv_has "password-file" "$LAST_PID" "$PW_FILE"
  stop_pid "$LAST_PID"
fi

# 2. env password --------------------------------------------------------
export HELIOSDB_PASSWORD="$SECRET"
if run_start_ok "env-password" "$WORK/pid2" "$WORK/env.log" 1 --auth scram-sha-256; then
  assert_identity "env-password" "$LAST_PID"
  assert_hides "env-password" "$LAST_PID" "$SECRET"
  assert_argv_lacks "env-password" "$LAST_PID" "--password"
  stop_pid "$LAST_PID"
fi
unset HELIOSDB_PASSWORD

# 3. legacy literal --password ------------------------------------------
if run_start_ok "legacy-argv" "$WORK/pid3" "$WORK/argv.log" 2 \
      --auth scram-sha-256 --password "$SECRET"; then
  assert_identity "legacy-argv" "$LAST_PID"
  assert_hides "legacy-argv" "$LAST_PID" "$SECRET"
  assert_argv_lacks "legacy-argv" "$LAST_PID" "--password"
  stop_pid "$LAST_PID"
fi

# 4. MCP bearer token ----------------------------------------------------
if run_start_ok "mcp-token" "$WORK/pid4" "$WORK/mcp.log" 3 \
      --auth trust --mcp-token "$TOKEN"; then
  assert_identity "mcp-token" "$LAST_PID"
  assert_hides "mcp-token" "$LAST_PID" "$TOKEN"
  assert_argv_lacks "mcp-token" "$LAST_PID" "--mcp-token"
  stop_pid "$LAST_PID"
fi

# 5. trust plus password ------------------------------------------------
if run_start_ok "trust-plus-password" "$WORK/pid5" "$WORK/trust.log" 4 \
      --auth trust --password "$SECRET"; then
  assert_identity "trust-plus-password" "$LAST_PID"
  assert_hides "trust-plus-password" "$LAST_PID" "$SECRET"
  stop_pid "$LAST_PID"
fi

# 6. missing / empty / deleted password file ----------------------------
run_start_fail "missing-password-file" "$WORK/pidm" "$WORK/missing.log" 5 \
  --auth scram-sha-256 --password-file "$WORK/does-not-exist.txt"

EMPTY_FILE="$WORK/empty.txt"; : > "$EMPTY_FILE"
run_start_fail "empty-password-file" "$WORK/pide" "$WORK/empty.log" 6 \
  --auth scram-sha-256 --password-file "$EMPTY_FILE"

DELETED_FILE="$WORK/deleted.txt"
printf '%s' "$SECRET" > "$DELETED_FILE"
rm -f "$DELETED_FILE"
run_start_fail "deleted-password-file" "$WORK/pidd" "$WORK/deleted.log" 7 \
  --auth scram-sha-256 --password-file "$DELETED_FILE"

echo
if [ "$FAILED" -ne 0 ]; then
  echo "RESULT: FAIL" >&2
  exit 1
fi
echo "RESULT: PASS — every secret stayed off the daemon child's argv"
exit 0
