---
name: heliosdb-nano-install
description: Install HeliosDB-Nano via crates.io or build from source. Lists every cargo feature flag (code-graph, code-embed, mcp-endpoint, fips, ha-full, etc.), shows feature-matrix recipes, initializes a data directory, and verifies the install. Use this when the user has not yet run heliosdb-nano on this machine, or when adding a feature that requires a custom build.
allowed-tools: Bash(cargo *), Bash(heliosdb-nano *), Bash(rustc *), Read
---

# Install & Build HeliosDB-Nano

## When to use
- The user has nothing installed yet.
- They need a feature that's not in the default build (MCP, code-graph, FIPS, HA tier 2+).
- They want to know what `heliosdb-nano --version` returns or which features were compiled in.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| install (default) | crates.io | `cargo install heliosdb-nano` |
| install (with features) | crates.io | `cargo install heliosdb-nano --features <list>` |
| install (FIPS) | crates.io | `cargo install heliosdb-nano --no-default-features --features fips,encryption,vector-search` |
| build from source | git | `git clone <repo> && cd Nano && cargo build --release [--features <list>]` |
| init data dir | CLI | `heliosdb-nano init [./heliosdb-data]` |
| version | CLI | `heliosdb-nano --version` |
| help | CLI | `heliosdb-nano --help`, `heliosdb-nano <subcmd> --help` |
| list features (live) | cargo | `cargo info heliosdb-nano --features` |
| deploy skills (cargo install) | shell | copy `…/registry/src/*/heliosdb-nano-*/.claude/skills/` → `~/.claude/skills/` + `~/.agents/skills/` |
| deploy skills (git checkout) | shell | `bash scripts/install-agent-skills.sh [--symlink]` |

## Cargo feature catalogue

| Feature | Default? | Adds | Why |
|---------|----------|------|-----|
| `encryption` | ✅ | TDE at-rest (AES-256-GCM) | Encrypted storage |
| `vector-search` | ✅ | HNSW indexing, PQ codebook | Similarity search |
| `ring-crypto` | ✅ | `ring` + BLAKE3 | Standard crypto provider |
| `ha-tier1` | ✅ | WAL streaming, automatic failover | Warm standby (active-passive) |
| `code-graph` | — | tree-sitter (rust/python/ts/go/md/sql), `_hdb_code_*` tables, LSP API | Index source code |
| `graph-rag` | — | `_hdb_graph_*` schema, seed+expand+rerank API; **implies `code-graph`** | RAG knowledge graph |
| `code-embed` | — | fastembed-rs (ONNX), local in-process embedder; **implies `code-graph`** | Embed without external HTTP service |
| `mcp-endpoint` | — | JSON-RPC 2.0 dispatcher, stdio/HTTP/WS, 16-tool catalog | MCP server for AI agents |
| `fips` | — (exclusive with `ring-crypto`) | AWS-LC FIPS #4816 + SHA-256 + PBKDF2 | FIPS 140-3 compliance |
| `ha-tier2` | — | Branch-based multi-primary, vector-clock conflict resolution; implies `ha-tier1` | Active-active replication |
| `ha-tier3` | — | Consistent hash ring, cross-shard routing, dynamic resharding | Horizontal sharding |
| `ha-dedup` | — | Content-addressed deduplication across nodes | Storage savings |
| `ha-ab-testing` | — | Branch-to-experiment routing | A/B test branches |
| `ha-branch-replication` | — | Selective branch sync to remote servers; implies `ha-tier2` | Per-branch replication topology |
| `ha-full` | — | Bundle: `ha-tier1 + ha-tier2 + ha-tier3 + ha-dedup + ha-ab-testing + ha-branch-replication` | All HA features |
| `compression` | — | MySQL wire-protocol packet compression (zlib via flate2) | Bandwidth on slow links |
| `server` | — | Server-mode async runtime knobs | Heavyweight server build |

**Live source of truth**: `cargo info heliosdb-nano --features` prints the exact set the published crate supports.

## Recipes

### Recipe 1: Default install (most users)
```bash
cargo install heliosdb-nano
heliosdb-nano --version          # → heliosdb-nano 3.22.2 (or higher)
heliosdb-nano init ./mydata      # creates ./mydata as a fresh data directory
heliosdb-nano repl --data-dir ./mydata
```

### Recipe 2: Install with code-graph + MCP (AI-coding workflow)
```bash
cargo install heliosdb-nano --features code-graph,mcp-endpoint
heliosdb-nano --help | grep -E 'code-graph|mcp'
```
Recipes for each surface live in `heliosdb-nano-code-graph` and `heliosdb-nano-mcp`.

### Recipe 3: Local in-process embedder (no external HTTP service)
```bash
cargo install heliosdb-nano --features code-embed,mcp-endpoint
# First run downloads the fastembed model into ./.fastembed_cache/
```

### Recipe 4: FIPS-compliant build (production / regulated environments)
```bash
cargo install heliosdb-nano --no-default-features \
  --features fips,encryption,vector-search,ha-tier1
```
**Note**: `fips` is exclusive with `ring-crypto`. The `--no-default-features` flag is required because the default set includes `ring-crypto`.

### Recipe 5: HA full bundle
```bash
cargo install heliosdb-nano --features ha-full
heliosdb-nano start --data-dir ./data --replication-role primary --sync-mode semi-sync
```

### Recipe 6: Build from source (development)
```bash
git clone https://github.com/HeliosDatabase/HeliosDB-Nano.git
cd HDB-HeliosDB-Nano
cargo build --release --features code-graph,mcp-endpoint
./target/release/heliosdb-nano --version
```

### Recipe 7: Verify what was compiled in
```bash
heliosdb-nano --help                              # shows subcommands present
heliosdb-nano code-graph --help 2>/dev/null \
  && echo "code-graph: ENABLED" \
  || echo "code-graph: NOT compiled in"
```

## Pitfalls
- **`fips` requires `--no-default-features`**. Mixing `fips` with the default set fails to compile because `ring-crypto` is in `default`.
- **Feature implications matter**. `graph-rag` and `code-embed` both imply `code-graph`. Add `mcp-endpoint` separately if you also want the MCP server — it is not implied.
- **First `code-embed` run is slow**. The fastembed model (~80–500 MB depending on model) downloads to `./.fastembed_cache/`. Cache the directory in CI.
- **`heliosdb-nano init` is optional**. `start` and `repl` create the data dir on demand if missing. `init` is for scripted bootstrap and pre-flight validation.
- **`heliosdb-nano` (binary) vs `heliosdb_nano` (library crate)**. The binary is hyphenated; the Rust library imported as `use heliosdb_nano::{…}` is underscored. Both come from the same crate.

## Deploy the agent-skill catalogue (Claude Code / OpenCode / Codex)

The 19 `heliosdb-nano-*` skills are plain [Anthropic `SKILL.md`](https://agentskills.io) files. There is **no `heliosdb-nano skills` subcommand** — the binary cannot deploy them. Two deployment paths:

**A — git checkout (official):** `bash scripts/install-agent-skills.sh [--symlink]` copies/symlinks into `~/.claude/skills/` with timestamped backups. Requires a checkout; the script is **excluded from the published crate** (`scripts/` is in Cargo.toml `exclude`).

**B — `cargo install` only (no checkout):** the skill files *do* ship inside the crate (`.claude/skills/` is not excluded), so they sit in cargo's registry cache. Copy them out:

```bash
# Newest cached crate's skills (or pin: …/heliosdb-nano-<version>/.claude/skills)
SRC=$(ls -d ~/.cargo/registry/src/*/heliosdb-nano-*/.claude/skills 2>/dev/null | sort -V | tail -1)
for DEST in ~/.claude/skills ~/.agents/skills; do
  mkdir -p "$DEST"
  cp -r "$SRC"/heliosdb-nano-* "$DEST"/
  cp -r "$SRC"/_index "$DEST"/heliosdb-nano-_index   # reference docs, not a skill
done
```

Per-agent global discovery dirs (same `SKILL.md`, just different roots):

| Agent | Reads | Invocation |
|-------|-------|------------|
| Claude Code | `~/.claude/skills/` | Auto-discovered next session start. |
| OpenCode | `~/.claude/skills/`, `~/.agents/skills/`, `~/.config/opencode/skills/` | On-demand via the `skill` tool (allow it if policy is `ask`/`deny`). |
| OpenAI Codex | `~/.agents/skills/` (personal), `.agents/skills/` (repo), `/etc/codex/skills/` (admin) | Implicit by `description`, or `/skills` / `$heliosdb-nano-…`; restart to pick up new skills. |

Deploying to both `~/.claude/skills/` and `~/.agents/skills/` covers all three (`.agents` is the cross-tool standard Codex + OpenCode share; Codex does **not** read `~/.claude/skills/`). The cache only exists after cargo extracts the crate — if GC'd, run `cargo fetch heliosdb-nano` or reinstall. Each `SKILL.md` carries the required `name` + `description` frontmatter, so all three accept them unchanged.

## See also
- `heliosdb-nano-connect` — open a connection to a running or in-memory database.
- `heliosdb-nano-server` — daemonize + auth + TLS + HA flags.
- `_index/feature-matrix.md` — feature → skill cross-reference.
- `Cargo.toml:189-299` — authoritative feature definitions.
