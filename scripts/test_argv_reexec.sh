#!/bin/sh
#
# Thin gated wrapper around test_argv_reexec.py — see that file for the full
# contract (foreground/daemon /proc argv + identity + SQL proofs for the Linux
# re-exec, evidence bundle, exit codes). Reuses the sibling secret-argv harness
# primitives.
#
# GATED: a normal test pass only sees a visible skip. Run explicitly with:
#
#   HELIOSDB_PROC_REGRESSION=1 \
#   HELIOSDB_NANO_BIN=/abs/path/to/heliosdb-nano \
#   HELIOSDB_PROC_EVIDENCE_DIR=/abs/evidence/dir \
#     scripts/test_argv_reexec.sh
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
  echo "skip: gated real-process argv-reexec harness; set HELIOSDB_PROC_REGRESSION=1 to run" >&2
  exit 0
fi

exec python3 "$DIR/test_argv_reexec.py" "$@"
