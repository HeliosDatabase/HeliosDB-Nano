#!/usr/bin/env bash
set -euo pipefail
root_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
scope="nano-full-native-list-$(date -u +%Y%m%dT%H%M%S)-$$.scope"
exec flock /home/gpc/HDB/sprint/coordination/build.lock \
  systemd-run --user --scope --collect --unit="$scope" \
  -p MemoryMax=24G -p MemorySwapMax=0 \
  env NANO_FULL_LIST_BOUNDED=24G-noswap NANO_FULL_LIST_SCOPE="$scope" \
  python3.11 "$root_dir/adjudicate.py" collect "$@" --execute-list-only
