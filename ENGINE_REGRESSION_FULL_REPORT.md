# Engine regression: `code_index` write-phase slowdown between heliosdb-nano v3.22.2 → v3.30.0

**Status:** confirmed on `/home/gpc/HDB/Nano` corpus, 2026-05-18.
**Severity:** ~338× slowdown on the write phase; total ingest 45 s → ~93 min.
**Where:** `heliosdb-nano` write path exercised by `EmbeddedDatabase::code_index(...)`. Not in plugin code — the plugin was identical for both runs (modulo the dep pin).
**Owner:** engine team.

## Numbers

Same corpus (`/home/gpc/HDB/Nano` — 694 code files, 18 952 symbols, 117 344 refs), same plugin binary harness, sandboxed XDG dirs, default WAL `Async` mode, no embeddings, no Docling:

| Engine    | parse   | **write**       | code-graph total | Notes |
|-----------|---------|-----------------|------------------|-------|
| **3.22.2** | 4,360 ms | **9,709 ms**    | **35.1 s**       | Matches README pilot claim (~26 s). |
| **3.30.0** | 4,126 ms | **3,279,362 ms**| **~55 min**      | Parse essentially unchanged. |

- **Write phase: 9.7 s → 3,279 s = ~338× slower.**
- **Parse phase: 4.4 s → 4.1 s = unchanged (good — means tree-sitter perf is fine).**
- Telemetry showed `workers=8 chunks=1` in both runs, so parallelism wasn't the variable.
- Confirmed *not* the plugin's new heading/linker code: the 3.22.2 run was on the original (pre-change) plugin source; the 3.30.0 run included the heading + bulk-linker changes (which is also why the 3.30.0 figure subsumes a 70 k-edge MENTIONS pass). Even subtracting the linker entirely (which finished in 89 min on its own), the 55-min write phase is the engine's own time.

## Repro

```bash
# Sandbox so this does not touch ~/.config or ~/.local/share
TMP_XDG=$(mktemp -d)
BIN=$(realpath /path/to/heliosdb-codekb-mcp/target/release/heliosdb-codekb-mcp)

# Pin the engine version you want to test:
cd /path/to/heliosdb-codekb-mcp
# Edit Cargo.toml: heliosdb-nano = { version = "=3.22.2", ... }   (or =3.30.0)
OPENSSL_NO_VENDOR=1 cargo update -p heliosdb-nano --precise 3.22.2
OPENSSL_NO_VENDOR=1 cargo build --release

# Index the engine's own source tree
XDG_DATA_HOME="$TMP_XDG/data" XDG_CONFIG_HOME="$TMP_XDG/config" \
  "$BIN" init --source /home/gpc/HDB/Nano --mode global --ingest

# Cleanup
rm -rf "$TMP_XDG"
```

The line to compare across versions is:

```
code_index ms : parse=<P> write=<W> workers=<N> chunks=<C>
```

Plugin source (no edits required for repro): `https://github.com/dimensigon/heliosdb-codekb-mcp`, branch `main`, commit `af4f38e` or later.

## Where the time goes (suspects)

The plugin's `code_index` call path drives:
- Per-file: parse → upsert into `_hdb_code_files`.
- Per-symbol: insert into `_hdb_code_symbols` (FK → `_hdb_code_files`).
- Per-ref: insert into `_hdb_code_symbol_refs` (FK → `_hdb_code_symbols`).

For 694 files / 18 952 symbols / 117 344 refs that is ~137 k writes, mostly into FK-bearing tables. The CHANGELOG window `3.22.3 … 3.30.0` includes two changes that landed inside this exact path:

### 3.30.0 — Quirk H fix (FK validation)
> `EmbeddedDatabase::check_referencing_rows_exist` did a full `storage.scan_table` of the referencing table for every parent row being deleted. … Now uses the existing PK / UNIQUE / FK ART index for the lookup when available — O(log N) per call. **The slow scan-and-merge fallback stays for the in-transaction path (`active_txn = Some(_)`)** so read-your-own-writes semantics from the v3.22.1 fix are preserved.

The plugin runs `code_index` inside an outer transaction (`TxnGuard::begin` in `src/ingest.rs:603-637`). The Quirk H fix explicitly **does not apply when `active_txn = Some(_)`** — meaning the plugin still hits the slow scan-and-merge fallback. If that fallback was made slower as a side-effect of the H fix (or moved into a per-write FK check that didn't exist before), that fits the symptom exactly.

### 3.30.0 — Quirk I fix (`INSERT … ON CONFLICT DO UPDATE`)
The plugin's `upsert_src`/`upsert_doc` use `ON CONFLICT(path) DO UPDATE`. The engine code-graph indexer likely uses `ON CONFLICT` for `_hdb_code_files` deduping too. Bench in 3.30.0 reports 1230 ops/sec for ON CONFLICT writes — at 137 k writes that's still ~110 s, not 3,279 s. But if the new path interacts badly with the in-transaction FK fallback, the multiplier compounds.

### Other landmarks in the window (lower probability)
- 3.23.0 — `JoinPredicatePushdownRule` (read path; shouldn't affect writes).
- 3.24.0 — `information_schema` completion (catalog reads).
- 3.27.0 — bind-value parser hardening (extended query, unlikely on the embedded `db.execute_params` path).
- 3.28.0–3.31.x — drizzle-kit compat (pg_catalog system views, type plumbing — read-side).

## Tracing recipe (Nano `docs/TRACING_GUIDE.md`)

The plugin pins `tracing` to stderr and the engine's tracing pipeline is wired through `tracing_subscriber::EnvFilter::from_default_env()`. To capture the write path at debug level (and `txn_*` at trace level) during a 3.30.0 run, prepend `RUST_LOG`:

```bash
RUST_LOG=heliosdb_nano=debug,heliosdb_nano::storage=trace,heliosdb_nano::sql::executor=trace \
  XDG_DATA_HOME="$TMP_XDG/data" XDG_CONFIG_HOME="$TMP_XDG/config" \
  "$BIN" init --source /home/gpc/HDB/Nano --mode global --ingest \
  2> /tmp/helios-write-trace.log
```

Filters that should be telling (from `docs/TRACING_GUIDE.md`):

```bash
# Where is the time per-statement?
grep 'phase=.execute.' /tmp/helios-write-trace.log | \
  awk -F'duration_us=' '{print $2}' | awk '{s+=$1; n+=1} END {print "n="n" avg_us="s/n}'

# Per-table scan time (storage_scan)
grep 'phase=.storage_scan.' /tmp/helios-write-trace.log | \
  grep '_hdb_code_' | head -40

# Transaction commit overhead
grep 'phase=.txn_commit.' /tmp/helios-write-trace.log | head -20

# Slow queries (>1 s threshold, auto-logged at WARN)
grep 'Slow query' /tmp/helios-write-trace.log | head -40
```

Expected shape if Quirk H's FK fallback is the cause: many `storage_scan` events against `_hdb_code_symbols` or `_hdb_code_files` during the symbol/ref insert phase, with growing `duration_us` as the tables grow.

If that's not it, the second drill-down is per-INSERT planning cost — look for high `phase=plan` `duration_us` ratios.

## Suggested next steps

1. **Pin-bisect inside `[3.22.3 … 3.30.0]`** to localise. Plugin Cargo.toml accepts `cargo update -p heliosdb-nano --precise <v>`; full versions in window: 3.22.3, 3.23.0, 3.23.1, 3.23.2, 3.24.0, 3.25.0, 3.26.0, 3.26.1, 3.27.0, 3.28.0, 3.29.0, 3.30.0. Roughly 11 builds (~7 min each cold + ~1-55 min per ingest) — git-bisect-style binary search lands in 4 builds.
2. **Run the tracing recipe** on whichever version first regresses; correlate the dominant `phase=` against the CHANGELOG.
3. **Targeted unit benchmark**: a `criterion` bench that does N inserts into a 2-column FK-bearing table inside one txn vs outside; if the in-txn version is ~300× slower on 3.30.0 but ~equal on 3.22.2, that's the smoking gun.
4. **Confirm the fix on the real plugin run**: re-ingest `~/HDB/Nano` after the engine patch and check that `code_index ms write=` returns to single-digit seconds.

## What is NOT in scope here

- The plugin's new bulk linker (`src/linker.rs`) — that's a separate change (89 min → expected ~1 min on a fixed engine, but not gated on this regression).
- The `helios_graphrag_search` quality rank — still hop-distance only; tracked separately.
- Plugin's API surface — no engine API mismatch; both 3.22.2 and 3.30.0 compiled cleanly.

## Repro artefacts

The verified worktree that produced the 3.22.2 numbers above lived at
`/home/gpc/HDB/heliosdb-codekb-mcp/.claude/worktrees/agent-afd83cc3bafe0b587`
during the 2026-05-18 session. It will be auto-removed when the session
ends. The repro commands above will reproduce the same numbers from a
fresh checkout.
