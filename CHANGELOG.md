# Changelog

All notable changes to HeliosDB Nano will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [3.9.2] - 2026-04-09

### Fixed
- MySQL wire protocol column type: bigint columns returned MYSQL_TYPE_NULL (type 6)
  because column type was inferred from the first row's value (NULL for auto-generated
  PK). Now scans all rows for first non-NULL value to determine correct type.
  This was the root cause of insert_id=0, WHERE ID=N returning 0 rows, and all
  content CRUD appearing to succeed but returning id=null.

## [3.9.1] - 2026-04-09

### Fixed
- KEY index regex matched inside column names (meta_key → corrupted DDL).
  Regex now requires comma anchor so only standalone KEY definitions match.
- Bigint equality: WHERE ID = 1 failed because Int4(1) literal didn't match
  Int8 PK in ART index. Added PK type coercion in get_row_by_pk_inner().
- Duplicate PK detection: insert_tuple_fast wrote data BEFORE checking
  constraints, silently creating duplicates. Now checks PK+UNIQUE first.
- check_unique_constraints() now covers pk_indexes (was only checking
  unique_indexes, missing PK violations entirely).
- ON DUPLICATE KEY handler: case-insensitive error detection for dup matching.
- 5 new WordPress-specific regression tests.

## [3.9.0] - 2026-04-08

### Fixed (WordPress zero-drop-in milestone)
- LAST_INSERT_ID: PK columns now auto-fill with row_id across ALL insert paths
  (transactional, fast, versioned, branch-aware). Missing PK in INSERT column list
  now generates NULL placeholder instead of erroring.
- DEFAULT CHARSET/COLLATE: translator now handles `DEFAULT CHARACTER SET utf8mb4`
  (with spaces) and `DEFAULT CHARSET=utf8mb4` (with equals) correctly
- ON DUPLICATE KEY UPDATE: implemented upsert via INSERT-then-UPDATE-on-conflict
  pattern in MySQL handler (planner lacks ON CONFLICT support, so handler detects
  duplicate error and falls back to UPDATE)
- SELECT VERSION(): MySQL handler now intercepts and returns MySQL-format
  "8.0.35-HeliosDB-Nano" instead of falling through to PG evaluator
- USE database: SQL-level `USE dbname` now accepted silently (was only handled
  at binary protocol COM_INIT_DB level)
- SHOW INDEX: fixed table name extraction to handle backtick-stripped and
  database-qualified names

## [3.8.3] - 2026-04-08

### Fixed
- SELECT alias.* in JOINs: added QualifiedWildcard handling in planner so
  `SELECT t.*, tt.* FROM wp_terms AS t JOIN wp_term_taxonomy AS tt ON ...`
  correctly expands to all columns of each aliased table (13/15 → 15/15)
- SHOW FULL COLUMNS: now returns all 9 MySQL fields including Collation
  (utf8mb4_unicode_ci), Privileges, and Comment. WordPress wpdb::get_col_charset()
  can now determine column charsets without falling back to bypass mode

## [3.8.2] - 2026-04-08

### Fixed
- SERIAL/BIGSERIAL columns now auto-fill with row_id when NULL on INSERT.
  This was the root cause of LAST_INSERT_ID() returning 0 — the column
  stayed NULL because only the storage-level row_id was generated, not the
  SQL-level column value. MAX(pk) now returns the correct ID.
- INNER JOIN cross-type hashing: Int4(1) and Int8(1) now hash identically
  and compare equal in JoinKey, fixing empty results on SERIAL↔BIGSERIAL joins.
- Prefix key indexes: nested-paren regex handles KEY meta_key(meta_key(191)).

## [3.8.1] - 2026-04-08

### Fixed
- LAST_INSERT_ID returns 0: query_last_serial_id used double-quoted identifiers
  that caused case-sensitive mismatch with unquoted table names
- INNER JOIN returns empty results: hash join key comparison failed across integer
  widths (Int4 vs Int8). JoinKey now uses cross-type numeric coercion for both
  Hash and PartialEq, so SERIAL(Int4) joins match BIGSERIAL(Int8)
- Prefix key indexes `KEY col(191)`: regex didn't handle nested parentheses.
  Fixed pattern to match `(col(191))` correctly
- Backtick identifiers: strip entirely instead of converting to double-quotes

## [3.8.0] - 2026-04-02

### Added
- **Built-in Backend-as-a-Service layer** — REST API, Auth, OAuth, Realtime, Storage
- REST API at `/rest/v1/{table}` with 19 PostgREST-compatible filter operators
  (eq, neq, gt, gte, lt, lte, like, ilike, is, in, cs, cd, ov, fts, not, or, and)
- Auth endpoints: `/auth/v1/signup`, `/auth/v1/token`, `/auth/v1/logout`,
  `/auth/v1/refresh`, `/auth/v1/user` with JWT sessions and Argon2id hashing
- OAuth2 support for Google and GitHub (`/auth/v1/authorize`, `/auth/v1/callback`)
  with PKCE, automatic user creation, and provider linking
- Realtime WebSocket at `/realtime/v1/websocket` with Phoenix-protocol
  channel subscriptions and INSERT/UPDATE/DELETE change notifications
- Row-Level Security enforcement on REST queries using JWT claims
- `ChangeNotifier` broadcasts DML events to WebSocket subscribers
- Auth persistence: `_auth_users` and `_auth_refresh_tokens` tables in DB
- MySQL wire protocol with WordPress compatibility layer
  (SQL translator, SHOW commands, AUTO_INCREMENT, ON DUPLICATE KEY, etc.)
- 14 MySQL date/time functions (DATE_FORMAT, DATE_ADD, UNIX_TIMESTAMP, etc.)
- MySQL `$10+` parameter substitution fix
- 9 convenience methods on `EmbeddedDatabase` (branches, explain, refresh MV)

### Fixed
- Transaction read-your-writes (INSERT visible in same-transaction SELECT)
- SQLAlchemy pg_catalog.version() compatibility
- Column names (column_0 → real names) and quoted strings in PG wire protocol
- CREATE TABLE IF NOT EXISTS errors when table exists
- LAST_INSERT_ID() tracking per MySQL connection
- Backslash-quote escaping for PHP serialize() compatibility

## [3.7.0] - 2026-03-21

### Added
- INSERT ... SELECT with full constraint, trigger, FK, and RLS support
- String concatenation `||` operator with NULL propagation and auto-cast
- `generate_series(start, stop, step)` and `unnest()` table functions
- Aggregate expressions: `SUM(a)+SUM(b)`, `CAST(AVG(...) AS INT)`, `CASE` on `COUNT`
- ORDER BY aggregate sorting (rewrite aggregate refs to column aliases)
- Named window references: `WINDOW w AS (...)` with inheritance
- Multiple ALTER TABLE operations in a single statement
- 456 hardening tests across 9 test suites (null semantics, type coercion, truncate, savepoints, aggregates, string/unicode, window functions, subqueries, set operations)
- 182 additional hardening tests (JOIN, CTE, JSONB, triggers, PL/pgSQL)

### Fixed
- Recursive CTE with LIMIT (fast-path bypass skipped CTE materialization)
- Recursive CTE with COUNT(*) (storage fast-path returned 0)
- SMALLINT CAST truncation (now errors on overflow instead of silent wrap)
- DECIMAL-to-FLOAT cast corruption (now errors on precision loss)
- LIMIT + OFFSET integer overflow (saturating arithmetic)
- NULL comparisons and arithmetic return NULL (SQL three-valued logic)
- AND/OR short-circuit with proper NULL handling
- MIN/MAX on empty set returns NULL
- COUNT(col) skips NULLs (fast path restricted to COUNT(*))
- CUME_DIST uses ORDER BY keys
- SUM OVER all-NULL partition returns NULL
- ORDER BY / GROUP BY ordinal positions (SQL-92)
- INT8 checked arithmetic (no panic on overflow)
- UTF-8 fast-path parser preserves multi-byte characters
- ART index cleared on TRUNCATE
- Savepoint data rollback via write set snapshot/restore
- UPDATE/DELETE in explicit transactions use branch-aware keys
- TRUNCATE respects active transactions (buffered in write set)
- INSERT rollback properly clears ART index entries
- WAL only logs committed changes (no phantom entries during transactions)

### Improved
- Zero clippy warnings (pedantic + nursery + cargo)
- All `eprintln!` in production code replaced with `tracing` macros
- All `unwrap()` in production code replaced with safe patterns or annotated
- Zero `todo!()` or `unimplemented!()` in production paths
- 1367 lib tests, all passing

## [3.6.0] - 2026-03-01

### Added
- Performance fast paths: `try_fast_insert()`, `try_fast_update()`, `try_fast_select()`
- Result cache: 128-entry LRU with DML/DDL invalidation
- Schema cache: in-memory HashMap, pre-warmed on connection
- ART index: zero-copy PK lookups
- RocksDB tuning: 14-bit bloom filter, 16KB blocks, prefix extractor
- 21/21 benchmarks won vs PostgreSQL 13
