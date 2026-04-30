# HeliosDB-Nano pilot deployment

A single-command install of HeliosDB-Nano + the code-graph indexer
+ the MCP endpoint into a hidden git-ignored directory inside any
git repo. Validated end-to-end during the FR-1 pilot on Nano's own
`src/` tree (cold-index 128 s, warm re-index 12 ms).

## Prerequisites

- Linux or macOS (Windows pilot is a separate FR).
- A git working tree.
- Either: a Cargo toolchain (Rust 1.85 or newer) **or** a
  pre-built `heliosdb-nano` release binary on `$PATH`.

## Install

From any git repo root:

```sh
sh scripts/install-nano-pilot.sh
```

Or from a remote checkout (read the script first if you don't
trust the source):

```sh
curl -fsSL https://raw.githubusercontent.com/Dimensigon/HDB-HeliosDB-Nano/main/scripts/install-nano-pilot.sh | sh
```

After the installer runs you have:

| Path | Purpose |
|---|---|
| `.helios-nano/bin/heliosdb-nano` | The binary, built with `code-graph,graph-rag,mcp-endpoint`. |
| `.helios-nano/data/` | RocksDB data files. |
| `.helios-nano/config.toml` | Loopback-only defaults. |
| `.git/hooks/pre-commit` | Re-indexes touched files on every commit. |
| `.gitignore` | The `.helios-nano/` line, appended idempotently. |

## First index (cold)

```sh
HELIOS=.helios-nano
$HELIOS/bin/heliosdb-nano code-graph index \
    --data-dir $HELIOS/data \
    --table src
```

Timing on a medium repo (~10 k LOC): 5 – 30 s.

## Warm re-index

The git pre-commit hook runs the same indexer on the changed
file set, so subsequent edits cost ~tens of ms thanks to the
content-hash gate (`code_graph::storage::sha256_hex`).

You can trigger a warm pass manually:

```sh
git diff --cached --name-only --diff-filter=ACMR | \
  $HELIOS/bin/heliosdb-nano code-graph hook \
    --data-dir $HELIOS/data \
    --repo-root . \
    --source-table src
```

## Wire into your AI client

`heliosdb-nano` itself is a database engine — it has no `mcp-server`
CLI subcommand on purpose (the engine stays generic). The MCP
server runs as a tiny stdio / HTTP shim that consumes the engine
as a library: the
[`heliosdb-codekb-mcp`](https://github.com/dimensigon/heliosdb-codekb-mcp)
plugin binary.

Drop this snippet into `~/.config/claude/claude.json` (or the
equivalent for Cursor / Codex / Continue):

```json
{
  "mcpServers": {
    "helios-codekb-myrepo": {
      "command": "/abs/path/to/heliosdb-codekb-mcp",
      "args": ["serve", "--source", "/abs/path/to/repo-root"]
    }
  }
}
```

For Cursor / Continue / any non-stdio MCP client, add `--http
<addr>` to bind an HTTP/WebSocket/SSE server instead of stdio
and point the client at it.

The plugin's `init --source X --mode <co-located|global|hybrid>
--ingest` step is what populates the KB before `serve` opens it.
The full 27-tool catalogue (16 unified DB-backed + 11 LSP /
GraphRAG / code-graph extensions) shows up in the agent's tool
list immediately.

## Run the flagship query

```json
{
  "name": "helios_graphrag_search",
  "arguments": {
    "seed_text": "ProductQuantizer",
    "hops": 2,
    "limit": 30
  }
}
```

The agent gets back the seed code symbol plus its 2-hop subgraph
with hop_distance + node_kind + path metadata.

## Tear down

```sh
rm -rf .helios-nano/
git restore --source HEAD --staged --worktree .gitignore .git/hooks/pre-commit
```

(or, if the hook isn't tracked, just delete `.git/hooks/pre-commit`).

## See also

- [overview.md](overview.md) — architecture + feature flag map.
- [troubleshooting.md](troubleshooting.md) — common gotchas.
- [`docs/followups/192-fr6-pilot.md`](../followups/192-fr6-pilot.md) — design doc.
