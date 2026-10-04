#!/bin/sh
#
# Thin gated wrapper around test_server_secret_argv.py — see that file for the
# full contract (real-process /proc argv checks, SELECT 1 credential proof,
# evidence bundle, and exit codes). Kept at a .sh path for existing callers.
#
# GATED: a normal test pass only sees a visible skip. Run explicitly with:
#
#   HELIOSDB_PROC_REGRESSION=1 \
#   HELIOSDB_NANO_BIN=/abs/path/to/heliosdb-nano \
#   HELIOSDB_PROC_EVIDENCE_DIR=/abs/evidence/dir \
#     scripts/test_server_secret_argv.sh
#
# Add HELIOSDB_PROC_MCP_HTTP=1 to require the MCP 401/200 HTTP gate (needs a
# binary built with the mcp-endpoint feature; otherwise the run is unavailable).
#
# Exit codes: 0 pass/skip, 1 assertion failure, 3 requested but unavailable.
set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"

if ! command -v python3 >/dev/null 2>&1; then
  if [ "${HELIOSDB_PROC_REGRESSION:-0}" = "1" ]; then
    echo "unavailable: python3 not found; cannot run the gated harness" >&2
    exit 3
  fi
  echo "skip: gated real-process harness; set HELIOSDB_PROC_REGRESSION=1 to run" >&2
  exit 0
fi

exec python3 "$DIR/test_server_secret_argv.py" "$@"
