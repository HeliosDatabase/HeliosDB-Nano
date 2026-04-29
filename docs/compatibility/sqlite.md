# SQLite Compatibility

HeliosDB Nano accepts a deliberate subset of SQLite-flavoured SQL so that
existing `sqlite3`-driven applications can be retargeted with a one-line
import swap. The table below is the authoritative reference for what is
supported, where the rewriting happens, and what to expect when a feature
isn't covered.

## Feature matrix

| Feature | Status | Where it's handled | Notes |
|---|---|---|---|
| `?` positional placeholders | ✅ | `src/sql/sqlite_compat.rs::rewrite_question_placeholders` | Quote-, comment- and dollar-quote-aware rewrite to `$1, $2, …`. Mixing `?` with `$N` in the same statement is rejected. |
| `:name`, `@name`, `$name` placeholders | ✅ (via SDK) | `heliosdb_sqlite/main.py::_bind_parameters` | The SDK pre-binds named parameters by string substitution before the SQL reaches the engine. |
| `INSERT OR REPLACE INTO t (cols) VALUES …` | ✅ | `src/sql/sqlite_compat.rs::rewrite_insert_or_replace` | Rewritten to `INSERT … ON CONFLICT DO UPDATE SET col = EXCLUDED.col` for every named column. Falls back to a plain `INSERT` if no column list is given. |
| `INSERT OR IGNORE INTO …` | ✅ | `src/sql/sqlite_compat.rs::rewrite_insert_or_ignore` | Rewritten to `INSERT … ON CONFLICT DO NOTHING`. |
| `INTEGER PRIMARY KEY AUTOINCREMENT` | ✅ | `src/sql/sqlite_compat.rs::rewrite_autoincrement` | Mapped to `BIGSERIAL PRIMARY KEY`. |
| `DATETIME('now')` | ✅ | `src/sql/sqlite_compat.rs::rewrite_datetime_now` | Mapped to `CURRENT_TIMESTAMP`. Other `DATETIME(arg)` forms pass through unchanged. |
| `sqlite_master` introspection | ✅ | `src/sql/system_views.rs`, `src/sql/phase3/system_views.rs` | Returns one row per user table / materialised view, columns: `type, name, tbl_name, rootpage, sql`. The `sql` column is best-effort and intended for `WHERE name = ?` filtering, not faithful DDL. |
| `PRAGMA table_info(t)` | ✅ | `src/lib.rs::handle_pragma_query`, `src/protocol/postgres/handler.rs::pragma_table_info`, `src/repl/shell.rs` | Returns SQLite-shaped rows `(cid, name, type, notnull, dflt_value, pk)`. |
| `PRAGMA foreign_keys = …`, `journal_mode = …`, `synchronous = …`, `busy_timeout = …` | ✅ no-op | same as above | Parsed and acknowledged silently. Foreign keys are always on; journal mode is RocksDB-managed. |
| Other `PRAGMA name [= value]` | ✅ no-op | `parse_pragma` | Unknown PRAGMAs return an empty result instead of raising. |
| `executescript("a; b; c;")` | ✅ | PostgreSQL simple-query protocol + SDK split-on-`;` | Multi-statement strings work over the wire and via the SDK shim. |
| Multi-column `CREATE INDEX` (B-tree) | ⚠️ partial | `src/sql/planner.rs` | Accepted; only the **leading column** is indexed today. Vector indexes (`USING hnsw|ivf|…`) still reject multi-column with a clearer error. |
| `INTEGER` type affinity | ✅ | parser type mapping | Maps to `INT4` (or `BIGSERIAL` when paired with `PRIMARY KEY AUTOINCREMENT`). |
| `BLOB` / `BOOLEAN` / `REAL` / `TEXT` types | ✅ | parser type mapping | Map to `BYTEA` / `BOOL` / `FLOAT4` / `TEXT` respectively. |
| `\|\|` string concat, `IFNULL`, `COALESCE`, `LENGTH`, `IFNULL`, `LIKE` | ✅ | evaluator | Same semantics as PostgreSQL. |
| `cursor.lastrowid` | ✅ (v3.21+) | SDK auto-rewrite | Cursor.execute() detects INSERT statements, appends `RETURNING <pk>` (PK column cached per table via `PRAGMA table_info`), captures the returned value. Tables with TEXT PKs return `None`, matching sqlite3 semantics. |
| `STRFTIME(fmt, …)` / `JULIANDAY(d)` | ❌ engine; ⚠️ SDK rewrite optional | — | Not added to the engine — SQLite-specific names would be drift. Use the PG surface: `TO_CHAR(d, fmt)`, `EXTRACT(EPOCH FROM d) / 86400`, `DATE_TRUNC`, `DATE_PART`, `AGE`, `MAKE_DATE`, `MAKE_TIMESTAMP`. All shipped in v3.21. |
| User-defined Python functions, `register_adapter`/`register_converter` | ❌ | SDK raises `NotSupportedError` | Embedded mode runs in a separate process; in-process callbacks are not bridged. |
| Custom collations, loadable extensions | ❌ | SDK raises `NotSupportedError` | — |

## How rewriting works

`Parser::parse()` (in `src/sql/parser.rs`) runs SQLite-compat preprocessing
before any other rewrite, so downstream stages — including time-travel
rewriting, `STORAGE` clause stripping and the sqlparser front end —
operate on canonical PostgreSQL syntax. The PostgreSQL wire handler
short-circuits `PRAGMA` before the parser sees it; `EmbeddedDatabase`
short-circuits inside `query()` and `execute()`; the REPL formats
PRAGMA results with the SQLite-shaped schema directly.

## Embedded vs daemon mode

| | Embedded (`mode='embedded'`) | Daemon (`mode='daemon'`, `HELIOSDB_DSN=…`) |
|---|---|---|
| How it runs | One `heliosdb-nano repl` subprocess per `Connection` | Many `psycopg2` connections to one running `heliosdb-nano start` |
| Per-query latency | ~10–50 ms (subprocess RPC) | Sub-millisecond (PG wire) |
| Cross-connection consistency | ✅ as of v3.21 — every new process re-registers PK / UNIQUE / FK indexes and replays existing rows through `on_insert` on open. Cross-connection upserts and uniqueness checks behave correctly. | Single shared process — full consistency. |
| Setup | Zero — only the binary on `PATH` (or `HELIOSDB_BIN=`) | Run `heliosdb-nano start --port … --auth=trust` (or with passwords) |
| Recommended for | Dev, CI, single-process scripts | Production, multi-user, long-lived dashboards |

## Honest gaps

- **Persistent ART pages** — the v3.21 fix rebuilds indexes from data on
  open (O(rows + indexes) one-time cost). For multi-million-row data
  directories, that startup cost can be material. v3.22+ tracks moving
  to a RocksDB column family per index so opens are O(small).
- **`STRFTIME` / `JULIANDAY` engine functions** — intentionally not
  added. The PostgreSQL surface (`TO_CHAR`, `EXTRACT`, `DATE_TRUNC`,
  `DATE_PART`, `AGE`, `MAKE_DATE`, `MAKE_TIMESTAMP`, etc.) is the
  canonical home. SDKs can rewrite SQLite-specific names if desired.
- **Numeric `TO_CHAR(123.45, '9,999.00')`** — engine support is
  date/timestamp only today. Numeric formatting can be done at the
  client layer.

## See also

- `docs/guides/sqlite_drop_in_tutorial.md` — runnable end-to-end port.
- `tests/sqlite_compat_tests.rs` — every item in this matrix has at
  least one assertion against a live `EmbeddedDatabase`.
- `src/sql/sqlite_compat.rs` — pre-parser rewrites + 16 unit tests.
