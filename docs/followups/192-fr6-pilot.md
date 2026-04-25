# Task 192 — FR 6 pilot deployment

## Goal

Operations doc + bootstrap script for installing Nano + the
indexer + MCP endpoint into a hidden git-ignored directory inside
an existing git repo, mirroring the FR 1 pilot pattern that
already validated the cold/warm-index ergonomics.

## Acceptance

A user can run a single command from any git repo root:

```sh
curl -fsSL https://raw.githubusercontent.com/.../install-nano-pilot.sh | bash
```

and end up with:

* `.helios-nano/` directory in the repo (added to `.gitignore`).
* RocksDB data files inside.
* `.helios-nano/config.toml` with sane defaults.
* `.helios-nano/bin/heliosdb-nano` symlink to a downloaded
  release binary.
* git pre-commit hook calling `heliosdb-nano code-graph hook` so
  edits stay reflected.
* MCP server config snippet printed for paste-into Claude Code's
  `~/.config/claude/claude.json`.

Subsequent indexing of the repo's `src/` is a one-liner:

```sh
.helios-nano/bin/heliosdb-nano code-graph index --table src
```

## Design

### `scripts/install-nano-pilot.sh`

```sh
#!/bin/sh
set -eu

REPO_ROOT="$(git rev-parse --show-toplevel)"
HELIOS_DIR="$REPO_ROOT/.helios-nano"

mkdir -p "$HELIOS_DIR/bin"
mkdir -p "$HELIOS_DIR/data"

# Download latest release binary for the current platform
PLATFORM="$(uname -s)-$(uname -m)"
RELEASE_URL="https://github.com/Dimensigon/HDB-HeliosDB-Nano/releases/latest/download/heliosdb-nano-${PLATFORM}.tar.gz"
curl -fsSL "$RELEASE_URL" | tar xz -C "$HELIOS_DIR/bin"

# Default config
cat > "$HELIOS_DIR/config.toml" <<EOF
[storage]
data_dir = "$HELIOS_DIR/data"

[code_graph]
auto_reparse = true

[mcp]
bind = "127.0.0.1:0"
auth = "disabled"
EOF

# .gitignore
if ! grep -q "^.helios-nano/" "$REPO_ROOT/.gitignore" 2>/dev/null; then
  echo ".helios-nano/" >> "$REPO_ROOT/.gitignore"
fi

# pre-commit hook
HOOK="$REPO_ROOT/.git/hooks/pre-commit"
cat > "$HOOK" <<'EOF'
#!/bin/sh
.helios-nano/bin/heliosdb-nano code-graph hook \
    --data-dir .helios-nano/data --repo-root . --source-table src
EOF
chmod +x "$HOOK"

echo "Installed. Add the following to ~/.config/claude/claude.json:"
cat <<JSON
{
  "mcpServers": {
    "helios-nano-$(basename "$REPO_ROOT")": {
      "command": "$HELIOS_DIR/bin/heliosdb-nano",
      "args": ["mcp-server", "--db", "$HELIOS_DIR/data"]
    }
  }
}
JSON
```

### `docs/code_graph/pilot.md`

Step-by-step walkthrough:
1. Prereqs (git, curl, tar; Linux/macOS).
2. Run installer.
3. First index (cold): walks through expected timing on a
   medium repo (~5–30 s).
4. Warm re-index: < 1 s — content-hash gate kicks in.
5. Configuring Claude Code / Cursor / Codex.
6. Running the flagship query: `helios_graphrag_search` over the
   indexed codebase.
7. Adding ingest_pdf for project docs.
8. Tearing down: `rm -rf .helios-nano/` removes everything.

### `docs/code_graph/troubleshooting.md`

* `MCP connection refused` — port mismatch / mcp-endpoint feature
  missing in build.
* `lsp_definition returns nothing` — index not run / source_table
  wrong / language not registered.
* `body_vec NULL` — embedder not configured.
* Where to find logs (`HELIOS_LOG=debug`).

## Files to touch

* `scripts/install-nano-pilot.sh` — new.
* `docs/code_graph/pilot.md` — new.
* `docs/code_graph/troubleshooting.md` — new.
* `docs/code_graph/overview.md` — already exists; add a "see
  pilot.md" pointer.
* `README.md` — one-liner under "Pilots".

## Tests

* `tests/pilot_install_smoke.sh` — runs the installer in a tmp
  dir against a fake `git init` repo, asserts file layout +
  `.gitignore` entry. Skipped on non-Linux runners.

## Out of scope

- Windows installer (separate PR).
- Multi-repo setups / shared data dir across projects.
- Auto-update / version pinning.
