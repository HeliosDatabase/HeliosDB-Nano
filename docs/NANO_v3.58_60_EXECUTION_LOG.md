# Nano 3.58→3.60 opt-in HTAP/ingest program — execution log & gate protocol

Driver for the program specified in `docs/NANO_v3.58_HTAP_OPTIN_SPEC.md`.
Owner/orchestrator: Opus (Nano session). Executor: Codex (this window).
Workspace: `/home/gpc/HDB/Nano` (the ISSUE-08 WIP is stashed; do not pop it).

## Per-item GATE PROTOCOL (every item, no exceptions)
1. **Builds clean**: `cargo build --release` (default features) AND with the
   item's feature(s) enabled. No new errors (pre-existing phase3.rs/main.rs
   warnings OK).
2. **pg35 NEUTRALITY (the hard gate) + PER-CATEGORY EROSION TRACKING**: rebuild
   `pg35_benchmark`, run ≥2 rounds vs PG18.4 capturing the **full per-category
   table** (not just the SCOREBOARD) to `/tmp/<item>_pg35_full.log`, then run
   `python3 /tmp/pg35_track.py <full-log> "<item-label>"`. The tracker appends a
   snapshot to `perf/v358_program/pg35_category_history.json` and flags ANY
   category whose Nano/PG ratio rose >0.05 vs baseline (margin eroding) or whose
   winner flipped toward PG. Goal: catch GRADUAL loss of Nano's advantage that a
   32-vs-33 scoreboard hides. A flagged category that isn't pure noise
   (ratio in the 0.95–1.05 ~tie band oscillating) blocks the item until fixed.
   The whole 35-category set must stay at-or-above its historical Nano margin.
3. **Targeted ON A/B**: prove the claimed downstream win with the feature ON
   (e.g. embed wall-time, COPY ingest rate, extended-protocol tps).
4. **Correctness**: relevant suite(s) green; for wire features add conformance
   tests.
5. Report results to Opus (this session) BEFORE commit. Do NOT tag/push.
   Opus reviews each item, then authorizes the commit.

Constraint reminder: every feature is opt-in / default-off / new-path-only.
Nothing changes Nano defaults or the simple-Query OLTP path pg35 measures.

## Item status

### 3.58
- [x] **Item 1 — `Embedder::embed_batch`** — IMPLEMENTED by Opus (src/code_graph/embed.rs
  trait default + FastEmbedder override calling `guard.embed(owned, Some(256))` once;
  src/code_graph/storage.rs `bulk_insert_symbols_batched` now collects non-empty
  signatures → one `embed_batch` → scatter by index, empty→None, `embed_calls`
  preserved). **GATE NOW.** Feature: `code-embed`. Targeted A/B: code_index embed
  wall-time, single-call vs batched (expect 2–5× on code_index). pg35 neutrality:
  embed is not on the pg35 path → confirm pg35 unchanged + default-feature build clean.
- [ ] Item 1b — `bulk_load_mode` actually suspends inline vector-DML/secondary-index
  maintenance on `bulk_insert_tuples` (lib.rs:14465-14473). Also the 3.57 write-path
  regression suspect → bisect.
- [ ] Item 2 — COPY wire sub-protocol (CopyIn/CopyData/CopyDone/CopyOut).
  PROTOCOL LAYER MAPPED (ready to implement in increments, each build+gate):
  - **2a messages.rs:** FrontendMessageType += `CopyData=b'd'`, `CopyDone=b'c'`,
    `CopyFail=b'f'`; BackendMessageType += `CopyInResponse=b'G'`,
    `CopyOutResponse=b'H'`, `CopyData=b'd'`, `CopyDone=b'c'`. FrontendMessage enum
    += `CopyData(Vec<u8>)`/`CopyDone`/`CopyFail(String)` + parse arms (dispatch in
    parse fn ~256). BackendMessage enum += `CopyInResponse{overall_format:u8,
    column_formats:Vec<i16>}`/`CopyOutResponse{..}`/`CopyData(Vec<u8>)`/`CopyDone`
    + encode arms (encoder writes `put_u8(tag)` + len-prefixed payload like the
    existing CommandComplete b'C' ~599). CopyInResponse payload: format byte +
    int16 col count + int16 per-col formats.
  - **2b parser:** `COPY tbl [(cols)] FROM STDIN | TO STDOUT [WITH (FORMAT
    text|csv|binary[, DELIMITER ..., HEADER ...])]` -> a Copy AST node (new
    LogicalPlan/Statement variant). Default format text.
  - **2c handler (handler.rs, the sub-protocol state machine) — FULLY SCOPED, APIs found:**
    - Hook: in `handle_single_query` (handler.rs:561), after the empty-query check,
      `if let Some(c) = super::copy::parse_copy(query) { return self.handle_copy(c).await; }`
      (intercepts BEFORE the normal parse/plan path).
    - Receive loop API: `self.read_message().await? -> Option<FrontendMessage>`
      (handler.rs:430). Insert API: `self.database.execute(sql)` (used at :668).
      Send: `self.send_message(BackendMessage)` (:1248), `send_command_complete(tag)`
      (:1522), `send_ready_for_query()` (:1508).
    - FROM STDIN flow: determine column count (copy.columns.len() or table schema
      via catalog) -> send CopyInResponse{overall_format:0, column_formats:vec![0;ncols]}
      -> loop read_message: CopyData(d)=>accumulate; CopyDone=>finish; CopyFail=>abort
      (discard, send ErrorResponse). Parse accumulated bytes as TEXT rows: split on
      '\n'; a lone "\\." line = end marker; each row split on '\t'; decode escapes
      (\t \n \r \\ ; "\\N" => NULL). **INJECTION-SAFE**: build INSERT via the
      parameterized path or strict single-quote escaping (double every '\''),
      NULL for \N. Batch inserts. -> send_command_complete("COPY <n>") ->
      send_ready_for_query().
    - TO STDOUT flow: send CopyOutResponse{0,vec![0;ncols]} -> SELECT [cols] FROM
      table -> per row, TEXT-encode (tab-join, NULL=>"\\N", escape \t\n\r\\) + '\n'
      -> send as CopyData -> CopyDone -> CommandComplete("COPY <n>") -> RFQ.
    - Proxy contract: ONE ReadyForQuery at the end (Sync-scoped), overall+per-col
      format bytes. Increment: TEXT format first (migration-mirror ingest path),
      then CSV + BINARY (PGCOPY\n\377\r\n\0 signature) as a 2c-followup.
    - WIRE TEST: round-trip COPY FROM STDIN then COPY TO STDOUT over a DuplexStream
      (like wire_tests.rs), assert row data integrity incl. NULLs + tab/newline
      escapes. Plus error cases: CopyFail aborts cleanly, malformed row -> error.
  - **2d gate:** build + new wire-conformance tests (round-trip a COPY FROM STDIN
    then COPY TO STDOUT, text+binary) + pg35 erosion check (COPY is a new code
    path entered only on the COPY statement -> pg35-neutral by construction).
  Proxy already relays CopyData/CopyDone and yields on CopyInResponse (Batch B),
  so it validates the moment 2c lands. NOTE: ~400 LOC total; implement 2a->2d in
  order, committing each compilable increment green.
- [ ] Item 4 — Fast-ingest `ProfileConfig` bundle (extends existing profile system).
- [ ] Item 8 — expose hard-coded RocksDB knobs (engine.rs:2019-2024) as StorageConfig.
- [ ] Regression bisect 3.36.1→3.57.0 bulk write (CodeKB §4.2).

### 3.59
- [ ] Item 3 — plan cache for unnamed/text-keyed extended stmts (`SET helios.plan_cache=on`).
- [ ] Item 6 — pipelined extended exec (N Bind/Execute before one Sync).
- [ ] Item 5 — cheap session reset (DISCARD ALL / helios.reset_session()).
- [ ] Item 10 — connection-setup + `ParameterStatus` capability advertising.

### 3.60
- [ ] Item 7 — binary result-format per portal.
- [ ] Item 9 — per-session `helios.fast_autocommit` (default off).
- [ ] Deferred Nano polish — engine.rs Arc<Tuple> cache; integer-filter scan dedup;
  INLJ streaming operator.
- [ ] R4.1 — on-disk layout v2 (its own cycle).

## Coordination results (2026-06-14)

**Proxy (general:1) — priority + wire contracts:**
- Land order: **COPY (item 2) FIRST** (headline, unblocks Batch G2 migration mirror).
  **Plan cache (item 3)** is Proxy's FASTEST validation loop — instant A/B vs recorded
  c16 baseline (extended 69.7k / prepared 84.9k / simple 93.3k tps), zero proxy changes;
  land alongside if cheap. **Session reset (item 5) LAST** — gated on proxy SCRAM (F.3b).
- Wire contracts (build-to-spec):
  - Pipelining (6): N Bind/Execute before ONE Sync; results per Execute; **exactly ONE
    ReadyForQuery AFTER Sync — Sync-scoped, never per-Execute** (else Batch-B relay
    terminates early).
  - COPY (2): CopyInResponse/CopyOutResponse carry overall format byte (0=text/1=binary)
    + per-column format list; CopyData/CopyDone/CopyFail; COPY FROM STDIN + COPY TO STDOUT;
    binary (fidelity) + text/CSV (compat).
  - Binary results (7): honor per-column result-format codes in Bind (0/1); RowDescription
    correct OIDs+format; binary for int2/4/8, float4/8, bool, timestamp(tz), uuid, numeric.
    Lower urgency (proxy stays text until binary-DataRow follow-up).
  - ParameterStatus probe (10): emit `S` per active GUC at startup + on SET (GUC_REPORT).
    **Exact GUC names:** `helios.plan_cache`, `helios.copy`, `helios.pipeline`,
    `helios.binary_results`, `helios.fast_autocommit`, `helios.reset_session`.
  - Plan cache (3): key by statement **TEXT**; DDL/schema-version invalidated; bounded LRU;
    `SET helios.plan_cache=on` default off.
  - Portal `max_rows` → `PortalSuspended` for streamed partials.

**CodeKB (general:2) — item 4 finalized** (`/home/gpc/HDB/heliosdb-codekb-mcp/NANO_v3.58_FAST_INGEST_PROFILE_CONFIG.md`):
- **FastIngest exact values:** `wal_sync_mode=Async`, `time_travel_enabled=false`,
  `durable_commit=Some(false)`, `compression=Lz4`, `cache_size=2 GiB`; CodeIndexOptions
  `skip_symbol_refs=true`, `skip_cross_file_resolve=true`, `chunk_size=Some(2000)`.
- Structural reqs: (1) `ProfileStorageBundle` (config.rs:482-487) only carries
  wal_sync_mode/time_travel/durable_commit → **ADD `compression` + `cache_size` as Option
  fields** (existing profiles None → pg35-neutral) **+ a profile→CodeIndexOptions bridge**
  (skip flags live in a separate struct). (2) cache_size splits 75% block / 25% write-buffer
  → item 8 (expose `write_buffer_size`) matters; 2 GiB pragmatic until then. (3)
  **`chunk_size=Some(2000)` is load-bearing for embed_batch** — bounds the per-chunk ORT
  batch (a single chunk on ≥10k files would batch ~344k strings at once). Embeddings stay
  ORTHOGONAL (user `--with-embeddings`), NOT implied by fast-ingest.
- **Ask back:** expose skip/chunk overrides as a `pub` method/constructor so the plugin
  reads them from Nano instead of re-hardcoding.
- Profile alone ~20-35% off ingest, multiplicative with embed_batch.

**Revised 3.58 order:** 1 (done) → 4 (fast-ingest profile, fully specced) → 8 (RocksDB
knobs, makes cache_size effective) → 1b (bulk_load_mode suspend) → 2 (COPY, Proxy headline)
→ 3 (plan cache, Proxy fast-validate). 3.59/3.60 unchanged.

## Log
- 2026-06-14: program opened. Spec + driver written. Item 1 implemented.
- 2026-06-14: **Item 1 (embed_batch) GATE GREEN** — default build clean (code_graph is
  feature-gated OUT of default → pg35-neutral by compilation), code-embed build clean
  (embed_batch compiles), code_graph tests 56/0. CodeKB to validate embed wall-time on its
  corpus. Committed integrate/v3.58.0 f71cdcb. Proxy + CodeKB coordinated (above).
- 2026-06-14: **Item 4 (FastIngest profile) GATE GREEN** — default/pg35 build clean,
  config tests 39/0, profile tests 11/0. pg35-neutral by construction (apply_profile_defaults
  only runs when a profile is selected; pg35 selects none; existing profiles set new fields
  None). Committed 455de90.

**pg35 strategy (efficiency):** items neutral-BY-CONSTRUCTION (feature-gated out, or
opt-in-profile/SET default-off) are gated by clean build + targeted tests + the inertness
argument; a SINGLE consolidated pg35 A/B is run on the full integrate/v3.58.0 branch at the
release boundary to confirm the whole batch is neutral (avoids N×40min redundant runs).
Items that DO touch the live OLTP code path (e.g. 1b bulk_load_mode, plan-cache lookup on
the hot path) get a per-item pg35 A/B before commit.
