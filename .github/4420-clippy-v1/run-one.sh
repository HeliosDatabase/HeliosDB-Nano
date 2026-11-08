#!/usr/bin/env bash
# Prepared gate runner; this file was not executed during preparation.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UNIT="nano-final-${1:?stage required}-$(date -u +%Y%m%dT%H%M%S)-$$"
exec flock /home/gpc/HDB/sprint/coordination/build.lock \
  systemd-run --user --scope --collect --unit="$UNIT" \
    -p MemoryMax=24G -p MemorySwapMax=0 \
    env NANO_FINAL_GATE_BOUNDED=1 python3 "$HERE/run_stage.py" "$@" --execute
