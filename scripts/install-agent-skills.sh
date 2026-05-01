#!/usr/bin/env bash
# install-agent-skills.sh — Install HeliosDB-Nano agentic skills into ~/.claude/skills/
#
# Usage:
#   bash scripts/install-agent-skills.sh                  # copy (default)
#   bash scripts/install-agent-skills.sh --symlink        # symlink (live updates)
#   HELIOSDB_SKILLS_DEST=/some/dir bash scripts/install-agent-skills.sh
#
# Existing ~/.claude/skills/heliosdb-nano-* directories are backed up to *.bak.<unix-ts>
# before being overwritten in either mode.
set -euo pipefail

MODE="copy"
if [[ "${1:-}" == "--symlink" ]]; then
    MODE="symlink"
elif [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    sed -n '2,11p' "$0" | sed 's/^# //; s/^#$//'
    exit 0
fi

DEST="${HELIOSDB_SKILLS_DEST:-$HOME/.claude/skills}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$(cd "$SCRIPT_DIR/.." && pwd)/.claude/skills"

if [[ ! -d "$SRC" ]]; then
    echo "ERROR: source directory not found: $SRC" >&2
    echo "       Run this script from a checkout of HeliosDB-Nano." >&2
    exit 1
fi

mkdir -p "$DEST"

count=0
ts="$(date +%s)"
for skill in "$SRC"/heliosdb-nano-*/; do
    [[ -d "$skill" ]] || continue
    name="$(basename "$skill")"
    target="$DEST/$name"

    if [[ -e "$target" || -L "$target" ]]; then
        backup="$target.bak.$ts"
        echo "  backing up existing $target -> $backup"
        mv "$target" "$backup"
    fi

    if [[ "$MODE" == "symlink" ]]; then
        ln -s "$skill" "$target"
        echo "  symlinked $name"
    else
        cp -r "$skill" "$target"
        echo "  copied    $name"
    fi
    count=$((count + 1))
done

# Also publish the _index/ helpers (verb-map, feature-matrix) if present.
if [[ -d "$SRC/_index" ]]; then
    target="$DEST/heliosdb-nano-_index"
    if [[ -e "$target" || -L "$target" ]]; then
        mv "$target" "$target.bak.$ts"
    fi
    if [[ "$MODE" == "symlink" ]]; then
        ln -s "$SRC/_index" "$target"
    else
        cp -r "$SRC/_index" "$target"
    fi
    echo "  installed _index/ helpers"
fi

echo
echo "Installed $count HeliosDB-Nano skills (mode=$MODE) into:"
echo "  $DEST"
echo
echo "Claude Code will discover them automatically on the next session start."
echo "Codex CLI / generic agents: read AGENTS.md at the project root."
