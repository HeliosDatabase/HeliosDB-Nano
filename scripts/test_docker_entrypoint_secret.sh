#!/bin/sh
#
# test_docker_entrypoint_secret.sh — static + stub regression for the Docker
# entrypoint's secret handling. Shell-only: no Cargo build and no real server,
# so it is safe to run anywhere.
#
# It asserts two properties:
#   1. STATIC: deployment/docker/docker-entrypoint.sh never appends a literal
#      `--password <value>` to the server command line, and documents the
#      pre-4.41.1 refusal instead.
#   2. STUB: against a fake `heliosdb-nano` that does NOT advertise
#      `--password-file`, a configured password makes the entrypoint exit
#      non-zero with an upgrade message and WITHOUT executing the binary.
#      Against a fake that DOES advertise it, the entrypoint forwards
#      `--password-file <path>` and never the secret value.
#
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENTRY="$ROOT/deployment/docker/docker-entrypoint.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok: $*"; }

[ -f "$ENTRY" ] || fail "entrypoint not found: $ENTRY"

# Never let an inherited secret environment change the outcome of any case.
unset HELIOSDB_PASSWORD HELIOSDB_PASSWORD_FILE HELIOSDB_MCP_TOKEN 2>/dev/null || true

# ── 1. static checks ─────────────────────────────────────────────────────
if grep -nE 'set -- "\$@" --password([ "=]|$)' "$ENTRY" >/dev/null 2>&1; then
  fail "entrypoint still appends literal --password to the command line"
fi
pass "entrypoint does not append literal --password"

if grep -qF 'will not fall back to --password' "$ENTRY"; then
  pass "entrypoint documents the no-fallback refusal"
else
  fail "entrypoint is missing the pre-4.41.1 refusal message"
fi

# ── 2. stub runs ─────────────────────────────────────────────────────────
TMP="$(mktemp -d "${TMPDIR:-/tmp}/hdb-entrypoint-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM
mkdir -p "$TMP/bin"

writestub() { # help-line for `start --help`
  {
    echo '#!/bin/sh'
    echo 'if [ "${1:-}" = "start" ] && { [ "${2:-}" = "--help" ] || [ "${2:-}" = "-h" ]; }; then'
    printf 'echo "heliosdb-nano start options: %s"\n' "$1"
    echo '  exit 0'
    echo 'fi'
    echo 'printf "%s\\n" "$@" > "${HDB_STUB_ARGV:?}"'
    echo 'exit 0'
  } > "$TMP/bin/heliosdb-nano"
  chmod +x "$TMP/bin/heliosdb-nano"
}

# 2a. pre-4.41.1 binary: refuses.
writestub '--auth --password --data-dir'
set +e
HDB_STUB_ARGV="$TMP/argv-old" \
HELIOSDB_PASSWORD="hdb-entrypoint-secret" \
HELIOSDB_DATA_DIR="$TMP/data" \
HELIOSDB_TLS=off \
PATH="$TMP/bin:$PATH" \
  sh "$ENTRY" start --port 6543 >"$TMP/out-old" 2>"$TMP/err-old"
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "entrypoint did not fail for a pre-4.41.1 binary with HELIOSDB_PASSWORD"
grep -qi 'predates\|upgrade\|not fall back' "$TMP/err-old" \
  || fail "refusal message missing; stderr: $(cat "$TMP/err-old" 2>/dev/null)"
[ ! -e "$TMP/argv-old" ] || fail "stub was executed despite the refusal"
pass "pre-4.41.1 + HELIOSDB_PASSWORD refused without executing the binary"

# 2b. fixed binary: forwards only the path.
writestub '--auth --password-file --data-dir'
printf '%s' 'hdb-entrypoint-secret' > "$TMP/pwfile"
HDB_STUB_ARGV="$TMP/argv-new" \
HELIOSDB_PASSWORD_FILE="$TMP/pwfile" \
HELIOSDB_DATA_DIR="$TMP/data" \
HELIOSDB_TLS=off \
PATH="$TMP/bin:$PATH" \
  sh "$ENTRY" start --port 6543 >"$TMP/out-new" 2>"$TMP/err-new" \
  || fail "entrypoint failed for a --password-file-capable binary; stderr: $(cat "$TMP/err-new" 2>/dev/null)"
grep -q -- '--password-file' "$TMP/argv-new" || fail "password-file not forwarded"
grep -qF "$TMP/pwfile" "$TMP/argv-new" || fail "password-file path not forwarded"
if grep -qx -- '--password' "$TMP/argv-new"; then
  fail "entrypoint forwarded a literal --password"
fi
if grep -qF 'hdb-entrypoint-secret' "$TMP/argv-new"; then
  fail "secret value appears on the forwarded command line"
fi
pass "fixed binary receives --password-file <path>, never the secret value"

# 2c. an explicit --password-file outranks a stale/missing inherited env file.
#     A HELIOSDB_PASSWORD_FILE that does not exist must NOT block the flag.
writestub '--auth --password-file --data-dir'
printf '%s' 'explicit-secret' > "$TMP/explicit-pwfile"
HDB_STUB_ARGV="$TMP/argv-explicit" \
HELIOSDB_PASSWORD_FILE="$TMP/does-not-exist-envfile" \
HELIOSDB_DATA_DIR="$TMP/data" \
HELIOSDB_TLS=off \
PATH="$TMP/bin:$PATH" \
  sh "$ENTRY" start --port 6544 --password-file "$TMP/explicit-pwfile" \
  >"$TMP/out-explicit" 2>"$TMP/err-explicit" \
  || fail "entrypoint failed although an explicit --password-file was given (missing inherited HELIOSDB_PASSWORD_FILE must not block it); stderr: $(cat "$TMP/err-explicit" 2>/dev/null)"
grep -qF "$TMP/explicit-pwfile" "$TMP/argv-explicit" \
  || fail "explicit --password-file path not forwarded"
if grep -qF 'does-not-exist-envfile' "$TMP/argv-explicit"; then
  fail "entrypoint forwarded the stale inherited env file instead of the explicit flag"
fi
if grep -qF 'explicit-secret' "$TMP/argv-explicit"; then
  fail "secret value appears on the forwarded command line"
fi
pass "explicit --password-file honoured despite a missing inherited HELIOSDB_PASSWORD_FILE"

echo
echo "RESULT: PASS — entrypoint never reintroduces password argv exposure"
