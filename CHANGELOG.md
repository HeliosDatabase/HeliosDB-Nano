# Changelog

All notable changes to HeliosDB Nano will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed — UNIQUE was enforced only for the first table to use a column name; every spelling now enforces; ON CONFLICT never duplicates; FK targets validated at DDL

The Partner Portal saw "state-dependent" uniqueness: identical DDL rejected duplicates in one run
and accepted them in the next, a fresh table lost a constraint an older one had, and a table with
two UNIQUE columns behaved differently depending on which column was violated first. Root cause:
column-level UNIQUE indexes were registered in a process-global name map under the bare column
name, so the first table to declare `login UNIQUE` owned the name and every later table's index
registration failed silently — leaving those constraints enforced by nothing.

Now: constraint indexes are named per table; a registration that cannot enforce fails the DDL;
table-level `UNIQUE (col)`, `CREATE UNIQUE INDEX` and `ALTER TABLE … ADD CONSTRAINT UNIQUE` all
create the same enforced, durable constraint (existing duplicates are rejected with 23505 at
creation); quoted identifiers in table constraints resolve; `DROP CONSTRAINT` / `DROP INDEX`
remove exactly what they name and never an index another table or the primary key still owns.
`INSERT … ON CONFLICT (target) DO UPDATE` resolves the target against every unique constraint
(inline, table-level, composite, unique index; quoted or unordered), updates the existing row,
re-raises 23505 for a collision on a different constraint, and rejects targets without a unique
constraint with 42P10 — so Prisma upserts, including composite ones, no longer insert
duplicates. `REFERENCES t(c)` is validated when the table is created or altered (42P01 / 42703).

Two older index-maintenance defects were reachable behind it and are fixed too: the text-family
`ON CONFLICT … DO UPDATE` leg maintained indexes from the PROPOSED row's values instead of the
updated row's pre-image (a bystander row could lose its index entries, and the updated row
vanished from equality lookups on its unique column while a scan still found it), and the
autocommit `UPDATE … RETURNING` path — Prisma's update shape — performed no ART index
maintenance at all (after changing a unique column, `= new value` missed and `= old value` still
matched; re-inserting the vacated value was wrongly rejected). Index maintenance now visits every
index and reports the first error instead of abandoning the rest mid-loop; a refused INSERT that
never stored its row leaves no index entry behind, while a row that is already stored always keeps
its primary-key entry; NULLs are distinct under every UNIQUE spelling (single and composite) on
every insert, update and upsert path; an UPDATE is never reported as a duplicate of its own row
— including the plain `UPDATE t SET v = <a value no other row holds>`, which failed with 23505 on
every table declaring `UNIQUE (v)` at table level, because that spelling is recorded as two
constraints over one column and the per-statement duplicate check counted the row against its own
first pass (both executor families, with and without RETURNING);
and `ALTER TABLE … RENAME TO "Quoted"` resolves its target like CREATE TABLE does, keeps the
source schema (a different schema qualifier is rejected as in PostgreSQL), and carries the
table's constraint, identity and partition records with it.

Operators: an ART index snapshot written by an earlier release names indexes the old way, so the
first open after upgrading rebuilds them from rows (correct, one slower open); duplicates that an
unenforced constraint let in earlier surface as 23505 when the constraint is rebuilt.

### Fixed — a session transaction deadlocked against ITSELF when it wrote the same row twice

Over the wire (per-session transactions carry a lock manager; embedded transactions do not), a
transaction that updated a row and then updated it again — two plain UPDATEs, or Prisma's
create-then-update inside one `$transaction` — failed the second statement with
`Deadlock detected` (40P01) although no other transaction existed: lock compatibility ignored who
was asking, so a transaction's own write lock counted as a conflict, and the wait-for graph then
recorded a self-edge the cycle detector reported. The lock manager is now re-entrant for the
holder (re-request and uncontended read-to-write upgrade are granted, a re-entrant read never
downgrades a held write), and a requester is never recorded as waiting on itself. Behaviour
between different transactions is unchanged.

### Fixed — parameterized INSERT/UPDATE/DELETE … RETURNING escaped the session transaction (Prisma `$transaction`)

Over the extended protocol, a DML statement with bound parameters AND a RETURNING clause was
executed through a session-less entry point that used the global transaction slot, so inside an
explicit BEGIN it wrote straight to storage and autocommitted: ROLLBACK did not undo it, other
connections saw it, and `ROLLBACK TO SAVEPOINT` could not reach it. Every Prisma write is exactly
that shape, so interactive transactions were not atomic. The parameterized RETURNING path now
mirrors the simple-protocol one: it joins the session's transaction, reads its own uncommitted
writes, stays invisible to other sessions until COMMIT, and is discarded by ROLLBACK and by
ROLLBACK TO SAVEPOINT. Outside a transaction it still autocommits.

### Fixed — RETURNING names and types qualified columns like PostgreSQL (Prisma create/update)

`INSERT … RETURNING "public"."Account"."id"` — the statement Prisma emits for every write — came
back with the raw expression text as the field name and typed TEXT, with values serialised as
text; Prisma mapped rows by name and raised P2023 on the types. A qualified column reference in a
RETURNING list is now lowered to the target table's column, so it is named `id`, typed by the
catalog (int4/bool/timestamp…), and encoded accordingly, on both wire protocols and both executor
families. Unaliased expressions are named by the same rule the SELECT list uses (a function call
by its name). The client-side rename/type shim the Partner Portal added is no longer needed.

### Added — `pg_advisory_lock` family (session and transaction scope)

`pg_advisory_lock`, `pg_try_advisory_lock`, `pg_advisory_unlock`, `pg_advisory_unlock_all`,
`pg_advisory_xact_lock`, `pg_try_advisory_xact_lock` in the `(bigint)` and `(int, int)` forms —
what `prisma migrate` needs (`pg_advisory_lock(72707369)`). Locks are process-global, owned by the
connection's session, re-entrant, released at COMMIT/ROLLBACK for the transaction scope, on
disconnect for everything, and by `DISCARD ALL` for the session scope. A blocking acquire honours
`statement_timeout` and holds no engine lock while waiting. New `pg_advisory_locks` system view and
`[locks] advisory_max_per_session` setting. The `_shared` variants are not provided (they fail with
42883 rather than being served as exclusive locks).

### Security — `mcp-endpoint` builds: the HTTP listener never started, and the duplicate `/mcp` was unauthenticated

Since v4.27.0 (which mounted the BaaS router on the HTTP listener) every binary built with
`--features mcp-endpoint` panicked at startup on `Overlapping method route: POST /mcp` and the
HTTP port never came up: `ApiServer` mounted its own copy of the MCP routes without bearer
auth or the bind-safety check, next to the authenticated mount from `main.rs`. The
unauthenticated copy is removed; the token-checked, bind-safety-checked mount is the only one.
A composition test now builds the listener router exactly as the binary does and asserts
401 without the bearer / 200 with it. Default builds (crates.io, PyPI wheel, the Docker image)
do not include the feature and were not affected.

### Fixed — MCP `tools/list` now always includes `inputSchema`

Real MCP clients (Claude Code, Codex CLI, MCP Inspector) received tools without a schema
and could not build typed calls: `inputSchema` was only sent with a non-standard
`params.verbose = true`. It is now unconditional, as the MCP `Tool` schema requires;
`verbose` only adds the non-spec extras `category` and `requiresDatabase`.

### Security — dependency advisories (lockfile only)

`h2` 0.4.13 → 0.4.19 closes RUSTSEC-2026-0258 (unbounded empty DATA frames) on the HTTP
listener that serves the BaaS/REST/MCP routes on `--http-port`; `chacha20` 0.10.1 → 0.10.2
replaces a yanked release reachable only from the benchmark harness; `lru` 0.12 → 0.18 closes
RUSTSEC-2026-0253 (`LruCache::pop()` panic safety — a method this codebase never calls). Still
open and tracked separately: `h2` 0.3 under `oauth2` 4.x (client side of the OAuth flow; no
patched 0.3.x exists, remedy is the oauth2 5.x major).

### Fixed — `FROM generate_series(1, n) AS g` could not be referenced as `g` (PGConf.Brasil #10)

`SELECT g FROM generate_series(1, 3) AS g`, `SELECT g.g FROM generate_series(1, 3) g` and
`SELECT u FROM unnest(ARRAY[1,2,3]) AS u` all failed with `Column 'g' not found in schema`,
reproduced over the PostgreSQL wire on v4.30.0. PostgreSQL names the single output column of a
scalar-returning table function after the table alias when no column list is given; Nano used
the alias only as the source-table name and kept `generate_series` / `unnest` as the column name.
Both places that derive that schema — the logical plan (which `SELECT *` expands from) and the
executor (which the rows carry) — now use the same precedence: explicit column list (`g(i)`),
then table alias (`AS g`), then the function name. The alias-less form and `g(i)` are unchanged
and pinned by tests. Nine new tests in `tests/generate_series_alias_tests.rs`, seven of which
failed on the unfixed tree.

Not in this change: `generate_series` in the SELECT list (`SELECT generate_series(1, 3)`) still
reports `Unknown scalar function` — set-returning functions in the projection are a separate
feature (sprinter item filed).

## [4.30.0] - 2026-09-04

Seven fixes from the PGConf.Brasil 2026 lightning-demo capture, each verified still present on
the prior release by direct repro and each guarded by a test that failed before the fix.

### Fixed — vector KNN silently returned non-KNN order (worst-case for a vector store)

`… ORDER BY embedding <=> <operand> LIMIT n` returned rows in arbitrary (id) order, with NO
error, whenever the ORDER BY key could not be evaluated — an unmaterialised scalar subquery, a
vector literal in an unrecognised format, a dimension mismatch, a NULL parameter. The top-k
operator swallowed the evaluation error and sorted every row on a NULL key. It now surfaces the
error, exactly as the non-LIMIT sort path and the SELECT list already did.

### Fixed — vector text format: `{…}` rejected, `[…]` printed as `{…}`

The PostgreSQL wire printed vectors as `{1,2,3}` but every SQL-side parser accepted only
`[1,2,3]`, so a value read from a `SELECT` could not be pasted into a `WHERE`; and a `{…}` operand
was silently mis-ordered rather than rejected. There is now one shared vector-text parser that
accepts `[…]`, `{…}` and bare `1,2,3`; the wire prints pgvector's `[…]`; and every distance
operator rejects a non-vector operand identically.

### Fixed — multi-row `INSERT … VALUES` with vectors was O(n²) (~60 s per 500-row statement)

The `DECIMAL`→`NUMERIC` statement preprocessor called `to_uppercase()` on the entire remaining
statement at every character position — quadratic, and paid by every statement before parsing. A
1.45 MB multi-row vector INSERT took ~60 s. It is now a single linear, quote-aware scan with a
fast exit when the statement contains no `DECIMAL` at all.

### Fixed — `CREATE DATABASE BRANCH x FROM main` required `AS OF`

`AS OF` is now optional and defaults to `AS OF NOW`, matching the documented grammar. A present
but empty `AS OF` is still an error.

### Fixed — `AS OF TIMESTAMP` diagnostics and parsing

A timestamp with no snapshot at or before it now reports the interpreted instant and the
available snapshot range instead of a bare "No snapshot found"; fractional seconds and explicit
UTC offsets now parse.

### Fixed — unaliased aggregate column name

`SELECT count(*)` now names the column `count`, as PostgreSQL does, rather than `count(...)`. - 2026-09-02

### Fixed — **`INSERT … SELECT` now participates in the enclosing transaction** (#100)

Through 4.28.0, `BEGIN; INSERT INTO t SELECT …; ROLLBACK;` **left the rows** — on psql, MySQL, the
REPL, the embedded API and every extended-protocol driver. Both executor arms wrote each row
straight to storage with a call that takes no transaction, while holding a live transaction and
using it for the foreign-key probe a few lines earlier. Found by the ACID audit; never reported.

Inside a real transaction the rows are now staged through the same transactional path a
multi-row `INSERT … VALUES` uses: written at COMMIT, removed by ROLLBACK, undone in the ART and
HNSW indexes, with the row-id counter staged so a restart cannot hand out already-used ids. A
failure on row N inside a transaction now removes rows 1..N-1. A chained
`INSERT INTO stage SELECT …; INSERT INTO final SELECT … FROM stage` in one transaction sees the
staged rows.

**Scope, stated exactly.** Autocommit is unchanged: it runs through an implicit transaction that
carries no logical WAL, so staging under it would silently drop replication ops for HA standbys.
Consequently a mid-statement failure under *autocommit* still leaves the earlier rows — a
pre-existing, secondary audit finding, now filed. `CREATE TABLE … AS` population also stays on the
engine path so its compensating drop remains correct; DDL is non-transactional on this engine. A
tenant context or a branch falls back to the engine path, as the multi-row fast path already does.

New knob `[performance] insert_select_txn_batch_rows` (default 1000; `0` = one chunk) bounds the
transient per-chunk buffer.

**Breaking** only for anyone who relied on `INSERT … SELECT` rows surviving a `ROLLBACK`. That
was never a feature.

## [4.28.0] - 2026-09-02

### Changed — **BREAKING**: a NULL into a plain `INT PRIMARY KEY` is now rejected, as in PostgreSQL

Through 4.27.0, `INSERT INTO t (id, v) VALUES (NULL, 'x')` — and `INSERT INTO t (v) VALUES ('x')`
— on `t (id INT PRIMARY KEY, v TEXT)` **succeeded** and silently invented a primary key from the
row id. PostgreSQL raises `null value in column "id" violates not-null constraint` for both. Nano
now does too, on every insert shape and both executor families.

**`SERIAL` / `BIGSERIAL` / `IDENTITY` primary keys are unaffected** and still auto-fill when omitted
or explicitly NULL. The planner already marks those columns nullable precisely so the storage
layer can fill them; the defect was a `primary_key` exemption in the NOT NULL checks that could
only ever fire for a plain non-nullable key, where it was wrong.

**If you relied on the old behaviour**, declare the column `SERIAL` (or `GENERATED … AS IDENTITY`)
— that is the shape that means "generate it for me".

Also in this release: two HNSW backfill tests asserted exact nearest-neighbour *rank* on an
approximate, randomised index — one between two vectors ~0.006 apart, the other an exact-copy
probe at `k=1` over 3-dimensional synthetic vectors — and each flaked a release gate while
passing against the identical binary elsewhere in the same run. Both now assert what their names
claim, that the backfilled row is *reachable*, which still fails if backfill breaks.

## [4.27.0] - 2026-09-01

### Added — the built-in BaaS HTTP layer is now actually served (**behaviour change on port 8080**)

The README's opening line advertises a "built-in BaaS layer (Auth, REST API, Realtime)" and
prints a working `curl -X POST …/auth/v1/signup`. On every released binary those endpoints
returned **404**: the `start` command's HTTP listener served `/` and `/health` and nothing else,
and the REST / Auth / Realtime / Swagger router existed only as a library API that nothing
mounted. It is mounted now. `/version`, `/docs`, `/openapi.json`, `/rest/v1/*`, `/auth/v1/*` and
`/realtime/v1/websocket` respond; the README's signup example returns a real session.

**Review your exposure before upgrading.** Port 8080 (`--http-port`) previously exposed
effectively only a health check. It now exposes a REST API, authentication endpoints, a realtime
websocket and Swagger UI.

Four defects had to be fixed to deliver this, each sufficient on its own to keep it dead: the
router was never mounted; the auth bridge was never constructed (every `/auth/v1/*` call would
have returned 503); its schema bootstrap was never called (the first signup would have failed on
a missing `_auth_users`); and **four hardcoded fallback JWT secrets** sat behind the path.

### Security — no shipped JWT signing key

All four hardcoded fallback secrets are gone. The signing key comes from `[api] jwt_secret`; when
unset the server generates a fresh 256-bit key per start and **warns that tokens will not survive
a restart**. Set it before relying on sessions. The previous generator produced 128 bits from a
hash seed written out twice to look like 256 — inert while nothing signed tokens, unacceptable
now that something does.

### Fixed — `/health` kept its JSON shape

Mounting the router would have silently changed `/health` from `{"status":"ok"}` to the plain
string `OK`, breaking any monitor parsing it. Caught by the gate; both handlers now return the
JSON object.

## [4.26.0] - 2026-09-01

### Security — **`--auth md5` authenticated everyone; `--auth scram-sha-256` authenticated no one** (GH#19, GH#20)

Affects 4.23.0 through 4.25.0. **If you ran `--auth md5` on a non-loopback interface, treat the
database as having been open for the lifetime of that deployment.**

`--auth md5` accepted a client with the correct password, a wrong password, or **no password at
all**. `AuthMethod::Md5` had no arm in the wire handler's auth dispatch and fell into a catch-all
that set `authenticated = true` without ever sending a challenge — which is why an empty
`PGPASSWORD` connected without even a prompt. The startup banner printed `Authentication: MD5`,
confirming a control that did not exist. md5 is now genuinely implemented (salted challenge,
constant-time compare, uniform work on the unknown-user path), and the catch-all is **deleted**:
the match is exhaustive, so a future auth method is a compile error rather than another silent
accept-everyone.

`--auth scram-sha-256` rejected every password including the correct one. The handler advertised
a fresh random salt per connection instead of the salt the stored key was derived from, so no
proof could ever verify; it also never sent `AuthenticationOk` after `SASLFinal`, and rebuilt
`client-first-message-bare` rather than using the client's bytes. All three fixed. `scram-sha-256`
is the mode to use.

### Fixed — unknown config sections are no longer silent

`[auth]` in `config.toml` was parsed, discarded and never mentioned — the real section is
`[authentication]`. Unknown top-level sections now warn and suggest the intended name.

## [4.25.0] - 2026-09-01

### Fixed — table-level composite `UNIQUE (a, b)` was enforced by nothing (#107)

Accepted by the parser, persisted, reported by the catalog views — and enforced on no write path.
A duplicate pair simply succeeded. Only the index-creation call was missing; everything below it
was already composite-capable. The index is now created at `CREATE TABLE` and re-registered and
backfilled at open, so it survives a restart.

### Fixed — `ALTER TABLE … DROP CONSTRAINT` never stopped a UNIQUE constraint enforcing

The executor removed the constraint record but never dropped the index the write path actually
probes, so a dropped UNIQUE went on rejecting rows forever.

### Fixed — the integration-test CI added in 4.23.0 was red on main

Two bugs in the workflow itself: coloured cargo output defeated the suite-name parser, and the
empty-suite rule fired on the ~49 feature-gated targets that are empty by design.

## [4.24.0] - 2026-08-31

### Fixed — `INSERT … SELECT` over the PostgreSQL extended protocol wrote to the wrong columns (#101)

On the executor family used by psycopg3 (server-side bind), JDBC, sqlx, Drizzle, node-postgres and
every REST write, `INSERT INTO t (b, a) SELECT x, y FROM s` stored `x` in the **first** column,
ignoring the column list. With type-compatible columns there was no error — the values were
simply swapped. Silent data corruption.

### Fixed — the same path enforced almost no constraints (#102)

No NOT NULL, no FOREIGN KEY, no UNIQUE, no DEFAULT fill. `INSERT INTO child SELECT …` created
orphan rows past a foreign key. Both executor families now share one row-assembly and validation
gate. An unknown column name in an INSERT column list now errors instead of being silently dropped
and shifting every later value.

### Fixed — `BEFORE INSERT … FOR EACH ROW` rewrites did not apply to `INSERT … SELECT` (#84)

Known remaining gap, pinned by a test: `INSERT … SELECT` rows are not rolled back by a surrounding
`ROLLBACK` (#100).


## [4.23.0] - 2026-08-31

Ten backlog items, most of them found by an ACID audit rather than reported. Several are
silent wrong answers on the wire; two are transaction-boundary bugs.

### Fixed — SQLSTATE could depend on the CONTENT of your data

`sqlstate_for_error` classified constraint violations by substring, and the unique-violation
arm was tested BEFORE the foreign-key arm. Because the foreign-key message interpolates the
offending row's values, `INSERT INTO child VALUES (1, 'unique')` against a missing parent
reported **23505 unique_violation instead of 23503 foreign_key_violation** — the error code
depended on what the user's data happened to contain. Both arms now anchor on structural
phrases, foreign key first.

### Fixed — ordinary table names misclassified by the SQLSTATE classifier

Three bare-substring arms (added in 4.20.0) mapped `Table 'roles' does not exist` to 42704
instead of 42P01, `Table 'functions'` to 42883, `Table 'columns'` to 42703, and — because
the role arms preceded the column arm — `Column 'role' not found` to 42704. Every arm now
anchors on the shape its emitters actually produce.

The anchors are backed by a full emitter audit, which is the part that made them correct:
`role` has 9 emitters, all double-quoted, so excluding table/relation messages is safe;
`function` has 8, of which **3 are unquoted** (`Unknown scalar function: …`), so its anchor
needs that alternative and must NOT exclude "table" — `Unknown table function` contains it.

Also: `Column 'c' already exists` now maps to 42701 (was 42P07, duplicate *table*), and
`Function 'f' already exists` to 42723 (was XX000).

### Fixed — `END;` did not commit, `ROLLBACK WORK;` did not roll back

Four independent copies of the transaction-control classifier all prefix-matched
`BEGIN`/`START TRANSACTION` but used exact matching for `COMMIT`/`ROLLBACK`. sqlparser maps
`END` to `Statement::Commit`, and `COMMIT WORK` / `ROLLBACK WORK` / `ROLLBACK TRANSACTION`
are ordinary PostgreSQL spellings — all reached the wrong executor. Replaced by ONE
classifier accepting `END`, `ABORT`, `COMMIT`/`ROLLBACK [WORK|TRANSACTION] [AND [NO] CHAIN]`
and `ROLLBACK TO [SAVEPOINT] n`.

Two hazards the collapse exposed and closed: `ROLLBACK TO SAVEPOINT` is a distinct
non-boundary variant (treating it as a plain ROLLBACK silently converts a partial rollback
into a full one), and `AND CHAIN` genuinely chains (accepting the spelling without
re-opening would leave every following statement autocommitting).

### Fixed — an unknown column in an INSERT column list was silently dropped

`INSERT INTO t (no_such_col) VALUES (1)` dropped the unknown name, shifting every later
value, and then failed with "More values than columns specified" — a message naming neither
the column nor the problem, classified as XX000. All three sites (text VALUES, text SELECT,
params SELECT) now share one resolver that errors with the column name and 42703.

### Changed — `SERIALIZABLE` now tells you what it actually is

There is no read-set tracking in the engine, so `SERIALIZABLE` and `REPEATABLE READ` are
byte-identical and both are snapshot isolation: **write skew is not prevented**. Requesting
it now emits a PostgreSQL WARNING on the wire naming the anomaly, and the new
`[storage] serializable_policy = "warn" | "error"` can refuse it outright for deployments
that need the guarantee or nothing. There is deliberately no silent-accept setting. Real SSI
remains unimplemented and is not claimed anywhere.

### Changed — DDL is documented as non-transactional, and the test now says so

`BEGIN; DROP TABLE t; ROLLBACK;` destroys the table permanently. The only guarding test
asserted `rows.is_empty() || rows.len() == 1` — a tautology that could never fail. Replaced
with a test that pins the real behaviour, and documented in the README and the transactions
skill. Making DDL transactional is tracked separately.

### Fixed — DROP TABLE leaked vector indexes and index definitions

`Catalog::drop_table` dropped ART indexes only, leaving vector-index registrations and
`meta:index:` definitions behind. Orphaned definitions do worse than warn at every open: a
later `CREATE TABLE` with the same name **inherits them**, which for a vector index means
silently wrong kNN results. Teardown now runs from the one catalog funnel, so WAL replay and
the partition cascade get it too.

### Fixed — RENAME TABLE stranded trigger records

Trigger definitions and row-mutation recipes were not re-keyed, so triggers stopped firing
and the records were orphaned under a name that no longer existed. (The DROP half was
already handled.)

### Fixed — one open transaction anywhere slowed down every other session

`COPY` demoted to a ~10× slower fallback, and — previously unfiled — plan normalization was
disabled for **all** sessions' reads, whenever any unrelated session held an open
transaction, because the gate consulted a process-wide counter. Both now consult the
caller's own transaction state. Measured effect on the vs-PostgreSQL benchmark: indexed
point-reads improved from 14,606 to 16,063 TPS (simple) and 9,729 to 11,350 (extended).

### Added — `#>` and `#>>` JSON path operators

Implemented, delegating to the existing `jsonb_extract_path` traversal so the function and
operator forms cannot diverge. (In 4.22.0 `#>` was corrected from silently computing a
vector inner product to erroring.) The traversal also gained the array-index-as-text case
(`'{items,0}'`) the old loop returned NULL for.

### Internal — the last duplicate of the PK-literal coercion rule is gone

`scan.rs`'s private copy is removed; all six call sites now share one function. A drift
between the read probe and the write probe is exactly what caused the 4.22.0 write-loss bug.
Pinned by a ~45-row coercion matrix and a bincode discriminant test asserting every
`BinaryOperator` variant's on-disk index equals its declaration position.

### Added — CI finally runs the integration suite

`.github/workflows/tests.yml`: a blocking 16-target smoke on every PR, the full suite sharded
six ways nightly and on release tags, and a compile-only job for the `internal-tests` feature
(15 files that silently stopped compiling for months). Every tier fails on an empty suite —
`ok. 0 passed` is treated as failure, not success.

This was never a cost question: the repository is public, so runners are free. It was a
physical one — `cargo test --tests` builds 272 targets totalling ~67 GiB of debug binaries
against a runner's ~14 GB disk. `CARGO_PROFILE_TEST_DEBUG=0` (CI env only; ~74% of each
binary is DWARF) plus sharding makes it fit.

**Honest scope:** this would NOT have caught the defects found in the 4.20–4.22 campaign —
their regression suites shipped WITH the fixes, and no CI catches a defect nobody wrote a
test for. What it buys is that the ~5,200 tests which now exist can no longer rot unobserved.


## [4.22.0] - 2026-08-31

Four externally-reported issues, filed against 4.21.0 by a user evaluating Nano as a vector
store. One of them is a silent write-loss bug that has shipped for many releases.

### Fixed — **`UPDATE ... WHERE <pk> = '<literal>'` could report `UPDATE 0` and change nothing** (#15)

`UPDATE mem_test SET payload='...' WHERE id='<uuid>'` returned `UPDATE 0` and modified nothing,
while `SELECT` with the identical predicate returned the row. Reproduced on a live server.

The literal-UPDATE fast path bails when the SET clause contains a comma — so a two-key JSON value
diverts the statement into the planner arm, which built the primary-key index probe from the
parser's `Value::String` with **no coercion to the column's declared type**. A `UUID` encodes into
the index as 16 raw bytes and a `String` as 36 UTF-8 bytes, so the probe could never match; the
miss returned "no such row" with no fallback, and the predicate was never evaluated. `SELECT`
survived only because the read path already had the coercion.

Fixed as a class, not an instance:

- The coercion rule now covers **every** `DataType` whose index encoding differs from its string
  form — UUID, DATE, TIME, TIMESTAMP, TIMESTAMPTZ, BYTEA, BOOLEAN, FLOAT4/8, NUMERIC, INTERVAL,
  JSON/JSONB, ARRAY, VECTOR — derived by reading the index encoder and matched exhaustively with
  no wildcard arm, so a future type is a compile error rather than a silent miss.
- **`DELETE ... WHERE <pk> = ...` had the identical defect** and is fixed by the same change.
- A literal that cannot be represented in the column's type now **declines the index and scans**
  instead of asserting the row is absent. A correctly-coerced probe that misses still means
  absent, so `WHERE id = <nonexistent>` remains a point lookup and does not become a table scan.
- **The read fast path had the mirror defect**: `SELECT ... WHERE ts_pk = '2024-01-15 10:30:00'`
  returned zero rows on a row that exists, because its own copy of the coercion accepted only
  RFC 3339. Fixing only the write side would have inverted the divergence.
- Four copies of this coercion rule existed; there are now two.

### Fixed — `CREATE INDEX` without a name (#16)

`CREATE INDEX ON t USING hnsw (v vector_cosine_ops)` — the form in pgvector's own README — failed
with "Index name is required". Names are now generated as PostgreSQL does (`{table}_{col}_idx`,
uniquified on collision, truncated at 63 bytes), for both the ART and vector branches, and a
generated name is droppable and cannot collide with a constraint index.

### Fixed — JSON operators reject uncast literals (#17)

`payload @> '{"user_id":"alice"}'` required an explicit `::jsonb`. PostgreSQL resolves an untyped
literal against the operator signature; Nano now does the same, across **five** operators —
`@>`, `<@`, `?`, `?|`, `?&` — not only the reported one. The error text no longer leaks the
internal Rust representation.

Two related defects found and fixed alongside:

- **`?|` and `?&` could never reach the parser at all.** The SQLite-compat placeholder rewrite
  runs on every statement and turned any bare `?` into `$N`, producing `$1|`. Those two spellings
  are now exempt (a `$N` can never be followed by `|` or `&`). Bare `?` remains a placeholder by
  design — use `col ?| ARRAY['key']` for the single-key form.
- **`#>` was silently mis-planned as the vector inner-product operator**, so `jsonb_col #> '{a,b}'`
  computed an inner product instead of extracting a path. pgvector's operator is `<#>` and was
  handled separately, so the mapping was simply wrong. `#>` / `#>>` now error explicitly; they are
  tracked as unimplemented rather than silently answering wrongly.

### Added — `pg_typeof()` (#18)

Returns the PostgreSQL type name of an expression. Introspection tooling and ORMs verifying column
types reach for it early, and its absence made diagnosing the JSON typing issue above harder.


## [4.21.0] - 2026-08-30

### SECURITY — encryption at rest now covers row data. **Upgrade if you use `[encryption] enabled = true`.**

**Through 4.20.0, enabling encryption did not encrypt the rows written by SQL.** Encryption was
applied per code path rather than at the storage boundary: `StorageEngine::put` sealed values,
but the transaction commit batch, the default single-row autocommit `INSERT`, the batch/`COPY`
path, every MVCC version copy, branch row overlays and materialized-view delta records all wrote
straight to RocksDB with no key manager in scope. A row inserted through ordinary SQL was stored
in the clear, and because materialized-view deltas carry full tuples, a `DELETE` left a complete
plaintext image of the removed row behind. The affected data is whatever was written on an
encryption-enabled database by an affected version.

**What to do.** Upgrading seals *new* writes; it does not retroactively seal data already on
disk. To encrypt an existing database fully, dump and restore it under 4.21.0. To check exposure,
inspect the data directory for known plaintext values.

Encryption now happens at one boundary (`src/storage/tde.rs`) — `crypto::encrypt` and
`crypto::decrypt` have no call sites outside it, so the answer to "is this value sealed?" no
longer depends on which function wrote it. Sealed: row images, MVCC version history, row-id
counters, logical-WAL entries, branch overlays, materialized-view deltas, the catalog and durable
index snapshots, on every write route. **Still not sealed, deliberately and now documented:**
RocksDB *keys* (table and column names, row ids, timestamps); the column sidecars used by
non-default `STORAGE` modes; HNSW graph snapshot files; backup dumps; and the replication link.
See the README's "Encryption at rest" for the full table.

**Reads are tolerant, which is what makes the upgrade safe.** A stored value may be ciphertext or
plaintext and nothing on disk distinguishes them, so every read attempts decryption and falls back
to the raw bytes only on an AEAD authentication failure. A false accept would require forging a
GCM tag (2^-128). Existing databases therefore keep reading correctly while new writes are sealed.

**Wrong-key protection, which tolerant reads would otherwise have removed.** A sentinel is sealed
at `meta:tde:keycheck` and verified at every open with a strict decrypt: a wrong or rotated key is
now refused up front instead of silently serving unreadable bytes, and an encrypted directory
cannot be opened with encryption disabled. A pre-existing database gains a sentinel only after the
configured key is shown to open data already present — an ambiguous database is refused rather
than stamped with a key that may be wrong.

**Not implemented:** key rotation. AES-256-GCM here uses a random 96-bit nonce under one static
key; NIST SP 800-38D caps that at 2^32 invocations per key, and every sealed value costs one.
`[encryption] rotation_interval_days` remains inert and is documented as such.

### Fixed — **on an encrypted (TDE) data directory, every `CREATE INDEX` index silently disappeared at every restart**

**Read this if you run HeliosDB Nano with `[encryption] enabled = true`.** This is a
pre-existing defect, present in every release that supported both TDE and user-created
secondary indexes. It was found while implementing `DROP INDEX`.

`Catalog::save_index_definition` writes the `meta:index:<name>` record through
`StorageEngine::put`, which **encrypts** when a key manager is configured.
`Catalog::list_index_definitions` read those record VALUES straight off a raw RocksDB
iterator, which returns **ciphertext** — `StorageEngine::get` is the one place decryption
happens. Because index-record decoding is deliberately per-record resilient (an undecodable
record is `warn!`ed and SKIPPED so one bad record cannot abort the rebuild of every other
index), nothing failed and nothing errored.

The consequence was total and silent. `Catalog::rebuild_all_indexes` is the ONLY thing that
re-registers user secondary indexes when a process attaches to a data directory, and it reads
that list. On an encrypted database it saw **zero** index definitions, so:

* every index created with `CREATE INDEX` vanished from the moment the process restarted;
* it never came back — each subsequent restart re-read the same ciphertext;
* **query results stayed correct.** Affected queries silently fell back to full table scans.
  There was no error, no failed statement, and nothing above `warn!` in the log.

Unencrypted data directories were never affected.

The fix routes the read through `StorageEngine::meta_blobs_with_prefix`, the single
decrypting prefix-read discipline already used for `list_roles` / `list_acls`. Regression test:
`secondary_indexes_survive_reopen_on_an_encrypted_database` in `tests/drop_index_tests.rs`.

**No action needed beyond upgrading** — index definitions were persisted correctly the whole
time; only reading them back was broken, so your indexes are rebuilt on the first start after
the upgrade.

**The identical defect in `Catalog::list_sequences` is fixed in the same change.** Sequence
definitions are written through the same encrypting `put` and were read off the same kind of
raw iterator with the same per-record resilience, so on an encrypted data directory every
`CREATE SEQUENCE` definition silently disappeared at restart too — taking `nextval`, `SERIAL`
defaults, and the `pg_sequences` / `information_schema.sequences` views with it.

### Added — real `DROP INDEX` (roadmap §2.1, second half)

Through 4.19.0, `DROP INDEX x` fell through the planner's `_ => LogicalPlan::DropTable`
catch-all and **destroyed a TABLE named `x`**. 4.20.0 removed the catch-all and made the
statement a loud error. It is now implemented.

* `DROP INDEX [IF EXISTS] <name> [, …]` drops ART (`art`/`btree`/`hash`), HNSW in every
  flavour (`hnsw`/`hnsw_pq`/`persistent_hnsw`, i.e. including
  `WITH (persistent = true)`) and DDL-only `gin`/`gist` indexes, dispatching on the persisted
  definition so each undoes exactly the `CREATE INDEX` branch that made it. `CREATE INDEX`,
  `DROP INDEX` and the open-time index rebuild all classify the index type through **one**
  shared function (`storage::index_family`), so a new index type cannot be taught to one of
  them and forgotten by the others.
* It removes the `meta:index:` catalog record, which is what makes the drop **durable** —
  indexes are rebuilt from those records at every open, so a drop that left the record behind
  would quietly undo itself at the next restart.
* Reached from the shared `plan_to_operator`, so `db.execute()` and `db.execute_params()`
  (the PostgreSQL **extended** protocol — psycopg, JDBC, sqlx, node-postgres, and the
  REST/BaaS layer) behave identically.
* It gives `StorageEngine::log_drop_index` its first caller, so the drop is replicated; the
  `WalOperation::DropIndex` replay arm now also clears the in-memory ART / vector
  registration, which a long-lived **standby** needs in order to converge.
* Cached plans and cached results are invalidated, so a dropped index cannot keep being used
  by a warm plan cache. This also closes a **pre-existing** gap for every comma-list drop:
  `DROP TABLE a, b` / `DROP VIEW a, b` plan as one `DropMulti` node, which was absent from the
  invalidation list — so the multi-target spelling invalidated nothing while the single-target
  spelling of the same statement invalidated correctly.

**Behaviour worth knowing:**

* **A PRIMARY KEY / UNIQUE / FOREIGN KEY backing index cannot be dropped.** It is refused with
  PostgreSQL's wording (`cannot drop index "…" because constraint … requires it`, SQLSTATE
  2BP01). This matters because those names are reachable: a `UNIQUE` column `email` registers
  a backing index literally called `email`, and a primary key registers `<table>_pkey`.
  Dropping one would have silently removed constraint enforcement.
* **`IF EXISTS` now silences a missing index.** In 4.20.0 it deliberately did not — nothing
  was dropped either way, so a quiet success would have been a silent no-op. Now that a real
  drop exists, PostgreSQL semantics apply. Without `IF EXISTS`, a missing index is
  `index "x" does not exist`, SQLSTATE 42704 undefined_object (previously XX000).
* **A same-named TABLE is never touched**, in either direction — pinned by tests.
* **The MySQL spelling `DROP INDEX <i> ON <t>` is accepted.** sqlparser rejects the trailing
  `ON`, so the MySQL wire previously could not drop an index at all. HeliosDB's index
  namespace is global (one index per name), so the table qualifier cannot disambiguate
  anything and is not used for the lookup — meaning, unlike MySQL, `DROP INDEX i ON
  wrong_table` is **not** rejected. A trailing `ALGORITHM=` / `LOCK=` clause is refused rather
  than silently discarded.
* The PostgreSQL wire now reports the `DROP INDEX` command tag instead of `OK 0`.

## [4.20.0] - 2026-08-28

Closes the five published "honest caveats" where PostgreSQL CE was ahead. Four were real —
three of them **worse** than documented — and the fifth is refuted by a fresh measurement.
Highest-severity item: `DROP ROLE x` was planned as `DROP TABLE x` and silently destroyed a
same-named table.

**Read before adopting:** trigger bodies still do not execute, and privileges are stored and
introspectable but **not enforced** on any read or write path. Both are stated in full below.

### Triggers — partial parity. **Trigger bodies still do not execute.**

Read the previous sentence again before adopting any of this. `CREATE TRIGGER`
still does not give you audit rows, derived-column maintenance, cascades, or any
other side effect, on any interface. What changed is that the ONE trigger
capability that has ever had an observable effect now behaves the same on every
client path, survives a restart, and respects the trigger's own `WHEN` clause.

**Fixed — `CREATE TRIGGER` / `DROP TRIGGER` over the PostgreSQL extended query
protocol.** They were a hard error, `Operator not yet implemented:
CreateTrigger { … }`, for every client that binds parameters server-side:
psycopg (bound params), JDBC, sqlx, Drizzle, node-postgres, Alembic, and the
REST/BaaS layer. Only the simple-query path, the MySQL wire, the REPL and the
embedded `execute()` ever worked. Both statements now run through one shared
implementation called by both executor families.

  *Behaviour change to be aware of:* a migration tool that previously CAUGHT
  that error will now see the statement SUCCEED. It creates a trigger whose body
  will not run. If your migration relied on the failure, it will no longer fail.

**Fixed — the BEFORE-INSERT row rewrite now applies on both executor families.**
The single mechanism with a real effect is `BEFORE INSERT … FOR EACH ROW EXECUTE
FUNCTION f()` where `f`'s body is top-level `NEW.<col> = <expr>` and/or
`RETURN NULL`: it rewrites, or drops, the row being inserted. It used to be
applied at exactly ONE call site, in the text executor family. Consequences that
are now gone:

  - a REST/JDBC/psycopg insert and a `psql` insert into the SAME table produced
    DIFFERENT rows;
  - `INSERT … RETURNING` skipped the rewrite on EVERY interface, including the
    embedded API, because `RETURNING` always routes through the params family.

  `RETURN NULL` now suppresses the row on both families: the row is not written,
  not counted in the affected-row total, and produces no `RETURNING` tuple.

**Fixed — triggers now survive a restart.** They previously survived ZERO
restarts. `StorageEngine::load_triggers()` had no callers, `CREATE TRIGGER` never
wrote to the catalog, and WAL crash-replay registered into a registry the SQL
executor does not read. Both the definition (`trigger:{table}:{name}`) and the
compiled rewrite recipe (a new, purely additive `trigger_rowmut:{table}:{name}`
sidecar) are now persisted, and the live registry is repopulated at open.

  *Residual gap, stated rather than hidden:* WAL crash-replay restores the
  DEFINITION but not the recipe sidecar. A trigger created after the last
  checkpoint and before a crash comes back present but inert — `DROP TRIGGER` and
  re-create it. A standby likewise only picks up replicated triggers at its next
  restart.

  `Catalog::load_all_triggers` also read values straight off the RocksDB
  iterator, which returns CIPHERTEXT on a TDE data directory; it now reads
  through `StorageEngine::get`. An unreadable record is logged and skipped rather
  than blocking the database from opening.

**Fixed — the rewrite honours `WHEN` and the `enabled` flag.** It previously
ignored both, so `BEFORE INSERT … WHEN (NEW.id > 10) …` rewrote every row
regardless of the predicate. `WHEN` is now evaluated against the NEW row and the
recipe fires only when it is TRUE; FALSE, NULL and an unevaluable predicate all
mean "not fired" (PostgreSQL semantics). Multiple recipes on one table now fire
in trigger-NAME order, as PostgreSQL does, instead of hash order.

**Fixed — `DROP TABLE` deregisters the table's triggers.** It used to leave them
behind, so the trigger name was unusable for the lifetime of the process and
re-creating the table plus its trigger failed with `already exists`. Both the
in-memory registrations and the persisted records are now removed.

**Still not implemented (unchanged):** trigger body execution of any kind;
`AFTER` triggers; `FOR EACH STATEMENT`; `INSTEAD OF`; the row rewrite on
`BEFORE UPDATE` / `BEFORE DELETE`; `OLD`; deferred/CONSTRAINT triggers; and
trigger introspection (`pg_trigger` does not exist,
`information_schema.triggers` is empty, and psql's `\d` reports
`relhastriggers = false` — the column does not exist on `pg_class` on any
other route). There is still no SQL surface for `ALTER TABLE … ENABLE/DISABLE
TRIGGER`, so the now-honoured `enabled` flag is only reachable from the library.
Use a `CREATE PROCEDURE` invoked with `CALL`, or a second statement in the same
transaction, wherever you would reach for a trigger body.

**Tests:** `tests/trigger_unimplemented_tests.rs` was a deliberately
red-on-purpose pin suite; it is deleted and replaced by
`tests/trigger_row_mutation_tests.rs`, which exercises the DDL matrix, the
rewrite, `RETURN NULL`, `WHEN`, restart durability and `DROP TABLE` cleanup
through BOTH `execute()` and `execute_params()`, and still pins — unconditionally
— that side-effecting bodies, `AFTER` triggers and statement-level triggers do
nothing.

### `DROP` no longer falls through to `DROP TABLE`. **Data-loss fix.**

**Fixed — `DROP ROLE x` and `DROP INDEX x` were executed as `DROP TABLE x`.**
The planner's `Statement::Drop` handler ended in
`_ => LogicalPlan::DropTable { … }`, so every object kind without an explicit
arm resolved a *table* reference with the object's name and dropped it:

```sql
CREATE TABLE analyst (…);        -- a real table
DROP ROLE analyst;               -- silently DROPPED THE TABLE, reported success
DROP INDEX IF EXISTS analyst;    -- likewise, and with no error at all
```

**If you ever ran `DROP ROLE` or `DROP INDEX` against a name that also existed
as a table, that table was deleted.** Check your data. The `IF EXISTS` spellings
were the worst case: they dropped the table and returned success.

The catch-all is removed. The match is now exhaustive over sqlparser's
`ObjectType`, so a future parser upgrade that adds an object kind is a compile
error rather than a silent table deletion.

  **Behaviour change / breaking.** `DROP INDEX` (with or without `IF EXISTS`)
  now returns an error naming itself: *DROP INDEX is not supported yet*. It
  previously "succeeded" while dropping either nothing or the wrong object.
  `IF EXISTS` deliberately does NOT silence it — HeliosDB cannot drop an index
  from SQL at all, so reporting success would be a lie. Indexes are still
  removed with their table. Scripts containing `DROP INDEX IF EXISTS …` will
  now fail loudly; remove the statement.

### Roles and privileges

> **Roles and grants are STORED and introspectable. Privileges are NOT
> enforced.** No read or write path checks a permission. A row in
> `information_schema.table_privileges` means "somebody ran `GRANT`", not
> "access is restricted". Do not treat any of this as a security control.

**Added — `CREATE ROLE` / `ALTER ROLE` / `DROP ROLE` are real DDL** (and the
`CREATE USER` / `ALTER USER` / `DROP USER` spellings), persisted to the catalog
under `meta:role:<name>` with a stable OID. `GRANT` / `REVOKE` persist an ACL
record instead of parsing and discarding it. Both executor families — `execute()`
and `execute_params()`, i.e. simple query AND the extended protocol every real
driver uses — go through one shared implementation.

**Fixed — `pg_roles` / `pg_user` / `pg_authid` invented two all-privilege
superusers.** They now report the real persisted roles with their real attribute
bits, plus the two virtual built-ins. A stored password is never rendered on any
view (`rolpassword` is always `********`). `information_schema.table_privileges`
and `role_table_grants` report the stored ACL records, and resolve on **every**
route — embedded, REPL, Python binding, PostgreSQL wire and MySQL wire — where
they previously resolved (empty) on the PG wire only. MySQL `SHOW GRANTS` reads
the same catalog instead of fabricating an `ALL PRIVILEGES` line.

  **Behaviour change / breaking — `GRANT` / `REVOKE` on an unknown name is now
  an ERROR.** Naming a role or a table that does not exist used to succeed
  silently (having stored nothing). Set `[authentication] legacy_acl_noop = true`
  to restore the old leniency (unknown names skipped, statement succeeds).

  **Behaviour change / breaking — `SET ROLE <x>` and
  `SET SESSION AUTHORIZATION <x>` now fail with `0A000 feature_not_supported`**
  instead of being acknowledged with zero effect. A client, pooler or hardening
  check that believed it had dropped to a restricted role kept full access; that
  is a false security claim. `SET ROLE NONE` and
  `SET SESSION AUTHORIZATION DEFAULT` are still acked — returning to the only
  identity there is, is genuinely satisfied. This applies on the simple query
  path AND the extended protocol. `legacy_acl_noop = true` restores the silent
  ack on both.

**Still not implemented:** privilege enforcement of any kind; role membership
(`GRANT role TO role`) and column-level grants (both rejected at plan time);
session identity (`current_user` is a literal); `DROP OWNED BY`;
`DROP ROLE … CASCADE` (a role holding grants must have them revoked first).

### User-defined functions are callable

**Fixed — `CREATE FUNCTION` registered a function that nothing could call.**
`SELECT f(x)` answered `Unknown scalar function: f` on every interface while a
complete interpreter sat unreachable. Scalar calls now work — in a `SELECT`
list, in a `WHERE` clause, through bound parameters, on the embedded API, the
REPL and both wires — and the definition survives a restart (`meta:function:` /
`meta:procedure:` records, reloaded at open). `CREATE`/`DROP FUNCTION` and
`CREATE`/`DROP PROCEDURE` run through shared helpers called by both executor
families, so the extended protocol is no longer a hard error.

**The `$` sigil is mandatory.** A body must reference parameters as `$1` or
`$paramname`; a bare parameter name is parsed as a column reference and fails
with a column error. This is deliberate — a variable must never silently shadow
a column.

**Not supported:** set-returning functions (`SELECT * FROM f()`), overloading,
`CALL f()` on a FUNCTION, non-`public` qualifiers, and routine introspection
(`pg_proc`, `information_schema.routines` / `parameters` stay empty). PL/pgSQL
control flow inside a FUNCTION body is refused with an explicit error rather
than being half-evaluated. Recursion depth is bounded by the new
`[session] udf_max_call_depth` config key.

  **Behaviour change to be aware of:** the four `sql::Executor` routine-DDL
  stubs used to return a fabricated `Function 'x' created` status without
  creating anything. They now error loudly if reached. Any code that relied on
  that fake success will now see a failure.

  **Behaviour change:** `CREATE/DROP FUNCTION` and `CREATE/DROP PROCEDURE` now
  invalidate the SQL result cache. They previously did not, which — now that
  UDFs are callable — meant `CREATE OR REPLACE FUNCTION f …` kept serving the
  old function's cached result and `DROP FUNCTION f; SELECT f()` kept answering
  instead of erroring.

### Catalog introspection unified

**Fixed — the PostgreSQL wire and the embedded/REPL/Python routes were served by
two different implementations and disagreed.** Twelve wire-only
`information_schema` / `pg_*` implementations are deleted; there is now ONE
registry (`src/sql/phase3/system_views.rs`) behind all five interfaces. Most
visibly, the wire's `information_schema.columns` had no `table_schema` column,
so `WHERE table_schema = 'public'` returned zero rows there while working
embedded.

Now returning real rows on every route: `information_schema.views`,
`check_constraints`, `constraint_column_usage`, `catalog_name`, `pg_views`,
`pg_indexes` and `pg_matviews` (the last three were reachable only over the PG
wire, or empty everywhere while a working implementation sat unreachable).
`information_schema.tables` now also lists views with `table_type = 'VIEW'`, and
`schemata` enumerates real schemas instead of three hardcoded rows.

  **Behaviour change — result SHAPES changed.** `information_schema.columns`
  went from 7 columns to 17 over the PG wire. `information_schema.tables` emits
  additional `VIEW` rows. Anything that selected `*` from these views and
  indexed the result positionally, or that counted rows from `tables`, must be
  rechecked.

  **Still PG-wire only:** `routines`, `parameters`, `triggers`, `domains`,
  `character_sets`, `collations`, `view_table_usage` and `view_column_usage` are
  answered by the PostgreSQL **and MySQL** wire interceptor (the MySQL handler
  routes `information_schema` through the same code) with the correct column
  list and zero rows; on the embedded API, the REPL and the Python binding they
  raise an unknown-relation error. See
  `docs/compatibility/information_schema.md`.

### New configuration keys

Both are optional and defaulted, so an existing `config.toml` keeps parsing
unchanged.

- `[authentication] legacy_acl_noop` (default `false`) — restores the pre-4.20
  silent acceptance of `GRANT`/`REVOKE` on unknown names and of
  `SET ROLE` / `SET SESSION AUTHORIZATION`.
- `[session] udf_max_call_depth` — recursion ceiling for user-defined function
  invocation.

## [4.19.0] - 2026-08-27

### Fixed

- **`CREATE TABLE … AS SELECT` created an empty, column-less table.** It returned
  `Ok(1)` and copied nothing:

  ```
  source s: 3 rows
  CREATE TABLE d  AS SELECT id, n FROM s             -> Ok(1),  d: 0 rows
  CREATE TABLE d2 AS SELECT * FROM s                 -> Ok(1), d2: 0 rows
  CREATE TABLE d3 AS SELECT id, n FROM s WHERE n>15  -> Ok(1), d3: 0 rows
  SELECT id INTO t2 FROM s2                          -> Ok(1), t2 absent
  ```

  **If you used `CREATE TABLE backup AS SELECT * FROM t` as a safety copy before a
  destructive change, that backup was empty and the statement reported success.**
  Check any table you created this way. The planner never read the source query, so
  no data was written anywhere — the rows were never copied, not copied-then-lost.

  `SELECT … INTO newtable` was the same defect and is also fixed. Both now derive
  columns from the query's static schema, so an empty source still produces a
  correctly-shaped table, and a failed population drops the partial table instead of
  leaving it behind.

### Changed

- **CTAS returns the inserted row count**, not `1`. Plain `CREATE TABLE` still
  returns `1`.
- **plpgsql `SELECT … INTO var` now assigns to the variable** (previously the
  statement ran and its result was discarded). It therefore **errors if the target
  variable was never declared**, where it used to appear to succeed. This is
  PostgreSQL semantics, and it is required: without it, the SQL-level `SELECT … INTO`
  fix would make a procedure body containing `SELECT COUNT(*) INTO cnt …` silently
  create a table named `cnt`.

### Known limitations

- The explicit column-name form `CREATE TABLE t (a, b) AS SELECT …` is still a parse
  error (a loud failure, not a silent one). A typed column list alongside a query —
  which PostgreSQL also rejects — now errors explicitly.
- CTAS tables are created with no primary key, no constraints and no indexes
  (PostgreSQL parity). Add them afterwards with `ALTER TABLE` / `CREATE INDEX`.
- Concurrent readers can observe a CTAS target mid-population; DDL is not
  transactional here, the same as `INSERT … SELECT`.

## [4.18.1] - 2026-08-25

### Fixed

- **`MvDeltaTracker::record_delta` overwrote deltas instead of appending them.**
  It keyed storage on the caller's `delta_id`, and `MvDelta::new` — the only
  constructor that accepts a timestamp — hardcodes that field to `0`. Every delta
  built that way wrote the same key, so the second silently replaced the first: a
  caller recording N deltas for one table read back exactly one, with no error.
  `record_delta` now allocates a key id when the field is unset.

  **Not reachable from SQL or any shipped write path.** The engine records deltas
  through `record_insert`/`record_update`/`record_delete`, which allocate ids
  correctly and are unaffected — their numbering is unchanged. This only affected
  embedded-library callers using `record_delta` directly, for whom it is a
  straightforward fix with no migration.

## [4.18.0] - 2026-08-25

### Fixed

- **A branch that had ever had a child could never be dropped.** The parent's
  children list was appended to on create and never pruned, so a dropped child
  stayed in it forever and `DROP BRANCH` kept reporting *has N child branch(es)*:

  ```sql
  CREATE BRANCH parent; CREATE BRANCH child FROM parent;
  DROP BRANCH parent;  -- correctly refused
  DROP BRANCH child;   -- ok
  DROP BRANCH parent;  -- STILL refused, permanently
  ```

  Nothing else writes that key, so there was no workaround: branches accumulated
  with no way to remove them.

- **Merged branches vanished from the catalog views.** `pg_database_branches()`,
  `pg_branch_stats()`, `pg_branches()` and `SHOW BRANCHES` all fed off a listing
  filtered to `Active`, so a merged branch disappeared and the `status` column
  could only ever read "Active" — merge history was unreachable even though
  `BranchState::Merged { into_branch, at_timestamp }` is stored faithfully.

  These four catalog surfaces now report `Active` **and** `Merged`. `Dropped`
  stays hidden: a drop is a delete, not history. Operational listings (version
  GC, branch resolution, REST `/branches`, MCP) are deliberately unchanged and
  remain `Active`-only — widening them would have altered GC retention.

  **This changes row counts.** Anything counting rows from those four views will
  now see merged branches included.

## [4.17.0] - 2026-08-25

### Changed

- **The "constant-time deep pagination" claim is withdrawn.** `README.md` and
  `docs/PERFORMANCE.md` advertised `LIMIT … OFFSET` as *constant-time (~30 µs
  regardless of offset)*, and `benches/external/README.md` claimed *~32 µs
  regardless of offset depth — up to 334× faster than PostgreSQL 13 for
  `OFFSET 99990`*, citing a page that is not in this repository. No committed
  artifact anywhere held an offset or keyset measurement.

  Measured (`perf/pagination_depth_curve.json`, N = 10 000, page 10, embedded
  path, p50):

  | shape | depth 0 | depth 9 000 | growth |
  |---|---|---|---|
  | keyset, `WHERE id > $1` (indexed) | 39 µs | **35 µs** | **0.9× flat** |
  | `OFFSET`, no `ORDER BY` | 11 µs | 1 255 µs | 115× |
  | `OFFSET` + `ORDER BY id` | 36 µs | 4 812 µs | 133× |
  | `OFFSET` + `ORDER BY created_at DESC` | 5 361 µs | 8 880 µs | 1.7× (flat, ~5 ms) |
  | keyset, row-constructor `(a, b)` | 5 282 µs | 5 867 µs | 1.1× (flat, ~5 ms) |

  **`LIMIT … OFFSET` is linear in the offset.** The ~30 µs figure matches the
  single-column *keyset* path, which is genuinely flat; the claim attributed
  keyset's property to `OFFSET`.

  No engine behaviour changed in this release — only what the project claims
  about itself. If you chose deep `OFFSET` on the strength of the old wording,
  switch to keyset on a single indexed column.

### Known limitations

- **Row-constructor keyset does not use an index seek.** `(a, b) < ($1, $2)` is
  evaluated as a post-scan filter: flat in depth, but ~5 ms at 10 000 rows versus
  ~35 µs for the single-column form. Planner-driven keyset pushdown onto
  `scan_table_pk_range` is the fix and is not implemented. Prefer a single
  indexed sort key.
- `OFFSET` without `ORDER BY` skips cheaply *per row* (no decode or decrypt) but
  still steps once per skipped row.
- A `NULL` in a row-constructor comparison makes the comparison unknown
  (PostgreSQL semantics, corrected in 4.16.0), so rows with a `NULL` sort key are
  excluded from every keyset page. Use `NOT NULL` sort keys.

## [4.16.0] - 2026-08-25

### Fixed

- **Row-constructor comparison returned a wrong answer when a NULL preceded a
  decisive element.** `(NULL, 1) < (2, 3)` evaluated to TRUE; PostgreSQL returns
  NULL. The evaluator noted the NULL pair and continued, letting a later pair
  decide a comparison PostgreSQL leaves unknown.

  **Who was affected.** Anyone using row-constructor (tuple) comparison — most
  importantly the keyset-pagination predicate `WHERE (sort_key, id) < (?, ?)`.
  A row whose sort key was NULL compared TRUE on its non-null `id` and appeared
  in **every** keyset page. If you paginate over a nullable sort column, pages
  produced by earlier versions may contain rows that should have been excluded.

  PostgreSQL gives the two operator families opposite precedence, and only the
  ordering family was wrong:

  | | rule | `(NULL,1) op (2,3)` |
  |---|---|---|
  | `<` `<=` `>` `>=` | stop at the first unequal **or null** pair; a null there is unknown | **NULL** (was TRUE) |
  | `=` `<>` | unequal if any members are non-null and unequal | FALSE (unchanged) |

  `=` and `<>` were already correct and are now pinned by tests, so the two rules
  cannot later be "simplified" into one.

  Reported by the HeliosDB Lite team while evaluating Nano's row-constructor
  support for adoption.

- **`MERGE BRANCH ... WITH (conflict_resolution = ...)` no longer pretends to
  work.** `StorageEngine::merge_branch` ignores the strategy argument and returns
  a hard-coded empty conflict list: it never detects a conflict and never applies
  a strategy, so `branch_wins` and `target_wins` both produced the same
  last-writer-wins merge while reporting success. The option is now rejected with
  an explicit *not implemented* error. `delete_branch_after` is honoured and
  continues to work.

  This is a **behaviour change**: the paren-less spelling
  (`WITH conflict_resolution=branch_wins`) previously returned success. It was
  silently doing nothing. Remove the option to merge with last-writer-wins
  semantics, which is what you were already getting.

### Changed

- `branch_merge_conflict_tests` lost 9 of its 13 tests. They drove
  `BranchTransaction` — an API with no production callers whose on-disk key
  encoding the real merge implementation does not read — and six asserted
  conflict detection that does not exist. Three covered mechanics the real merge
  does support and were rewritten against SQL in `branch_merge_surface_tests`.

### Known limitations

- Branch merging is last-writer-wins. Conflicts are not detected and merge
  strategies are not implemented.
- A branch that has ever had a child cannot be dropped: the children index is not
  decremented when the child is dropped.
- Merged branches disappear from `pg_database_branches()`. `BranchState::Merged`
  is recorded in storage but never projected, so merge history is unreachable.
- Row-constructor predicates are evaluated as a post-scan filter, not an index
  seek, so keyset pagination does not currently benefit from an index.

## [4.15.0] - 2026-08-22

### Fixed

- **Materialized-view auto-refresh never refreshed anything.** A view created with the
  documented `WITH (auto_refresh = true)` clause, with the auto-refresh worker started and
  reporting itself running, kept serving its creation-time rows forever. There was no error,
  no warning and no log line at any shipped level: the view simply stayed stale while
  `pg_mv_staleness` and the auto-refresh status view reported normally.

  **Who was affected.** Only callers of the library API `EmbeddedDatabase::start_auto_refresh`.
  Nothing in the server, CLI, REPL, HTTP layer or config starts the worker — the config key
  `[materialized_views] auto_refresh_default` is parsed and read by nothing — so no deployment
  changes behaviour on upgrade unless its own code calls `start_auto_refresh`. **For those
  callers auto-refresh goes from a no-op to genuinely running**: opted-in views will now be
  recomputed on the configured interval, which costs CPU and I/O that this build never spent.
  Review `[materialized_views]` (below) before upgrading.

  Three independent, stacked defects, each sufficient on its own:

  1. **The SQL opt-in never reached the flag the worker reads**, in all three spellings.
     - `CREATE MATERIALIZED VIEW v AS <query> WITH (auto_refresh = true)` — the form the REPL
       help, the `sql::phase3::materialized_views` module docs and the schema skill all teach —
       parsed "successfully" with an EMPTY option list. sqlparser 0.53 only accepts a view's
       option list *before* `AS`; a trailing one is swallowed by `parse_table_factor` as an
       MSSQL-style table hint on the query's last table, and that branch is not dialect-gated.
       Every option written this way was silently discarded. Fixed by canonicalizing the
       trailing form to the pre-`AS` position before parsing
       (`Parser::preprocess_mv_with_options`); genuine table hints (`WITH (NOLOCK)`, no `=`)
       are left alone.
     - `CREATE MATERIALIZED VIEW v WITH (auto_refresh = true) AS <query>` — the PostgreSQL
       standard position — parsed correctly but the executor wrote only
       `refresh_strategy = 'auto'`, a display-only field surfaced by `pg_matviews` that nothing
       gates on. The runtime gate reads `metadata["auto_refresh"]`, which was never written.
       Fixed: every parsed option is now persisted, and `auto_refresh` lands on the key the
       worker reads.
     - `CREATE MATERIALIZED VIEW IF NOT EXISTS …` takes its own pre-parse path, which
       hard-coded "no options". A trailing clause was dropped and a pre-`AS` clause was glued
       onto the view NAME. Both spellings now work on this path.
     - `ALTER MATERIALIZED VIEW v SET (auto_refresh = …)` worked only by accident (an
       unknown-key passthrough) and left `refresh_strategy` disagreeing with it, while the
       documented `SET (refresh_strategy = 'auto')` set the label without enabling anything.
       Both spellings are now explicit, validated (`auto_refresh` must be a boolean) and kept
       in sync with each other.
  2. **The refresh queue had no consumer.** `MVScheduler::run()` is the only code that pops the
     queue, and nothing in the library or the binaries ever spawned it — scheduled refreshes
     were enqueued and sat there. The auto-refresh worker now owns that task: it starts with
     the worker and is aborted by `stop_auto_refresh()`, by `EmbeddedDatabase::drop`, and by
     `AutoRefreshWorker`'s own `Drop`.
  3. **The scheduled refresh could not have changed content even if it had run.** It called
     `IncrementalRefresher::refresh_incremental`, whose deltas come from a tracker
     (`mv_incremental::DeltaTracker`) that no DML path ever writes to. It would have "succeeded"
     with zero deltas and reset the staleness clock over unchanged rows — a view reporting
     itself fresh while serving stale data, which is worse than doing nothing. Scheduled
     refreshes now dispatch the same code path a user-issued
     `REFRESH MATERIALIZED VIEW … CONCURRENTLY` takes: re-execute the stored optimized plan and
     atomically swap the view's rows, on the blocking pool rather than a runtime worker thread.
     `IncrementalRefresher` is unchanged and still public; it is simply no longer on the
     auto-refresh path.

- **A background MV refresh did not invalidate the reader's result cache.** `EmbeddedDatabase`
  invalidates its SQL-text-keyed result cache on every mutation that goes *through* the handle.
  The refresh worker deliberately does not hold an `EmbeddedDatabase` (it must not — see the
  4.7.0 shutdown note), so a reader that had cached `SELECT … FROM <mv>` would have kept serving
  pre-refresh rows indefinitely. Cached results are now reconciled against
  `StorageEngine::schema_generation`, which closes the hole for any out-of-handle catalog change,
  not just MV refresh. Cost is one atomic load on a cached read, plus one load and one store per
  cache invalidation (i.e. per mutating statement); neither is measurable on the perf gate.

  Invalidating the cache also advances the reconciliation marker, and a new handle seeds it from
  storage rather than from zero. Without both, a catalog change made *through* the handle — every
  `CREATE`/`ALTER`/`DROP`/branch switch — would leave the marker behind and make the next cached
  read discard a still-valid warm cache, so how long a cache survived would depend on which
  internal read path a caller happened to take. Correctness never depended on this: the reconcile
  can only over-invalidate, never serve a stale row.

- **A trigger firing silently stopped the auto-refresh worker.** `clone_for_trigger()` mints
  short-lived `EmbeddedDatabase` values that share the worker and drop mid-statement, and `Drop`
  called `request_stop()` unconditionally — so the first trigger or PL/pgSQL function to run
  killed auto-refresh. Only the last owner may stop it now
  (`Arc::strong_count(&auto_refresh_worker) == 1`). Unnoticed until now because the feature had
  never worked.

- **A second `start_auto_refresh` silently orphaned the first worker**, whose dropped command
  channel left its loop spinning on the sleep branch forever. It is now rejected with an error;
  call `stop_auto_refresh()` first.

### Changed

- `[materialized_views]` now configures the MV refresh **scheduler**, not just the staleness
  worker. `refresh_check_interval_secs` sets how often the scheduler drains its queue,
  `default_max_cpu_percent` is the CPU ceiling above which it skips a batch, and
  `max_concurrent_refreshes` caps concurrent refreshes. These were previously hardcoded
  (`SchedulerConfig::default()`: 5 s / 70 % / 4) and inert, because the scheduler loop was never
  started. **The CPU gate is now real for the first time** — it read a cached value that only
  the never-spawned loop updated, so it was permanently `0.0` and could never trip.

- Scheduler refresh logs report the view's resulting `row_count` rather than a delta count
  (the delta count came from the unfed tracker and was always zero).

### Known limitations

- Auto-refresh can only be started from the embedded library API
  (`EmbeddedDatabase::start_auto_refresh`). There is no CLI flag, config key, SQL statement or
  HTTP endpoint that starts it; `[materialized_views] auto_refresh_default` is parsed and read
  by nothing.
- The per-view `WITH (max_cpu_percent = …)` option is stored but not enforced — throttling is
  governed by the global `AutoRefreshConfig` / `[materialized_views]` limits. The same is true
  of `threshold_table_size`, `threshold_dml_rate`, `lazy_update`, `lazy_catchup_window`,
  `distribution` and `replication_factor`: they now persist (previously most were parsed and
  thrown away) but have no consumer.
- The `auto_refresh` flag has no SQL projection. `pg_matviews` is an empty PG-compat stub and
  `pg_mv_staleness` does not expose the flag, so the opt-in cannot be read back over the wire.
- Scheduled refreshes are always FULL recomputes on a time-based staleness trigger; there is no
  change-detection gate, and `MVScheduler::on_base_table_change` still has no callers.
- The worker-side `max_concurrent_refreshes` counter is reset unconditionally 5 s after
  scheduling (pre-existing); the scheduler-side limit is the effective one.

## [4.14.0] - 2026-08-21

### Fixed

- **PRIMARY KEY / UNIQUE were not enforced on non-SQL inserts when
  `storage.time_travel_enabled = false`** — duplicate primary keys were accepted
  silently and written durably, with no error to the caller and nothing in the
  log at any shipped level.

  **Which profiles.** `fast` and `fast_ingest`, both of which set
  `time_travel_enabled = false` (`src/config.rs`). `safe`, `balanced` and
  `agent` set it to `true` and were never affected. Setting
  `storage.time_travel_enabled = false` by hand, without a profile, had the same
  effect.

  **Which entry points.** Only callers that reach `StorageEngine::insert_tuple`
  directly:

  | Entry point | Source |
  |---|---|
  | REST `POST /rest/v1/<table>` | `src/api/handlers/data_handler.rs` |
  | Dump RESTORE (CLI `restore`, `db.restore_from_dump`, `restore_tables`) | `src/embedded_db_dump.rs` |
  | Protocol adapters | `src/protocols/adapters/executor.rs` |
  | Materialized-view create/refresh (incl. incremental) | `src/storage/materialized_view.rs`, `src/storage/mv_incremental.rs` |
  | FK-violation audit rows | `src/lib.rs` |

  **SQL and wire clients were NOT affected.** Every `INSERT` statement — psql,
  psycopg, sqlx, JDBC, the MySQL listener, the REPL, `db.execute`,
  `db.execute_params` — goes through `insert_tuple_fast`, which has always
  checked. A duplicate `INSERT INTO t VALUES (1, …)` was rejected under both
  settings, which is why this never surfaced in wire-protocol testing.

  **Consequence for backups: a restore could write duplicate primary keys.**
  Restoring a dump into a `fast`/`fast_ingest` database applied rows through the
  unchecked path, so a dump replayed onto a non-empty table (or replayed twice)
  produced a table with repeated PK values that no subsequent `INSERT` could
  have created. Such a table is inconsistent in a way reads expose unevenly: an
  indexed lookup returns the one row the ART index registered, a full scan
  returns them all.

  *Cause.* `insert_tuple` has two arms and the arm is selected by
  `time_travel_enabled`. The versioned arm checked; the non-versioned arm — the
  one the fast profiles select — never called `check_unique_constraints` at all.
  It did call `art_index_manager.on_insert`, whose duplicate-key rejection was
  the only signal the row was bad, and that error was discarded at
  `tracing::debug!` (off in every shipped configuration). So the row landed in
  the heap, the index refused it, and nothing reported the divergence.

  *Fix.* One rule, one implementation: a single private
  `StorageEngine::check_insert_constraints` is now THE pre-insert PK/UNIQUE gate,
  called by all three insert arms — the non-versioned arm of `insert_tuple`, the
  versioned arm (`insert_tuple_versioned_with_schema`) and the SQL fast path
  (`insert_tuple_fast`). Each arm checks exactly once, before anything is
  written. The shared gate is tuple-backed, so the versioned arm no longer
  allocates a per-row column map for its check (one `HashMap` less per insert on
  the default profile); `time_travel_enabled = true` behaviour is otherwise
  unchanged, and the SQL path is unchanged. A failed post-write index
  maintenance call is now reported at `WARN` with the table, the row id and the
  recovery action instead of `debug!` — it cannot become an error return,
  because at that point the row is already durable and the caller would be told
  a persisted write had failed. (`src/storage/engine.rs`; new
  `tests/insert_tuple_constraint_parity_tests.rs` asserts the rule on every arm
  under both settings, including that NULLs in a nullable UNIQUE column stay
  distinct and that legitimate inserts are not over-rejected.)

## [4.13.1] - 2026-08-17

**A documented surface that only worked if you happened to add a `WHERE`.** `RETURNING` is
advertised for `DELETE` in the README and in the `heliosdb-nano-query` skill, but the bare form —
no `WHERE`, no alias — never parsed.

### Fixed

- **`DELETE FROM t RETURNING …` failed to parse** with
  `Expected: end of statement, found: id at Line: 1, Column: 25`, the error column landing
  immediately after `RETURNING `. Every projection shape was affected:

  | Broken | Worked |
  |---|---|
  | `DELETE FROM t RETURNING id` | `DELETE FROM t AS x RETURNING id` |
  | `DELETE FROM t RETURNING *` | `DELETE FROM t WHERE amount > 0 RETURNING id` |
  | `DELETE FROM t RETURNING id, amount` | `DELETE FROM t WHERE TRUE RETURNING id` |
  | `DELETE FROM public.t RETURNING id` | `UPDATE t SET amount = 1 RETURNING id` |
  | | `INSERT INTO t VALUES (…) RETURNING id` |

  **Who this hit:** anyone deleting a whole table and wanting the removed rows back — queue and
  outbox drains (`DELETE FROM jobs RETURNING *`), "take everything" claim patterns, test-fixture
  teardown that reports what it removed — plus every ORM and client that emits the bare form
  (Drizzle's `.delete(table).returning()`, sqlx, node-postgres, psycopg). It failed **identically
  through `db.query` and `db.query_params`**, and identically over the PostgreSQL and MySQL wire
  protocols, because it failed at PARSE time, before either DML executor family was reached. It
  failed **loudly** — a syntax error, never a wrong or partial delete — so this was a
  reachability defect, not a correctness one. The workaround, for anyone who found it, was to add
  a redundant `WHERE TRUE`.

  *Cause.* sqlparser 0.53's `DELETE` grammar allows a **bare table alias**, and `RETURNING` is not
  reserved in that position — so it was consumed AS THE ALIAS, and the projection list that
  followed had nowhere to go. Each working shape escapes for the same structural reason: an
  explicit `AS x` fills the alias slot, and any `WHERE` ends alias lookahead. `UPDATE` and
  `INSERT` are immune because neither grammar has a bare-alias slot there — which is why every
  passing `RETURNING` test in the repo either was an `UPDATE`/`INSERT` or carried a `WHERE`, and
  why the gap went unnoticed.

  *Fix:* a new `Parser::rewrite_delete_returning` inserts `WHERE TRUE` between the table
  reference and `RETURNING`. It is chained into the same parse-failure fallback that already
  carries the Stage-0 partitioning rewrites, so it keeps that path's **strictly-additive
  guarantee**: it runs ONLY on SQL that fails to parse today, and a rewrite that still fails
  reports the ORIGINAL diagnostic rather than a masked one. `WHERE TRUE` was chosen over filling
  the alias slot because it is semantically neutral — every `DELETE` executor arm treats a
  missing selection as "every row matches", and no `DELETE` path branches on
  `selection.is_none()` — whereas an injected alias would show up in the AST. The rewrite parses
  the statement head structurally rather than searching the text for the word `RETURNING`, so a
  `RETURNING` inside a string literal, a comment or a dollar-quoted body can never be matched.
  Schema-qualified (`public.t`) and quoted (`"my table"`) names, `RETURNING *`, explicit column
  lists, mixed case, embedded newlines and a trailing `;` are all handled.
  (`src/sql/parser.rs`; new `tests/delete_returning_tests.rs`.)

## [4.13.0] - 2026-08-17

**One rule, two implementations — again.** A row-level-security read policy was attached to a
scan leaf by two different mechanisms depending on which plan node the leaf happened to be, and
the two disagreed about whether the policy is evaluated before or after the scan's projection.

### Fixed

- **On a table with an RLS policy, ordinary single-column projections failed with
  `Column '<policy column>' not found in schema`.** With a policy `owner = 'alice'` on
  `docs(id, owner, body)` and a tenant context active, these shapes **errored**:

  | Broken | Worked |
  |---|---|
  | `SELECT id FROM docs` | `SELECT * FROM docs` |
  | `SELECT body FROM docs` | `SELECT id, owner FROM docs` |
  | `SELECT id FROM docs LIMIT 1` | `SELECT id FROM docs WHERE id > 0` |
  | `SELECT id FROM docs LIMIT 1 OFFSET 1` | `SELECT id FROM docs ORDER BY id` |
  | | `SELECT DISTINCT id FROM docs`, `COUNT(*)`, `SUM(id)` |

  **It failed CLOSED** — an error raised before the first row was returned, never extra rows —
  so this was an availability defect, not a disclosure. But it meant an RLS-enabled table
  rejected the most ordinary read a client can issue, which is plausibly why this surface saw
  so little real use.

  *Cause — two mechanisms, and their order.* `ProjectionPruningRule` pushes a projection INTO a
  bare `Scan` when a `Project{distinct:false}` sits **directly** above it, leaving the scan's
  declared schema at full width. RLS injection then wrapped the scan in a `Filter` **above** it,
  and the text-family pipelines apply RLS **after** the optimizer. So `SELECT id` reached
  execution as `Project([id], Filter(owner='alice', Scan{projection:[0]}))`, the scan emitted
  one-column rows, and the filter's `owner` reference had nothing to bind to. Each working shape
  escapes for its own *structural* reason: `WHERE` interposes a `Filter`/`FilteredScan` so the
  `Project`'s input is no longer a bare scan and pruning never fires; `ORDER BY` places the
  `Sort` between the `Project` and the `Scan`; `DISTINCT` fails the rule's `distinct: false`
  match; aggregates interpose an `Aggregate`; and `SELECT id, owner` does prune but keeps the
  policy column inside the projection — which is exactly why the existing RLS read-parity suite,
  whose workhorse read is `SELECT id, owner FROM orders`, never caught it.

  *Fix:* the `Scan` arm of `apply_rls_to_plan_recursive` now emits a `FilteredScan` carrying the
  policy as the scan's **own** predicate — preserving the projection — instead of wrapping a
  `Filter` above the scan. `FilteredScan` evaluates its predicate against the full base-table row
  before projection, so a policy on an unprojected column resolves. Both scan-leaf arms take
  their policy from one new helper, `EmbeddedDatabase::rls_read_predicate`, so a read policy now
  has exactly one parse and one meaning.
  (`src/lib.rs`, `src/sql/executor/scan.rs`; new `tests/rls_projection_shapes_tests.rs`.)

- **The PostgreSQL simple-query protocol disagreed with itself about the same statement.**
  `query_with_columns_for_session` branches on whether the session has an open transaction:
  in autocommit it delegates to the optimized text path, while inside `BEGIN` it plans without
  the optimizer. So `SELECT id FROM docs` under a policy **errored in autocommit and returned
  rows inside a transaction** — the same connection, the same SQL, a different answer either
  side of `BEGIN`. Both branches are now correct, and a regression test pins them against each
  other. (This widened the defect's scope beyond the embedded API: any psql/psycopg client
  reading a policied table hit it.)

- **`VERSIONS BETWEEN` failed on any scan carrying a storage-level predicate.**
  `handle_filtered_scan` resolved its `AS OF` clause with `resolve_as_of`, which rejects
  `VersionsBetween` outright ("cannot be resolved to a single timestamp"), while `handle_scan`
  has a dedicated branch for it. Any `FilteredScan` over a `VERSIONS BETWEEN` source therefore
  failed where the bare `Scan` succeeded — reachable before this release through
  `StorageFilterPushdownRule`, which rewrites `Filter(Scan)` without inspecting `as_of`
  (e.g. `SELECT * FROM t VERSIONS BETWEEN … WHERE id = 5`). Found while making the RLS rewrite
  above behaviour-preserving: it is what makes `FilteredScan` a genuine drop-in superset of
  `Scan`. (`src/sql/executor/scan.rs`.)

### Notes

- Fail-closed behaviour is unchanged and re-pinned: a policy that cannot be parsed still
  propagates `Err` from the shared helper on every read family and every query shape. The new
  suite asserts row counts **and contents** throughout — a fix that widened the projection to
  keep the policy column addressable would return 3 rows where 2 are correct, and would pass any
  test that only checked for `Ok`.
- Still not applied inside CTE bodies, set operations, table functions or subquery expressions —
  unchanged by this release, and tracked separately.

## [4.12.1] - 2026-08-16

**Two broken copies of one rule.** HeliosDB Nano renders `Value`s into SQL text in two
places — stored-routine bodies and the PostgreSQL extended protocol's parameter
substitution — and each place had its own copy of the renderer. Each copy was broken for
exactly the types the other got right. Both are fixed, and there is now **one** renderer.

### Fixed

- **A `TIMESTAMP` argument to a `LANGUAGE sql` procedure or function failed the call.**
  `CALL p($1)` (and any `$1` / `$<paramname>` in a `LANGUAGE sql` body, and any PL/pgSQL
  `$name`) rendered a timestamp with Rust's `Display for DateTime<Utc>`, which appends a
  timezone **name**: `'2026-08-16 01:11:00 UTC'`. Nano's `TIMESTAMP` cast accepts an
  offset but not a name, so the body died with
  `Cannot cast '2026-08-16 01:11:00 UTC' to TIMESTAMP: trailing input`. Affects every
  client — this is not protocol-specific — and it was reachable from the moment routine
  bodies gained working argument interpolation in v4.11.0 / `CALL` started running in
  v4.12.0. Timestamps now render as RFC 3339 (`'2026-08-16T01:11:00+00:00'`).
  *Fix:* `src/sql/interpolate.rs` — `ts.to_rfc3339()`.
- **`DATE`, `TIME` and `UUID` values rendered triple-quoted in extended-protocol
  parameter substitution.** `src/protocol/postgres/prepared.rs` carried a private copy of
  the renderer whose catch-all arm wrapped `impl Display for Value` output in quotes —
  but `Display` already emits its own quotes for `Date` / `Time` / `Uuid` / `String` /
  `Json` / `Timestamp`. Everything that reached the catch-all came out as
  `'''2026-08-16'''`, which fails with `Cannot cast ''2026-08-16'' to DATE`. The same arm
  rendered `Interval` as a lossy `'01:11:00'` clock string and `Array` as a quoted
  `'{1, 2}'` **string** rather than an array.
  *Scope, precisely:* on the live wire path the substituted text is handed only to the
  regex-driven catalog dispatcher (real execution threads the values through
  `query_params` / `execute_params`), and the wire parameter decoder never produces
  `Date` / `Time` / `Numeric` / `Interval` / `Array` / `Vector` — so the wire-reachable
  case was a binary `UUID` (OID 2950) parameter feeding a `pg_catalog` /
  `information_schema` probe, where it silently mis-filtered. The full breakage was
  reachable through the public library API
  `heliosdb_nano::protocol::postgres::prepared::substitute_parameters`.
- **`Value::Vector` did not render as SQL at all.** The routine-body copy emitted a bare
  `[1.5,2.5]`, which is not valid SQL; the extended-protocol copy emitted `ARRAY[1.5,2.5]`,
  which re-parses as an `ARRAY`, not a vector. Both now render pgvector's text form,
  `'[1.5,2.5]'::vector`, which round-trips.
- **A `Value::Numeric` holding a non-number was spliced into SQL unquoted.**
  `Value::Numeric` is String-backed and legitimately holds PostgreSQL's special tokens
  (`NaN`, `Infinity`, `-Infinity`), which were emitted bare — SQL reads them as column
  references. Non-numeric contents are now quoted and cast (`'NaN'::numeric`); plain
  decimals stay unquoted so they keep their numeric-ness and so `ARRAY[…]` elements keep
  parsing as numbers.

### Changed

- **One renderer, not two.** `src/protocol/postgres/prepared.rs::substitute_parameters`
  now calls the shared `crate::sql::interpolate::value_to_sql_literal`; its private copy
  is deleted. The shared function is exhaustive over `Value` with **no** catch-all arm, so
  a new variant is a compile error rather than a silently-wrong rendering. The contract is
  documented on the function: *a rendering must be valid SQL that re-parses to an equal
  `Value`*.
- **Extended-protocol substitution output changed for four types** (visible only to direct
  callers of the public `substitute_parameters`, and to the catalog dispatcher): `Timestamp`
  loses its `::timestamp` suffix, `Json` loses its `::jsonb` suffix (neither is needed —
  the substituted text is no longer executed), `Numeric` loses its quotes for plain
  decimals, and `Bytes` renders as `E'\xdead'` instead of `'\xdead'` (identical text after
  escape processing). `Int`/`Float`/`Bool`/`String`/`Null` rendering is unchanged.

### Added

- `tests/value_rendering_tests.rs` — the pinned per-variant rendering table, plus a
  round-trip of **every** `Value` variant through both consumers (`CALL p($1)` into a
  `LANGUAGE sql` body via `execute_params`, and `substitute_parameters` followed by
  `execute`), the three measured regressions as named tests, an assertion that the two
  entry points produce byte-identical output, and a property test that no rendering ever
  begins with a doubled quote — the shape of the extended-protocol bug.

### Known gaps (unchanged by this release, documented for the first time)

- A non-finite `Float4` / `Float8` still renders as Rust's `NaN` / `inf`, which SQL reads
  as an identifier. It fails loudly rather than corrupting data.
- `CAST(… AS INTERVAL)` is unimplemented in the evaluator, so an `INTERVAL` column cannot
  be written at all — independent of rendering. The `INTERVAL '<n> microseconds'` literal
  itself parses and evaluates correctly.
- A plain-decimal `Value::Numeric` re-parses through `f64`, so beyond ~17 significant
  digits it loses precision.

## [4.12.0] - 2026-08-16

**`CALL <procedure>` was a silent no-op for every extended-protocol and REST client.**
If your application invokes a stored procedure through psycopg with server-side bind,
JDBC, sqlx, Drizzle, node-postgres, or a PostgREST-style `/rest/v1` write — rather than
through psql simple-query, the MySQL wire, the REPL, or the embedded `execute()` — the
procedure body **never ran**, the statement reported one affected row, and `CALL` on a
procedure that did not exist reported success. Re-run any procedure you believed had
executed on those paths.

### Fixed

- **`CALL` now executes the procedure body in BOTH DML executor families.** Nano has two:
  `db.execute()` → `execute_in_transaction_inner` (psql simple-query, the whole MySQL wire,
  the REPL, embedded), and `db.execute_params()` → `execute_plan_with_params_inner` (the
  PostgreSQL extended protocol and every REST/BaaS write). Only the first had a
  `LogicalPlan::Call` arm. The second fell through to a `sql::Executor`, which holds no
  function registry and could only answer with a status message — so `CALL p()` returned
  `Ok(1)` and wrote nothing. Measured before the fix:

  | statement | `execute()` | `execute_params()` |
  |---|---|---|
  | `CALL p0()` | Ok, row inserted | **Ok(1), no row inserted** |
  | `CALL p1($1)` | n/a | **Ok(1), no row inserted** |
  | `CALL nonexistent_proc()` | `Err: Procedure … does not exist` | **Ok(1)** |

  Both families now dispatch to one shared implementation, `execute_call_plan`, so they
  cannot drift again. Bound arguments bind: `CALL p($1)` with a server-side parameter now
  reaches the body.
- **`CALL` on a procedure that does not exist now errors on every path.** The params family
  never consulted the procedure registry at all, so a typo'd or dropped procedure was
  indistinguishable from a successful call.
- **`rows_affected` for `CALL` is 0 in both families** (it was 1 in the params family — the
  stub's own status *message*, counted as a row). PostgreSQL's command tag for `CALL`
  carries no row count; 0 is the honest value, and it is what the text family always
  returned.
- **The `sql::Executor` `Call` stub returns an error instead of a fake success.** An
  `Executor` cannot run a procedure body, so any remaining route into it — notably
  `query("CALL …")` — now fails loudly rather than returning a one-row status message that
  reads like success.

### Known limitation

- **`CALL` inside an explicit `BEGIN` on the embedded API / REPL is refused with an error.**
  A procedure body is run by re-entering the executor, which re-takes the process-wide
  transaction lock; that lock is not reentrant, so this combination previously **hung the
  calling thread**. It is now a clear error naming the procedure and stating the body did
  not run. Issue the `CALL` outside the transaction, or inline the body. This does **not**
  affect the PostgreSQL or MySQL wire: a wire `BEGIN` opens a per-session transaction, which
  does not use the global lock. Tracked as `docs/plans/ROADMAP_V5.md` §2.11 together with the
  pre-existing behaviour that a procedure body does not join its caller's transaction.

### Documentation

- `README.md`, `AGENTS.md`, `docs/llms.txt`, `docs/compatibility/plpgsql.md` and the
  `heliosdb-nano-schema` skill all recommend a `CREATE PROCEDURE` invoked with `CALL` as the
  replacement for the unimplemented triggers. That advice was inert for extended-protocol and
  REST clients; it is now true on every path, and each of those documents states the
  transaction limitation above.

## [4.11.0] - 2026-08-11

**Stored-procedure parameter substitution was corrupting bodies, silently. It is now
one literal-aware scanner, and it also works in `LANGUAGE plpgsql`.** If you have a
`CREATE PROCEDURE` with more than nine parameters, with two parameter names where one
is a prefix of the other, or with a `$` anywhere inside a string literal in its body,
re-read the Fixed section below — some of those cases wrote *wrong data* with no error.

### Fixed

- **Substitution no longer happens inside string literals.** A body such as
  `INSERT INTO t VALUES ($1, 'price is $1 dollars')` called with `5` used to store
  `price is 5 dollars`. It now stores `price is $1 dollars` — the literal text, as
  written. This is the silent one: no error was ever raised. The same now holds for
  `E'…'` escape strings, `"quoted identifiers"`, `--` and `/* … */` comments (comments
  nest, as in PostgreSQL), and `$tag$ … $tag$` dollar-quoted blocks.
- **`$1` no longer captures the prefix of `$10`.** With ten parameters, a body
  containing `$10` had its `$1` replaced first, leaving the substituted value followed
  by a stray `0` — for `CALL p(1,…,9,99)` that produced the text `10`, which is a valid
  integer, so the row stored `10` instead of `99`. Also silent. Placeholder names are
  now matched by longest-match.
- **`$p` no longer captures the prefix of `$p_id`.** Parameters `p` and `p_id` in the
  same body produced `7_id` — a loud parse error outside a string literal, a silently
  wrong string inside one. It was also declaration-order dependent: declaring the longer
  name first happened to work. Order no longer matters.
- **Argument data can no longer influence how other placeholders are interpolated.** The
  positional pass ran before the named pass, so a value inserted by the first pass was
  re-scanned by the second: `CALL p('$name', 'INJECTED')` failed with
  `Expected: ), found: INJECTED`. Substituted text is never re-scanned now, by
  construction — a single left-to-right pass.
- The never-reachable `LANGUAGE sql` **function** path (`execute_sql_function`) had the
  identical defects and was fixed with the same routine.
- Escaping of `'` in interpolated values is unchanged: `O'Brien` still round-trips.

### Added

- **`LANGUAGE plpgsql` procedure bodies now bind parameters**, by name (`$p_id`) and
  positionally (`$1`). Through 4.10.2 a plpgsql body substituted nothing at all — `$p_id`
  failed with `Invalid parameter placeholder: $p_id. Expected format: $1, $2, etc.` and
  `$1` with `Parameter $1 not provided. Expected 1 parameters, got 0`. Both languages now
  go through the same scanner (`src/sql/interpolate.rs`), called from
  `execute_sql_procedure` for `LANGUAGE sql` and from `ExecutionContext::interpolate` for
  the procedural runtime. `DECLARE`d variables interpolate from the procedural scope by the
  same rule. `EXECUTE '<dynamic sql>'` is deliberately *not* interpolated, matching
  PostgreSQL.
- Unresolvable placeholders are now left verbatim **by design**, so the planner still
  raises its existing loud error (`Invalid parameter placeholder: $oops`,
  `Parameter $9 not provided`). Typos do not become silent no-ops.

### Unchanged, and deliberate

- **The `$` sigil is still mandatory, in both languages.** A bare parameter name remains a
  column reference and still fails with `Column 'n' not found in schema`. PostgreSQL
  resolves bare PL/pgSQL variable names; HeliosDB Nano does not, so that a procedure
  variable can never silently shadow a column of the same name — which is the same
  wrong-data class as the bugs above. Real PostgreSQL plpgsql bodies that reference
  parameters bare need a `$` added when porting.
- Substitution remains **textual** (values render to SQL literals and the statement is
  re-parsed), not bound parameters. Moving to real binding is a follow-up that will reuse
  the same scanner.
- `SELECT … INTO <var>` inside a plpgsql body still does not populate the variable, `:=`
  assignments still store the raw expression *text* rather than a computed value (so
  interpolating such a local yields quoted text), `CALL` still does not validate argument
  count, and a `;` inside a string literal still splits a plpgsql body statement. All
  pre-existing; all tracked in `docs/plans/ROADMAP_V5.md` §2.9.

### Behavioural note

- `$01` used to survive verbatim (the old code searched for the exact text `$1`) and then
  failed downstream; the digit run now parses as `1` and substitutes. `$0` and `$00` still
  resolve to nothing and are left verbatim.

### Docs

- `README.md` (stored-procedure and trigger bullets), `AGENTS.md`, `docs/llms.txt`,
  `docs/compatibility/plpgsql.md` ("works, with two rules" → one rule),
  `docs/plans/ROADMAP_V5.md` §2.9 (fix shape (b) marked done; §2.2's qualification
  annotated), the `heliosdb-nano-schema` skill (Recipe 6, verb table, pitfalls) and the
  skills verb-map index all now state the one-rule form.
- `tests/procedure_interpolation_tests.rs` is the new regression matrix;
  `tests/function_unimplemented_tests.rs` had its two `LANGUAGE plpgsql` pins rewritten to
  assert the new behaviour (the bare-name pins stay failing by design).

## [4.10.2] - 2026-08-11

**Documentation correction: user-defined functions are not callable. Plus a
needed qualification to 4.10.1's stored-procedure advice.** No code behaviour
changes in this entry. If you wrote a `CREATE FUNCTION` against HeliosDB Nano, it
registered and it has never run — nothing in the database can call it. Stored
procedures, by contrast, do work; 4.10.1 just did not tell you the two rules for
writing one that binds its arguments.

- **Qualification to [4.10.1].** That entry said, of triggers: *"What to use
  instead: … a `CREATE PROCEDURE` invoked with `CALL` (procedures do execute)."*
  That advice stands — procedures execute and their arguments do bind — but it
  was under-specified, and following it without the two rules below lands you on
  a form that errors:
  - **Use `LANGUAGE sql`.** A `LANGUAGE plpgsql` procedure body performs no
    parameter substitution at all: `$p_id` fails with
    `Invalid parameter placeholder: $p_id. Expected format: $1, $2, etc.`, and
    `$1` fails with `Parameter $1 not provided. Expected 1 parameters, got 0`.
  - **Reference parameters with a `$` sigil** — by name (`$p_id`) or
    positionally (`$1`). A *bare* parameter name fails with
    `Column 'n' not found in schema` in **either** language.

  ```sql
  CREATE PROCEDURE log_named(p_id INTEGER, p_op TEXT) LANGUAGE sql
      AS $$INSERT INTO audit VALUES ($p_id, $p_op)$$;
  CALL log_named(42, 'hello');   -- OK, (42, 'hello') is inserted
  ```

  A zero-parameter body works, and a body that never mentions its parameter
  succeeds while silently discarding the argument. Within these rules a procedure
  is a legitimate replacement for a trigger. (The 4.10.1 entry itself is left
  intact as released history.)
- **User-defined functions are registered but callable by nothing.** All three
  `CREATE FUNCTION` forms — `LANGUAGE plpgsql`, `LANGUAGE sql`, and
  `RETURNS <type> RETURN <expr>` — parse, register, and return OK. Every
  invocation route then fails:
  - `SELECT f(x)` as a bare select, in a projection alongside columns, and in a
    `WHERE` clause → `Unknown scalar function: f`. Schema-qualifying it
    (`SELECT public.f(x)`) → `Unknown scalar function: public.f`.
  - `SELECT * FROM f(x)` → `Table 'f' does not exist`.
  - `CALL f(x)` → `Procedure 'f' does not exist` (a function is not a procedure).
  - `PERFORM f(x)` → SQL parse error; `PERFORM` is not a statement here.
  - The bound-parameter path fails identically:
    `execute_params("SELECT f($1)", …)` → `Unknown scalar function: f`.

  This is the same on the embedded API and over the PostgreSQL wire. Nothing
  silently returns a wrong answer — every route errors — but there is no route at
  all, and `CREATE FUNCTION` returning OK is the only signal you get.
- **No routine introspection.** `information_schema.routines`,
  `information_schema.parameters`, and `pg_proc` are structurally present on the
  wire and return **zero rows even with a function registered**. On the embedded
  path `information_schema.routines` does not resolve at all and `pg_proc`
  returns no rows. A registered routine is invisible to every ORM probe and
  catalog client.
- **What to use instead of a function:** inline the expression
  (`SELECT id * 2 FROM t`), a view, a column your application maintains, or move
  the logic into application code. There is no in-database replacement — in
  particular, a custom SQLite function ported via the sqlite3 drop-in cannot be
  reimplemented as a `CREATE FUNCTION`.
- **Docs (correction):** `README.md`, `AGENTS.md`, `docs/llms.txt`,
  `docs/compatibility/plpgsql.md`, `docs/compatibility/information_schema.md`,
  and the `heliosdb-nano-schema` / `heliosdb-nano-migrate` agent skills now state
  that user-defined functions are not callable, and carry the working stored-procedure
  recipe with its two rules. The schema skill's Recipe 6 previously ended with
  `SELECT post_count(1);`, a line that errors — it has been replaced with what
  each form actually does. `docs/compatibility/information_schema.md` listed
  `routines` and `parameters` as "Complete" with notes describing rows they have
  never returned; both are now marked as schema-only and always empty.
- **Tests:** added `tests/function_unimplemented_tests.rs`, which asserts
  unconditionally what ships today — every `CREATE FUNCTION` form registers,
  every call route fails with the exact error class, `LANGUAGE sql` procedures
  bind their arguments both by `$name` and by `$1` (asserted on the inserted
  row), `LANGUAGE plpgsql` bodies fail with their two verbatim errors, bare
  parameter names fail in both languages, and all three introspection surfaces
  are empty. Modelled on `tests/trigger_unimplemented_tests.rs`: the function
  half is designed to go red on purpose the day functions start working, and
  should be rewritten rather than relaxed.

## [4.10.1] - 2026-08-07

**Documentation correction: triggers are not implemented.** No code behaviour
changes in this entry — what changes is what we tell you about it. Previous
releases listed triggers as a feature ("Triggers: BEFORE/AFTER
INSERT/UPDATE/DELETE"). That was wrong, and wrong in the most costly way: a
`CREATE TRIGGER` you write today **succeeds, reports no error, registers the
trigger — and then never runs it.** If you built an audit log, a derived-column
maintainer, or any integrity check on a HeliosDB Nano trigger, it has never
fired. Check that data.

- **Docs (correction):** `README.md`, `AGENTS.md`, `docs/llms.txt`,
  `docs/guides/upgrade.md`, the `heliosdb-nano-schema` /
  `heliosdb-nano-overview` agent skills, and the three `examples/trigger_*.rs`
  demos now state that triggers do not execute. The schema skill's trigger
  recipe previously showed a worked audit-log example written in SQLite
  `BEGIN … END` syntax, which this parser cannot parse at all; it has been
  replaced with an accurate description of what each form actually does.
- **What actually happens, precisely.**
  - `CREATE TRIGGER … EXECUTE FUNCTION f()` (PostgreSQL form) parses and
    registers, for every timing (`BEFORE` / `AFTER` / `INSTEAD OF`), every
    event, `FOR EACH ROW` and `FOR EACH STATEMENT`, with or without
    `WHEN (…)`. Nothing fires on INSERT/UPDATE/DELETE. There is no error, no
    warning, and no log line.
  - `CREATE TRIGGER … BEGIN … END` (SQLite/MySQL inline body) is a **parse
    error** — that grammar does not exist here. `CREATE TRIGGER IF NOT EXISTS`
    is likewise a parse error; use `CREATE OR REPLACE TRIGGER`.
  - **The one exception that does have an effect:** `BEFORE INSERT … FOR EACH
    ROW EXECUTE FUNCTION f()` where `f`'s body contains literal
    `NEW.<col> = <expr>` assignments and/or `RETURN NULL` rewrites or skips the
    row being inserted (shipped in 3.58.1). It does not extend to `BEFORE
    UPDATE`/`BEFORE DELETE`, to `AFTER` timings, or to side effects such as
    `INSERT INTO audit_log …` inside the body.
  - `DROP TRIGGER [IF EXISTS] <name> ON <table>` works and deregisters
    correctly. `DROP TABLE`, however, does **not** — the table's trigger
    registrations survive it, so re-creating the table and its trigger fails
    with `Trigger '<name>' already exists on table '<t>'`. That trigger name is
    unusable for the lifetime of the process; `DROP TRIGGER` it before
    `DROP TABLE`, or use a different name.
  - There is no trigger introspection: no `pg_trigger` relation,
    `information_schema.triggers` is empty by design, `pg_class.relhastriggers`
    is always `false`, and no REPL command lists triggers. On a disk-backed
    database a registered trigger survives exactly one restart (WAL replay
    restores it, then the WAL is truncated and nothing reloads it from the
    catalog), so registration is not durable either.

    **CORRECTION (4.20.0):** the "survives exactly one restart" sentence above
    was WRONG. WAL replay registered the replayed trigger into
    `StorageEngine::trigger_registry`, which the SQL executor never reads — the
    executor uses `EmbeddedDatabase::trigger_registry`, which was a brand-new
    empty registry at every open. A trigger therefore survived ZERO restarts, not
    one. This entry also omitted that `CREATE TRIGGER` sent over the PostgreSQL
    *extended* query protocol did not "succeed and never fire": it returned a hard
    error, `Operator not yet implemented: CreateTrigger { … }`. Both are fixed in
    4.20.0 — see that entry.
- **What to use instead:** do the work in your application, in an explicit
  second statement inside the same transaction, or in a `CREATE PROCEDURE`
  invoked with `CALL` (procedures do execute).
- **Tests (correction):** removed `tests/trigger_tests.rs`,
  `tests/decimal_trigger_integration_tests.rs`,
  `tests/trigger_hardening_tests.rs`, `tests/test_trigger_errors.rs` and
  `tests/test_trigger_manual.rs` — about 2,600 lines that read as a
  comprehensive, fully-green trigger suite while providing zero regression
  protection (every assertion sat behind an `if result.is_ok()` /
  `match { Ok => …, Err => eprintln! }` guard that was always taken, and two of
  the files were `fn main()` scripts with no `#[test]` and no assertions at
  all). They are replaced by `tests/trigger_unimplemented_tests.rs`, which
  asserts unconditionally what ships today — including that a registered
  trigger leaves the audit table empty — so the day triggers start working, the
  suite goes red on purpose. (That suite was retired in 4.20.0 and replaced by
  `tests/trigger_row_mutation_tests.rs`, exactly as designed.)

## [4.10.0] - 2026-08-03

**Security fix.** Completes the row-level security work begun in 4.9.0. If you
use RLS, upgrade.

- **Fix (security, pre-existing):** the query result cache was shared across
  tenant contexts. Results were cached under the query text alone, and the cache
  was consulted before any policy was applied — so a query run under a policy
  cached its *filtered* rows and a later caller received them, and a query run
  with no policy cached unfiltered rows that a later policy-bound caller
  received. Both directions were reproduced. Under an active tenant context the
  result cache is now bypassed entirely.
- **Fix (security, pre-existing):** reads inside an explicit transaction applied
  no policy at all. This affected every transactional read — including the
  documented `Transaction` API, which is the recommended way to group reads and
  writes — and required no particular query shape: the first read after `BEGIN`
  was already unfiltered.
- **Fix (security, pre-existing):** `execute()` and `execute_params()` returned
  an unfiltered row count for a `SELECT`, which discloses information a policy is
  meant to hide.
- **Fix (security, pre-existing):** the PostgreSQL simple-query read path did not
  apply policies.

**Scope, unchanged from 4.9.0 and worth repeating.** Tenant context can still
only be established through the embedded Rust API and the REPL; no network
protocol sets one. Row-level security therefore remains unavailable to clients
connecting over PostgreSQL wire, MySQL wire, or `/rest/v1/`, and this release does
not change that. Exposing it additionally requires session-scoping the tenant
context, which is currently one process-wide value.

**Upgrade notes.**
- Queries against policy-protected tables will return fewer rows than before
  wherever a policy was previously being skipped. That is the intended
  behaviour; applications that had come to rely on the missing filtering will
  see the difference.
- While a tenant context is active, the result cache is bypassed, so repeated
  identical queries re-execute. This is a deliberate trade: a shared cache
  cannot safely serve results whose visibility depends on who is asking.
  Behaviour and performance without a tenant context are unchanged.

**Known limitations, tracked.** Policies are not applied inside CTEs, unions, or
table functions, nor to scalar and correlated subqueries. Two library-only
execution paths (the Lite protocol adapter and the Oracle handler) have no access
to tenant state and cannot apply policies at all. Row-level security should not
be relied upon as a security boundary for these constructs.

## [4.9.1] - 2026-08-02

- **Fix (availability, pre-existing):** a replication primary that was killed
  while writing a WAL segment would not restart. Startup did not fail with an
  error — it never finished.

  A partially-written trailing record is the normal state of a write-ahead log
  after any unclean shutdown, so this was reachable by `SIGKILL`, an OOM kill, or
  power loss. (4.7.0's `SIGTERM` handling made ordinary stops clean, which
  reduced how often this was hit but could not prevent it.)

  The segment scanner read a length field from the file and used it to skip over
  the record's payload — and skipping past the end of a file succeeds rather than
  failing, so a corrupt length was never detected. The scan continued into the
  remains of the partial record, read those bytes as if they were a record
  header, and derived an end-of-log position around 8.7 quintillion. Startup then
  tried to build an index entry for every position from the start of the segment
  to that number.

  Segment loading now validates each record's checksum and stops at the first one
  that fails, keeping every record before it. Record sizes are bounded by the
  remaining bytes in the file and by the configured maximum segment size, so a
  corrupt length can no longer drive a large allocation. Damaged segments are
  left on disk untouched rather than truncated, so they remain available for
  inspection.

  A primary in this state now starts normally and recovers every intact record.
  Healthy segments load exactly as before.

## [4.9.0] - 2026-07-30

**Security fix.** If you use row-level security, upgrade.

- **Fix (security, pre-existing):** row-level security was **not enforced on any
  write path**. Policies were evaluated for every `INSERT`, `UPDATE`, and
  `DELETE` — and the result was then discarded. A session subject to a policy
  could modify and delete rows the policy hid from it, with no error.

  `SELECT` filtering worked correctly throughout, which is what made this
  dangerous: reads looked properly isolated, so verifying tenant isolation with
  a query returned a clean result while writes were unrestricted.

  Enforcement now follows PostgreSQL semantics on both the simple-query and
  extended-protocol paths. `USING` decides which existing rows a statement may
  touch — rows outside the policy are silently not affected, exactly as in
  PostgreSQL. `WITH CHECK` validates the row being written and raises
  `new row violates row-level security policy for table "…"` (SQLSTATE 42501).

- **Fix (pre-existing):** when a table had several applicable policies, only the
  first was applied instead of combining them with `OR`. Tables relying on
  multiple permissive policies were more restrictive than declared. This
  affected reads as well as writes.

- **Fix (pre-existing):** a policy declared without a `WITH CHECK` expression did
  not fall back to its `USING` expression, as PostgreSQL specifies.

**Scope — please read.** Tenant context can currently only be established
through the embedded Rust API and the REPL; no network protocol sets one. So
row-level security has never been active for clients connecting over PostgreSQL
wire, MySQL wire, or `/rest/v1/` — this fix does not change that, and RLS should
not be considered available to networked clients. Making it so additionally
requires session-scoping the tenant context, which is currently a single
process-wide value: were it wired up as-is, one connection's tenant would apply
to every concurrent connection. That work is tracked and is not in this release.

**Upgrade note.** Writes that previously succeeded against policy-protected
tables will now be rejected or silently affect fewer rows. That is the intended
behaviour, but if an application has come to depend on the missing enforcement,
it will see new `42501` errors and lower affected-row counts.

## [4.8.0] - 2026-07-29

Replication integrity. If you run HA replication or consume the logical WAL,
read this entry before upgrading.

- **Fix (replication, pre-existing):** writes made inside an explicit or session
  transaction produced **no logical-WAL records at all**, and were therefore
  never sent to standbys. A primary and its standby silently diverged for every
  write issued inside `BEGIN … COMMIT` — which is most writes from any ORM or
  any client that groups work into transactions. Nothing errored; the standby
  simply never received the data. Plain autocommit statements were unaffected,
  which is why this went unnoticed: the failure depended on how a client framed
  its writes, not on what it wrote.

  Local durability was never affected — committed data was always written and
  visible on the primary. What was lost was everything downstream of the logical
  WAL: warm-standby replication and logical replication/CDC.

- **Fix (replication, pre-existing):** synchronous and semi-synchronous
  replication reported success for transactions it had never shipped. The
  acknowledgement wait was only reachable from the per-statement path, so a
  transaction that broadcast nothing also waited for nothing and returned
  "acknowledged". Anyone running `sync_mode` other than async held a guarantee
  that did not exist for transactional writes.

**Upgrade notes.**

- **Standbys may be behind.** A standby that has been replicating from an
  affected primary is missing every transactional write since it was seeded.
  Upgrading the primary fixes replication going forward but does **not**
  backfill what was already lost — re-seed any standby whose contents you need
  to trust.
- **Synchronous commits now actually wait.** Under `sync`/`semi-sync`, a
  `COMMIT` inside a transaction now blocks for standby acknowledgement where it
  previously returned immediately. This is the intended behaviour, but it is a
  real latency change: transactional commit times under sync replication will
  increase from "no wait at all" to the round-trip your topology actually costs.
  Async replication is unaffected.
- Transaction throughput otherwise measures flat (pg35 `Transaction ctl`
  36.1 µs → 35.9 µs).

Known limitation, tracked: a standby applies replicated operations one at a time,
so it can transiently observe a partially-applied transaction before converging.
The WAL format already carries transaction markers for atomic apply, but nothing
emits them yet; wiring that is a separate change.

## [4.7.0] - 2026-07-27

Transaction and shutdown correctness. One atomicity violation, plus three fixes
that together are the reason the engine's clean-shutdown work never ran in a
real deployment.

- **Fix (atomicity, pre-existing):** `ON DELETE CASCADE` and `ON DELETE SET NULL`
  escaped the enclosing transaction. Both ran in their own autocommit
  transaction that committed immediately, so `BEGIN; DELETE parent; ROLLBACK;`
  restored the parent row while the cascaded child deletions — or the NULLed
  child FK columns — stayed permanently applied. The child effects now join the
  caller's transaction and roll back with it. Autocommit behavior is unchanged.
- **Fix (durability, pre-existing):** at close, index snapshots were persisted
  *before* row counters were flushed. Because a valid index snapshot is what
  tells the next open that shutdown was clean, a crash between those two steps
  produced a database that looked cleanly shut down while carrying a stale row
  counter — and the next insert would reuse a live row id, overwriting data.
  This is the failure 4.6.1's counter reseed was meant to prevent. Counters are
  now flushed first.
- **Fix (shutdown, pre-existing):** the server had no `SIGTERM` handler; only
  `Ctrl+C` was handled. Since the Unix default for an unhandled `SIGTERM` is
  immediate termination, no close-time work ran under `systemctl stop`,
  `docker stop`, Kubernetes pod termination — or `heliosdb-nano stop`, which
  sends `SIGTERM` itself and then waits two seconds for a graceful shutdown that
  could not occur. The documented way to stop a server was an abrupt kill.
  `SIGTERM` and `SIGINT` now follow the same shutdown path, and the log records
  which signal arrived.
- **Fix (shutdown, pre-existing):** the HTTP endpoint's accept loop held a
  reference to the database and was never shut down, so the database was never
  dropped and its close-time work never ran under the default `--http-port 8080`
  — not even on `Ctrl+C`. Without this fix the two items above have no effect on
  a default `heliosdb-nano start`.

**Upgrade note:** if you relied on cascaded deletes persisting after a rolled-back
parent `DELETE` — behavior no standard SQL database exhibits — that no longer
happens. Servers now do measurable work on `SIGTERM`; if you run under a process
supervisor with a very short kill timeout, verify it allows shutdown to finish.
`heliosdb-nano stop` still hardcodes a 2-second grace period before `SIGKILL`,
which a large database may exceed; making it configurable is tracked for 4.8.

Also adds `docs/plans/ROADMAP_V5.md`, an audited inventory of known outstanding
issues sequenced toward 5.0. It documents several limitations candidly,
including that row-level security is currently not enforced on write paths.

## [4.6.3] - 2026-07-27

Constraint enforcement is now identical across both DML executor families.

HeliosDB Nano runs two parallel DML paths, selected by the wire frame type: a
*text* family (PostgreSQL simple-query, all MySQL wire, the REPL, the embedded
`execute()` API) and a *params* family (PostgreSQL extended protocol, plus every
REST/BaaS write). The params family had drifted, and three constraint checks the
text family performed were missing from it. Any client that uses the extended
protocol — psycopg2 server-side cursors, JDBC, sqlx, Drizzle, node-postgres — or
that writes through `/rest/v1/`, could bypass them.

- **Fix (data integrity, pre-existing):** a parameterized `DELETE` did not
  enforce *referencing* (inbound) foreign keys. `NO ACTION` / `RESTRICT` were
  not rejected, and `ON DELETE CASCADE` / `SET NULL` never ran — so deleting a
  parent row through the extended protocol silently orphaned its children, with
  no error and no cascade. The same statement over the simple-query protocol
  behaved correctly, which made this hard to spot: the bug depended on the
  client driver, not the SQL.
- **Fix (data integrity, pre-existing):** a parameterized `UPDATE` did not
  enforce `UNIQUE`, for either single-column constraints or multi-column table
  constraints, admitting duplicate keys.
- **Fix (data integrity, pre-existing):** `INSERT ... ON CONFLICT DO UPDATE`
  never revalidated the post-merge row on *either* family. `CHECK` constraints
  were not re-evaluated — a conflict update could set a column to a value the
  table's own `CHECK` forbids — and foreign keys on the updated row went
  unverified.

The three enforcement blocks now live in shared helpers called from both
families rather than in two implementations that can diverge again. Error
messages are unchanged. New suite `tests/constraint_parity_tests.rs` runs each
affected statement through both families and asserts they agree.

Known limitations left unchanged by this release, each pre-existing and tracked
separately: `ON CONFLICT DO UPDATE` does not re-check `NOT NULL` (matching the
general `UPDATE` arm); the `UNIQUE` self-collision guard tests "value changed"
rather than "different row", so a same-statement key swap or cycle is rejected;
`ON DELETE CASCADE` / `SET NULL` run in their own autocommit transaction, so
child-row effects survive a `ROLLBACK` of the parent statement; and the `UNIQUE`
probe reads the branch-blind shared index.

## [4.6.2] - 2026-07-26

Three fixes closing out the latent-bug backlog. The wire-protocol one is the
significant find:

- **Fix (wire-protocol correctness, pre-existing):** the PostgreSQL catalog
  dispatch matched `pg_type` / `pg_tables` / `pg_views` / `pg_settings` /
  `pg_indexes` / `information_schema.` as bare substrings of the raw statement
  text, so a statement that merely *mentioned* one — inside a string literal,
  a SQL comment, or as part of a longer identifier — had its real semantics
  discarded and a canned catalog response returned instead. Worst case, an
  `UPDATE ... SET note='see pg_tables'` silently never executed while the
  client received a reply it could not distinguish from success; a
  `CREATE TABLE pg_type_registry (...)` silently created nothing; and a user
  table named `app_pg_settings` was permanently shadowed. Interception is now
  restricted to read statements, matched against a literal- and
  comment-stripped view of the query, with word-boundary-aware marker
  matching. Client introspection (psql `\dt`, SQLAlchemy/pgAdmin/DBeaver/
  Drizzle probes) is unaffected.
- **`SIMILAR TO` now matches PostgreSQL semantics.** Three bugs fixed:
  alternation was not bound by the implicit full-string anchoring (so
  `'zzxyz' SIMILAR TO 'abc|xyz'` wrongly returned true); the repetition
  quantifiers `*` `+` `?` `{m,n}` were treated as literal characters rather
  than metacharacters (so `'aaa' SIMILAR TO 'a+'` wrongly returned false);
  and the `ESCAPE` clause was parsed but silently ignored. `ESCAPE 'x'`,
  `ESCAPE ''` (escaping disabled), and the default backslash escape all
  behave per PostgreSQL now. This surface previously had no test coverage at
  all; it now has 10 tests.
- **Fix:** bulk loads persisted their row-id counter to a key the engine never
  reads back on open, leaving the canonical counter stale. It now writes
  through the canonical path — relevant for crash recovery of internal
  bookkeeping tables, which are skipped by the counter reseed added in 4.6.1.

## [4.6.1] - 2026-07-21

Four latent-bug fixes, none release-blocking on their own but worth a prompt
patch given the severity of the wire-protocol issue:

- **Fix (silent data corruption, pre-existing):** a hard crash (`kill -9`,
  power loss, abort — anything that skips a clean shutdown) could leave a
  table's row-id counter stale if fewer than 64 rows had been inserted since
  its last periodic flush. On reopen, the next `INSERT` would then reuse an
  already-in-use row id, silently overwriting the pre-existing row. Fixed by
  reconciling the counter against the actual max row id scanned whenever the
  index-rebuild-on-open path runs without a valid snapshot (which happens
  precisely on a crash reopen, and is a no-op on a clean one).
- **Fix (wire-protocol correctness hazard, pre-existing):** the PostgreSQL
  wire handler intercepted any statement whose raw text merely *contained*
  `version()`, `current_database()`, or `current_user` — anywhere, including
  inside a WHERE clause — and answered with a hardcoded canned reply instead
  of executing the real statement. In the worst case, an `UPDATE`/`DELETE`
  referencing one of these in its `WHERE` clause never executed at all, while
  the client received a reply indistinguishable from a real "0 rows matched"
  result. Fixed by removing the interceptors; the real query engine already
  answers all three correctly and session-aware.
- **`~` `!~` `~*` `!~*` (PostgreSQL POSIX regex match operators) and the raw
  `~~` `~~*` `!~~` `!~~*` operator spellings of LIKE/ILIKE** are now
  implemented — previously errored with "Binary operator not yet supported"
  for every case, including in `WHERE` clauses.
- **`CREATE SCHEMA name CREATE TABLE ... CREATE TABLE ...`** (PostgreSQL's
  multi-element schema-creation form) is now accepted — previously any
  statement with more than one embedded element failed to parse entirely.
  Bare names inside the block (including cross-references between sibling
  tables, e.g. `REFERENCES`/`PARTITION OF`) resolve into the new schema; the
  whole statement is all-or-nothing (a failure partway through leaves nothing
  behind, matching PostgreSQL).

## [4.6.0] - 2026-07-19

Two schema Stage-2 follow-up items (leased to and delivered by the fleet's
HDB session) plus a round-3 cheap-root cascade:

- **PostgreSQL `NaN`/`Infinity`/`-Infinity` `NUMERIC` support** — the three
  special values are now accepted end-to-end (cast, comparison, equality,
  sort, arithmetic, aggregates, predicate pushdown), matching PostgreSQL's
  `numeric.c` contract: `NaN` sorts greater than all non-`NaN` values and
  `NaN = NaN` is `TRUE` (deliberately, for btree/`GROUP BY`/`DISTINCT`/
  `ORDER BY` usability, unlike IEEE-754); `+Infinity`/`-Infinity` compare and
  propagate through arithmetic as PostgreSQL does (`Inf-Inf`, `Inf/Inf`,
  `Inf*0` = `NaN`; `finite/Inf` = `0`; `Inf/0` still raises division-by-zero).
- **`CREATE FUNCTION … RETURNS TABLE(cols)`** — the composite return form is
  now accepted (previously rejected with "Custom data type not yet
  supported"); the column list is not yet persisted and set-returning
  execution is not yet wired (function calls keep existing scalar behavior).
- **Real catalog schema truth (`pg_namespace`/`relnamespace`)** —
  `pg_namespace` lists actual registered schemas (no longer hardcoded to
  `public`); `pg_class.relnamespace` maps each relation to its real schema's
  oid via a stable, deterministic name→oid map, so the two catalogs join
  consistently. `current_schema()` is now session-aware; `current_schemas(bool)`
  added.
- **Multi-entry `search_path`** — `SET search_path TO a, b` now resolves bare
  table references by walking the full ordered list (first match wins),
  replacing the prior first-non-public-entry-only behavior; `SHOW search_path`
  keeps the established append-`public` display convention.
- **Fix (pre-existing):** cross-type foreign keys on the `INSERT ... SELECT`
  path reported phantom violations (the twin of the direct-`INSERT` fix in
  4.5.0); now routes through the same type-aware validator, which also fixes
  deferred-FK skip on that path.
- Measured on the PG regression corpus: +14 statements flip to passing across
  both handbacks, 0 regressions.

## [4.5.0] - 2026-07-18

Real schema support:

- **Schemas coexist** — same-named tables in different schemas are now
  distinct objects (`a.t` and `b.t` no longer collide). Public-schema
  behavior and existing data directories are unchanged (fully backward
  compatible).
- **`search_path` is honored per session** — `SET/SHOW/RESET search_path`
  work over the wire and embedded; bare names resolve to the session's
  schema, falling back to public; connection isolation guaranteed (two
  connections with different search_paths cannot see each other's
  resolution).
- **Schema DDL** — `CREATE SCHEMA [IF NOT EXISTS]` registers (duplicates
  error); `DROP SCHEMA [IF EXISTS] … RESTRICT|CASCADE` (cascade drops member
  tables, composing with the partition-child cascade); `ALTER TABLE … SET
  SCHEMA` moves tables between schemas.
- **Deferred constraints** — `SET CONSTRAINTS {ALL|names} {DEFERRED|
  IMMEDIATE}` implemented (wire + embedded); column-level `INITIALLY
  DEFERRED` honored; deferred FK checks validate at COMMIT and evaporate if
  the referenced table was dropped in-transaction (PG parity).
- **Introspection** — `information_schema` reports real `table_schema`
  values; `pg_class.relname` stays bare.
- **Fix (wrong-data, pre-existing):** the first `INSERT` after any
  `ALTER TABLE … RENAME` silently overwrote the oldest row (volatile row
  counter was not migrated with the rename).
- **Fix (pre-existing):** cross-type foreign keys (e.g. `int8` child
  referencing `int4` parent) reported phantom violations on the indexed
  fast path; probe values now coerce to the referenced columns' types.
- Measured on the PG regression corpus: +158 statements flip to passing;
  ~60 prior "passes" that depended on cross-schema name collisions now
  correctly error (they were silently operating on the wrong tables).

## [4.4.0] - 2026-07-18

Wave-3 implementation (leased to and delivered by the fleet's HDB session) and
round-3 declarative-partitioning Stage 0:

- **PARTITION BY support (Stage 0)** — PostgreSQL declarative-partitioning DDL
  is now accepted: `CREATE TABLE … PARTITION BY RANGE/LIST/HASH` parents
  (including multi-column, expression, and opclass keys), `CREATE TABLE …
  PARTITION OF … FOR VALUES/DEFAULT` children (columns cloned from the
  parent), and `ATTACH/DETACH PARTITION` as accepted no-ops. `DROP TABLE
  parent` cascades to its partition children (PG parity);
  `pg_class.relpartbound` is present (NULL). **Stage-0 semantics**: each child
  is an independent table — INSERT into the *parent* is not routed and SELECT
  from the *parent* does not union children yet (Stage 1). Measured on the
  PG regression corpus: +1,427 statements flip to passing.
- **Version-copy elision** — new `[storage] elide_latest_version` (default
  off): the latest row version lives only in `data:`, cutting single-INSERT
  version-write volume from 65% to 41% of bytes. **One-way door**: once
  enabled on a data dir, downgrade requires dump/restore.
- **Statement retry on write conflicts** — `[locks] statement_retry_max`
  (default 0 = off) auto-retries autocommit statements that hit the typed
  SQLSTATE 40001 write conflict; backoff knobs included.
- **Snapshot schema evolution** — `[storage] snapshot_schema_evolution` now
  defaults to `"null_pad"`: reads through a snapshot taken before an `ALTER
  TABLE ADD/DROP COLUMN` return isolation-correct NULL-padded/truncated rows
  instead of erroring. Set `"strict"` to restore the previous error behavior.
- **`[locks] timeout_ms` is now genuinely wired** to the lock manager
  (previously orphaned); default 1000 preserves the prior effective bound.
- **Fix (durability):** the fast-INSERT row counter is flushed at clean
  shutdown — prevents silent row overwrites on reopen after short sessions in
  relaxed-WAL mode (crash-path reseed tracked separately).
- COPY constraint batching: ART maintenance batched per-table; CHECK
  expressions evaluated via a batched path (constrained-COPY follow-up from
  the Wave-3 measurements).

## [4.3.0] - 2026-07-17

Wave 3 (design-first) of the 2026-07 perf & stability campaign, the July
compat round 2, and a supply-chain hardening pass:

- **Typed write-conflict errors** — same-row write conflicts now surface as a
  typed `WriteConflict` error carrying table/row/holder context and map to
  SQLSTATE **40001** (`serialization_failure`) on the wire (previously a
  generic 25000), so PostgreSQL drivers' retry machinery engages. Timeout and
  locking behavior are unchanged.
- **Performance attribution instrumentation** (all zero-cost when disabled):
  lock-contention census behind the new `lock-census` cargo feature +
  `[performance] lock_census` knob (`heliosdb_lock_census` system view);
  per-statement-class write-volume byte accounting (`[performance]
  write_volume_stats` → `heliosdb_write_volume`); COPY wall-time phase
  breakdown (`[performance] copy_phase_stats` → `heliosdb_copy_phase_stats`).
  Measured verdicts and next-step designs live in
  `docs/plans/PERF_STABILITY_2026_07/W3_*_DESIGN.md` (notable: the version
  chain is 65% of single-INSERT byte volume; CHECK evaluation is 35% of
  constrained-COPY time; the c≥32 wire plateau is not mutex-blocking).
- **Compatibility round 2** (12 fixes + 1 hardening): `CREATE/DROP DOMAIN`
  no-op intercepts, `CREATE TABLE … INHERITS` clause stripping, and other
  pg-dialect gaps; `catch_unwind` isolation extended to the extended-protocol
  DML-RETURNING path (an erroring `UPDATE … RETURNING` no longer kills the
  connection).
- **Supply chain**: the openssl stack is fully out of the default dependency
  graph (`reqwest`/`oauth2` moved to rustls with the system trust store) —
  this also unblocks manylinux Python-wheel builds; the `cargo deny` gate was
  repaired (config had rotted unparseable) and now enforces hard bans on
  openssl/native-tls, an explicit license allowlist, and remediated
  advisories (aws-lc-rs 1.17, bytes 1.12.1, rustls-webpki 0.103.13,
  tokio-postgres 0.7.18, rand 0.8.7, time 0.3.53, crossbeam-epoch 0.9.20).
- **Python wheel**: `heliosdb-nano-embedded` (abi3, manylinux_2_28) now builds
  in CI and publishes to PyPI via the `py-v*` tag lifecycle.
- The CI perf-gate baseline (`benches/public/ci_baseline.json`) was
  regenerated post-W1/W2 — the previous 2026-06-11 baseline sat 1.9–3.9×
  below current throughput, leaving the 2.5× cliff-catcher unable to catch
  anything.

## [4.2.0] - 2026-07-17

Waves 1–2 of the 2026-07 performance & stability campaign
(docs/plans/PERF_STABILITY_2026_07/), every item implemented via a reviewed
subagent workflow and gated per docs/GATES.md:

- **Extended/prepared protocol unlock** — parameterized autocommit reads no
  longer serialize on the session-transaction mutex (atomic fast-out):
  extended point-read **30k-TPS ceiling → 122k @ 64 clients (4.0×)**; prepared
  **34k → 202k @ 32 clients (5.9×)** — the PostgreSQL driver-path gap is
  reversed to ~2.4× in Nano's favor.
- **Extended-protocol Parse reuse** — Describe metadata now derives from the
  shared parameterized plan cache: extended point-read **+10–18% on top of the
  unlock** (c64 115k → 132k TPS). View DDL (CREATE OR REPLACE / DROP VIEW) now
  invalidates the plan cache (stale-Describe fix).
- **COPY fast path for FK/CHECK tables** — bulk loads into constrained tables
  no longer fall back to row-at-a-time validation: **FK+CHECK COPY 100k rows
  3172 ms → 326 ms (9.7×)**, with batched index FK probes (including
  batch-local parents), CHECK parity with the slow path, and single-WriteBatch
  all-or-nothing semantics.
- **Streaming COPY decode with bounded memory** — the wire COPY path decodes
  incrementally across protocol frames (partial-line/quote/UTF-8 state carried)
  instead of buffering the whole stream; new `[server] copy_max_buffered_rows`
  knob (0 = unlimited) aborts cleanly with zero rows applied when exceeded.
- **MVCC bookkeeping diet** — prefix-bounded version scans, epoch-micros
  version timestamps (permanent fallback deserializer keeps old on-disk data
  readable), O(1) snapshot-cache invalidation, and unchanged-value
  index-maintenance elision on UPDATE (PG-HOT-style).
- **In-transaction read watermark** — reads inside a transaction of tables
  unchanged since the snapshot serve from the normal fast read path instead of
  the snapshot scan (30–150×); fail-closed policy (any version-skipping write
  funnel invalidates back to the snapshot path).
- **Fix (branch isolation, wrong-data class):** ~11 DML sites across both
  execution engines maintained the process-wide value index for branch-routed
  writes — phantom UNIQUE violations on main, and branch DELETEs stripping
  main's index entries for inherited rows. All sites now gate on the same
  predicate that routes branch data.
- **Fix:** SQL/plan/result caches and the row cache survived `USE BRANCH` and
  could serve another branch's data; both are invalidated at branch switch.
- **Fix (planner):** `ORDER BY` over grouped plans uses the select-list
  aggregate rewrite (was fragile alias-position slicing).
- **Fix:** `TRUNCATE` reports no affected-row count (PostgreSQL parity);
  `ALTER TABLE` bumps the schema generation so stale fast-path caches cannot
  leak backfilled values to open snapshot readers.

## [4.1.0] - 2026-07-06

Next performance batch (all gated through regression + scalability suites):

- **COPY → PostgreSQL parity** — the time-travel COPY fast batch now writes one
  durable `vmeta:` range marker per batch instead of a per-row `v:`/`v_idx:`
  version pair. **COPY 100k: ~397 ms → ~160 ms (2.5×), near PostgreSQL parity
  (~115–133 ms).** AS-OF visibility and insert-version materialization are wired
  at every mutation path (fast update/delete, general branch-aware, the txn
  commit-apply, and both TRUNCATE arms); an in-memory interval index is rebuilt
  from `vmeta:` at open (crash-safe) behind a one-atomic fast-out. Kill switch
  `HELIOS_COPY_VRANGE_OFF=1`. (PR #13)
- **Normalizer widening** — `IN` / `BETWEEN` / cast literals now parameterize for
  shared cached plans, with IN-list power-of-two arity padding; `IN` on an
  indexed column gained an index multi-probe path (was a full filter-scan),
  measured **100–161× on the wire**. (PR #12)
- **Fix (embedded):** `$n` placeholders inside a scalar subquery in
  `UPDATE … SET n = (SELECT … WHERE a=$1) …` now bind (previously
  "Parameter $1 not provided"). Wire path was never affected. Reported by a2h. (PR #14)

Also investigated and **stopped with evidence** (no code shipped): columnar OLAP
activation (#3 — shipped kernels measured best 3.5× / median 1.0× vs the
already-fast row store) and aggregate-over-join column pruning (#4 — pruning is
correct but the row-store scan full-decodes the blob, so it's a no-op; the real
lever is projected fast-skip decode in the join-input scan, deferred). See
`docs/plans/NEXT_PERF_BATCH_2026_07.md`.

## [4.0.0] - 2026-07-05

Major release: the **2026-07 performance & stability campaign**. Six milestones,
each shipped through independent regression + scalability gates. The headline is
a reversal of the one workload PostgreSQL used to win — indexed point-reads now
run **1.7×–2.3× faster than PostgreSQL 18.4** at every concurrency (they were
*losing* ~2× before). See
[`docs/benchmarks/heliosdb-nano-vs-postgresql-2026-07-05.md`](docs/benchmarks/heliosdb-nano-vs-postgresql-2026-07-05.md).

Major version because several defaults and behaviors change (see **Changed** /
**Behavior changes**).

### Performance

- **Indexed point-read: reversed to 1.7×–2.3× PostgreSQL** (was PostgreSQL-won
  ~2×; Nano saturated ~48k TPS, now scales to ~172k). Two levers: a
  cache-admission filter that stops unique-literal queries from churning the
  plan/result caches, and **token-level literal normalization** — repeated point
  reads that differ only in their WHERE literals now share ONE cached
  parameterized plan instead of re-parsing and re-planning every statement. A
  differential oracle (raw-SQL execution == normalized+parameterized execution,
  row-for-row) and the pg35 benchmark-of-record (35 categories, 35–0–0 vs
  PostgreSQL) prove correctness. Kill switch: `NANO_DISABLE_QUERY_NORMALIZATION`.
- **COPY bulk-load: 5.4× faster** (100k rows ~2.3 s → ~423 ms), closing the gap
  to PostgreSQL from ~20× to ~3×. COPY now applies the whole load as one atomic
  batch through the fast insert-batch machinery.
- **`nextval`-bound INSERT: ~32×** (~60 → ~2,000 TPS). Default sequences reserve
  a block of 32 per durable fsync (was one fsync per value), matching
  PostgreSQL's `SEQ_LOG_VALS=32` durability granularity.
- **Durable-write throughput +11–63% at 16–32 threads** (group-commit
  accumulation window 200 µs → 1000 µs), with lower p50 commit latency.
- Sharded row cache (16 partitions, lock-free write-stats) removes the
  single-global-lock convoy at the commit-time invalidation fence.

### Fixed

- **`ALTER TABLE … RENAME` server-wedge:** renaming a populated table hung 15+
  minutes (non-cancellable, ~2 WAL-fsyncs/row) and left a torn split-table on
  kill; it was also never WAL-logged, so recovery resurrected renamed-away
  tables. Now one atomic WriteBatch (~1.3 s for 50k rows) with proper WAL
  logging; also fixed a latent double-encryption of moved rows.
- **Pre-auth remote-OOM vector:** a client claiming a ~2 GiB message length
  forced an unbounded/2 GiB allocation before authentication. Frontend messages
  are now capped (64 MiB) and startup packets (1 MiB), validated before
  buffering; malformed Parse/Bind/Execute/CopyData frames return protocol errors
  instead of panicking.
- **HNSW recall bug:** an early-inserted, high-level vector could be
  layer-0-isolated and silently dropped from k-NN results (also the source of a
  release-gate test flake). A brute-force rescue guarantees correct recall on
  small indexes.
- **Recursive-CTE table shadow:** `WITH x AS (SELECT … FROM x)` where a table `x`
  exists now correctly reads the table (PostgreSQL semantics) instead of
  auto-recursing to 0 rows.
- **UUID equality** binary parameters (untyped OID) now use the index probe
  instead of a full scan.
- **WAL replay** now tolerates a torn tail (keeps the valid prefix) instead of
  discarding all recovered entries on one bad record.
- **`SET statement_timeout`** / configured `statement_timeout_ms` is now enforced
  (was accepted but ignored — a runaway query could pin a worker).
- **Recursive CTEs** gained a cumulative-row cap and O(1) fixpoint dedup (was
  O(n²), unbounded memory).
- MySQL listeners now enforce `max_connections` and a per-connection
  prepared-statement cap; accept loops back off on fd exhaustion.

### Changed / Behavior changes

- **COPY is now atomic** (all-or-nothing) rather than committing per 500-row
  chunk, and participates in an enclosing transaction (`BEGIN; COPY; ROLLBACK`
  no longer leaks rows).
- **Default sequences reserve blocks of 32** — a crash/restart may skip forward
  by up to 31 (never backward, never reuse). Use `CACHE 1` for the old
  gapless-per-value behavior.
- A deterministic SELECT warms its plan/result caches on its **second** execution
  (cache-admission filter), not its first.
- `group_commit_window_us` default 200 → 1000 (affects `durable_commit=true`).

### Deferred (documented follow-ups)

`durable_commit` and `version_retention` defaults are unchanged and flagged for a
product decision; per-connection wire `SET statement_timeout`, portal streaming,
and index-def cross-version migration are tracked in
`docs/plans/PERF_STABILITY_2026_07/`.

## [3.60.9] - 2026-06-30

Patch release: three Any2HeliosDB (a2h) Oracle→HeliosDB export-compatibility
fixes — Nano now tolerates three more migrated-DDL/SQL shapes.

### Fixed

- **A trailing comma before the closing `)` of a CREATE TABLE column/constraint
  list is now tolerated** (a2h export #1). Oracle-style exports emit
  `CREATE TABLE t ( …, PRIMARY KEY (id), )`; sqlparser 0.53 (like PostgreSQL)
  rejects the dangling comma. A CREATE-TABLE-scoped, quote-aware pre-parser
  rewrite strips a comma immediately before `)`. Scoped to CREATE TABLE, so
  `INSERT … VALUES (..),(..)`, `NUMERIC(p,0)` scale specs and string literals
  are unaffected.
- **`INTERVAL 'N' YEAR` / `INTERVAL 'N' MONTH` (and the `'N years'` /
  `'N months'` string forms) now lower** instead of erroring "Unsupported
  interval field" (a2h export #2). Nano stores intervals as a single i64
  microsecond count with no calendar component, so YEAR and MONTH are
  **approximated** to 365 and 30 days respectively. Imprecise across leap years
  and variable-length months (`DATE '2020-01-01' + INTERVAL '2' YEAR` lands on
  2021-12-31) but unblocks migrated date arithmetic; DAY/HOUR/MINUTE/… stay
  exact.
- **An Oracle-style self-referencing CTE without the `RECURSIVE` keyword now
  resolves** (a2h export #4). Oracle infers recursion and omits `RECURSIVE`;
  Postgres/sqlparser require it, so `WITH t (…) AS (… FROM t …)` failed with
  "Table 't' does not exist". The planner now detects a CTE that references its
  own name as a table in its body and treats the whole WITH as recursive
  (registering the CTE name into scope before planning its body) — both as a
  bare query and wrapped in CREATE VIEW.

### Note

a2h export deficiency #3 (view dependency ordering) is an export-side concern,
not a Nano fix: PostgreSQL and HeliosDB both require a view's referenced
relations to exist at CREATE time, so the export must emit views in topological
(dependency) order.

## [3.60.8] - 2026-06-30

Patch release: three embedded-engine correctness fixes surfaced by Any2HeliosDB
(a2h) dogfooding Nano as a manifest store and running an Oracle→HeliosDB migrate.

### Fixed

- **UPDATE / DELETE / SELECT on a COMPOSITE PRIMARY KEY no longer silently match
  0 rows when the WHERE constrains only a leading prefix of the key** (BUG F). A
  table with `PRIMARY KEY(a, b)` marks *both* columns `primary_key = true`; the
  single-value PK-index fast paths *and* the executor's `get_row_by_pk` point-
  lookup optimization (`try_extract_pk_value`) treated the first PK column as the
  complete key and probed the grouped composite index with a one-value key, which
  can never match. `DELETE FROM t WHERE a = ?` and `UPDATE t SET … WHERE a = ?`
  reported 0 rows while the rows remained, and a cold fast `SELECT … WHERE a = ?`
  could return `[]`. Five spec/optimization sites now decline a composite PK and
  fall through to the planner's scan + filter, which evaluates the predicate per
  row and matches every prefix row. Single-column-PK fast paths (the hot OLTP
  shape and point-lookup benchmarks) are unaffected. a2h's `reset_run`
  (`DELETE FROM chunks WHERE run_id = ?`) now works.
- **`execute_many` (and multi-row `VALUES` INSERT / COPY) no longer falsely
  rejects composite-DISTINCT keys as a UNIQUE violation** (BUG G). The intra-
  batch duplicate check derived its key specs from per-column `primary_key` /
  `unique` flags, fragmenting a composite `PRIMARY KEY(a, b)` into two single-
  column specs and rejecting e.g. `(r1,c0),(r1,c1),(r2,c0)` because rows share a
  column value. It now sources the specs from the grouped PK/UNIQUE ART indexes
  (the same authoritative source the single-row check uses), so distinct
  composite keys all insert and a genuine duplicate composite key is still
  rejected without leaving partial rows.
- **`<col> IS [NOT] JSON` (SQL:2016 / Oracle predicate) now parses** (BUG H).
  sqlparser 0.53 has no `IS JSON` support, so an Oracle→HeliosDB migrate emitting
  `CHECK (mfa IS JSON)` failed at parse time. A quote-aware pre-parser rewrite
  lowers `<col> IS JSON` → `json_valid(col)` and `<col> IS NOT JSON` →
  `(NOT json_valid(col))`. The new NULL-propagating `json_valid()` function
  returns NULL for a NULL input, so inside a CHECK (enforced per-row) a NULL value
  is treated as satisfied — exactly as real `IS JSON` behaves — and a migrate
  never spuriously rejects a NULL row.

## [3.60.7] - 2026-06-28

Patch release: two concurrency/availability fixes found by the Any2HeliosDB
Pagila→Nano work. See `docs/NANO_CONCURRENCY_LOCKING.md`.

### Fixed

- **A single same-row write conflict no longer stalls the whole server for 60s**
  (BUG A). The pessimistic row-lock acquire is a synchronous spin that was
  bounded by a hard-coded 60-second timeout; while a second writer waited on a
  contended row, unrelated statements and even new-connection startup stalled,
  and — because the lock holder's own COMMIT cannot make progress until the
  waiter gives up — the wait could only ever end in a timeout (waiting is futile
  for a write-write conflict). `LockManager::with_default_timeout()` now honors
  `NANO_LOCK_TIMEOUT_MS` and defaults to **1000 ms** (was 60 000), so a conflict
  fails fast with a retriable serialization/lock-timeout error. Non-conflicting
  concurrent writes (different rows) were never affected. This is a mitigation;
  the redundant pessimistic lock should be dropped in favor of the existing
  optimistic first-committer-wins registry (documented as Option 2).

- **`DROP TABLE` of a populated table was O(rows) `fdatasync` calls** (BUG B).
  `catalog.drop_table` deleted data rows one at a time, and each delete appended
  a WAL entry with a synchronous `fdatasync` — so a 200-row table took ~8s and a
  Pagila-sized table appeared to hang and monopolized the WAL writer (stalling
  other sessions, which read as "DROP wedges the whole server"). The drop is
  already WAL-logged as a single DDL op (replayed on recovery/replication), so
  the per-row WAL entries were redundant. Data rows are now removed in one
  batched RocksDB write — O(1) fsyncs. Regression: `tests/b_drop_table_batched.rs`.
  (Note: Nano still permits dropping an FK-referenced parent table — a
  deliberate divergence from PostgreSQL that Any2HeliosDB's `drop_existing`
  relies on; see the doc.)

## [3.60.6] - 2026-06-28

Patch release: two silent data-correctness fixes found by the Any2HeliosDB
Oracle/PostgreSQL→Nano CDC build.

### Fixed

- **DELETE/UPDATE by a `DECIMAL`/`NUMERIC` primary key silently matched 0 rows**
  (BUG D). `DELETE FROM t WHERE dec_pk = 6` (and the equivalent `UPDATE`) removed
  nothing while the identical `SELECT` returned the row; integer PKs were
  unaffected. The fast-DML PK path (`fast_parse_one_value`) parsed a numeric
  literal for a `NUMERIC` column as `Int8`/`Float8`, whose ART-index key
  encoding (sign-flipped 8-byte int) never matched the stored `Numeric("6")` key
  (the bytes of the decimal string), so the point lookup missed every time and
  the statement reported 0 affected rows. The parser now yields `Value::Numeric`
  for `NUMERIC` columns; the slow `get_row_by_pk` path additionally coerces an
  integer literal to the column's numeric type (`coerce_pk_value`). Impact:
  Oracle `NUMBER(p,0)` PKs map to `DECIMAL(p,0)`, so CDC delete-reconcile (and
  any keyed DELETE/UPDATE) silently no-oped on those tables — a data-drift bug.
  Regression: `tests/d_decimal_pk_dml.rs`, `coerce_pk_value_int_to_numeric`.

- **`BYTEA` corruption via Python/psycopg2 drivers** (BUG E). Nano did not
  advertise `standard_conforming_strings` in its startup `ParameterStatus`, so
  psycopg2 (and other drivers) saw it as unset, assumed the legacy off behaviour,
  and **doubled every backslash** in bytea/text literals (`'\x00..'::bytea` →
  `'\\x00..'::bytea`). Nano's (correctly conforming) lexer then decoded the
  doubled form via bytea *escape* format into the wrong bytes — silently
  corrupting `BLOB`/`RAW` round-trips (Oracle blobs with embedded NULs were the
  visible symptom). Nano now advertises `standard_conforming_strings=on` at
  connect, so drivers emit single-backslash literals that decode correctly
  (verified end-to-end with `psycopg2.Binary`). Additionally, a text-format
  `bytea` bind parameter (OID 17) is now hex-decoded to raw bytes instead of
  falling through to a string. **The bytea result-row encoder also now emits
  the `\x<hex>` text format instead of the raw bytes** — raw bytes made
  libpq/psycopg2 un-escape the field and drop any `0x5C` (backslash) byte, so a
  blob containing a backslash lost it on read-back (storage was always intact).
  Regressions: `decode_text_bytea_param_preserves_embedded_nul`,
  `bytea_text_output_is_hex_not_raw_bytes`.

## [3.60.5] - 2026-06-28

Patch release: a `timestamptz`→`TIMESTAMP` cast-leniency fix, found by the
Any2HeliosDB PostgreSQL→Nano CDC work.

### Fixed

- **String→`TIMESTAMP` cast now accepts a trailing timezone offset.** Nano
  downgrades `TIMESTAMP WITH TIME ZONE` to a plain `TIMESTAMP`, but the two write
  paths disagreed: bulk `COPY` of a Postgres `timestamptz` literal like
  `2026-06-28 05:52:42.692688+00` was tolerated while the same value via
  `INSERT`/`INSERT … ON CONFLICT` (and `::timestamp` / `::timestamptz`) failed
  with `Cannot cast '…+00' to TIMESTAMP: trailing input`. So a row could load via
  `migrate` (COPY) yet the identical value fail via CDC apply (literal upsert).
  The cast now parses Postgres' space-separated offset form (`%#z`, with or
  without fractional seconds) in addition to RFC3339, **accepts the zone and
  drops it, keeping the written wall-clock** (matching Postgres `::timestamp`).
  All offset-bearing forms now drop the zone *uniformly* — previously the RFC3339
  branch UTC-converted, so `+05:30` shifted while `-08` did not. Offset-less
  values are unaffected. Regression test:
  [`test_timestamptz_offset_cast_drops_zone`](src/sql/evaluator.rs). Found by the
  Any2HeliosDB CDC build (a2h has a source-side workaround; this fixes the
  COPY-vs-cast inconsistency itself).

## [3.60.4] - 2026-06-26

Patch release: fixes a dropped `WHERE` filter on `information_schema.columns`
over the wire, found by the Any2HeliosDB v1.0.0 re-validation on v3.60.3.

### Fixed

- **`information_schema.columns` `WHERE table_name=…`/`column_name=…` filters
  written without spaces around `=` are now honored.** The wire catalog handler
  extracted the filter with a hard-coded spaced pattern (`"table_name = '"`), so
  the no-space form `table_name='x'` that psycopg / ORMs actually emit matched
  nothing — the filter was silently dropped and the handler returned *every*
  table's columns. A client running
  `SELECT column_default FROM information_schema.columns WHERE table_name='t' AND
  column_name='id'` then `fetchone()` read back the first table's first defaulted
  column — e.g. a different table's `nextval('…_seq')` default (the a2h v3.60.3
  report: `harden_t.id` read back `actor`'s sequence). The extractor is now
  whitespace-tolerant (`col='x'`, `col = 'x'`, `col= 'x'`, `c.table_name='x'`)
  with an identifier-boundary guard, and the handler now also applies a
  `column_name='…'` equality filter so such a query returns exactly the requested
  column. **The stored defaults were always correct** — this was a
  catalog-readback filter bug only (functional `DEFAULT nextval(...)` was
  unaffected). Regression tests in
  [`src/protocol/postgres/catalog.rs`](src/protocol/postgres/catalog.rs)
  (`test_extract_eq_filter`, `test_information_schema_columns_filter_distinguishes_tables`).

## [3.60.3] - 2026-06-26

Patch release: a **`ROLLBACK TO SAVEPOINT` correctness fix**. A row written
after a savepoint left a ghost secondary/PK-index entry that `ROLLBACK TO
SAVEPOINT` failed to remove, so it survived a subsequent `COMMIT`. Found while
root-causing the one category the `pg35` benchmark classified as a PostgreSQL
win (*Prepared stmts*) — the cause turned out to be this real bug, not a
measurement artifact.

### Fixed

- **`ROLLBACK TO SAVEPOINT` now reverts eager index maintenance.** Secondary-
  and primary-key index updates are applied eagerly at statement time and undone
  via a per-transaction undo log that *full* `ROLLBACK` replays. `ROLLBACK TO
  SAVEPOINT` reverted the staged write set (row data) but **not** that undo log,
  so an `INSERT`/`UPDATE`/`DELETE` made after a savepoint left the in-memory
  indexes inconsistent with the rolled-back data: a post-savepoint `INSERT`
  persisted a ghost PK-index entry after `ROLLBACK TO SAVEPOINT` + `COMMIT`, and
  the next insert of that key then failed with a spurious duplicate-key error.
  Savepoints now snapshot the undo-log position and replay exactly the index ops
  staged after the savepoint. Covers all three undo kinds (insert/update/delete),
  nested savepoints, and per-session transactions. Regression tests:
  [`tests/savepoint_rollback_regression_tests.rs`](tests/savepoint_rollback_regression_tests.rs).

### Benchmarks

- **`pg35` *Prepared stmts* flips from a classified PostgreSQL win to a decisive
  Nano win** (~2.7µs vs PostgreSQL's ~670µs, **≈250× faster**). The benchmark's
  *Transaction ctl* category exercises `BEGIN / SAVEPOINT / INSERT / ROLLBACK TO
  SAVEPOINT / COMMIT`; under the bug the post-savepoint insert's ghost index
  entry made every later iteration's insert fail, which left the embedded
  connection wedged inside an open transaction for the remainder of the run. An
  open transaction disables the ART point-lookup fast path, so *every* later read
  — including the *Prepared stmts* `EXECUTE` — fell back to a slow scan
  (~650µs/query instead of <1µs). With the savepoint fix the connection is never
  wedged and the prepared path measures at its true cost. The only remaining
  non-Nano categories are the near-parity joins (INNER / 4-table), which hover at
  ~1.0× and remain shared-host-noise-dominated. See
  [`docs/benchmarks/PG35_BENCHMARK.md`](docs/benchmarks/PG35_BENCHMARK.md).

## [3.60.2] - 2026-06-26

Patch release: low-risk, plan-identical allocation and planning-cost reductions
from a pg35-targeted profiling pass. **No behavior or query-plan changes**, and
the pg35 scoreboard is unchanged — these trim constant-factor cost and variance,
not the structural gaps in the two near-parity categories (Prepared stmts,
4-table JOIN), which need deeper work and a dedicated benchmarking host to move.

### Performance

- **Trace-control statement gate.** A cheap ASCII first-byte check skips the
  uppercase allocation for any statement that cannot be a `SET` / `SHOW` /
  `RESET` trace control — removing one allocation from every other statement on
  the simple-Query path (and several per prepared-statement iteration).
- **Allocation-free fast-prepare header scan**, and **cold-path optimizer
  reuse**: the stateless rewrite-rule set + empty statistics catalog are built
  once and shared via `OnceLock` instead of per query, and the plan cache shares
  an `Arc` on the cold-miss path. Produced plans and results are byte-identical.

## [3.60.1] - 2026-06-26

Patch release: catalog readback of column `DEFAULT` expressions, found by the
Any2HeliosDB Pagila PostgreSQL→Nano migration (the v3.60.0 sequences otherwise
passed end-to-end: tables + data + FKs + indexes + sequences + views).

### Fixed

- **Column `DEFAULT` expressions now read back as SQL text.** Defaults are
  stored as a serialized `LogicalExpr` (for evaluation); introspection now
  renders them back to SQL for pg_dump / ORM round-trips:
  - `information_schema.columns.column_default` returns the real default
    (e.g. `nextval('actor_actor_id_seq')`, `5`, `'hi'`). The PostgreSQL-wire
    `information_schema.columns` view was also missing the `column_default`
    column entirely (so the projection mis-resolved to the table name) — it is
    now present.
  - `pg_attrdef` emits a row for every column with a default — both `IDENTITY`
    columns and explicit `DEFAULT` columns (previously only `IDENTITY`), so
    `pg_get_expr(adbin, adrelid)` returns the rendered expression instead of
    `NULL`.

  The default's *evaluation* was always correct (an INSERT auto-increments); only
  the introspected text was wrong. Functional migration was never blocked.

## [3.60.0] - 2026-06-26

Minor release: durable + scalable **sequences** (introspection catalogs, ALTER
SEQUENCE, owned SERIAL/IDENTITY, cached-block `nextval`), connection-pool /
proxy wire capabilities, an index-durability hardening pass, and join/scan
performance polish. All changes are additive, opt-in, or correctness fixes; the
default single-statement OLTP path and the PostgreSQL-comparison benchmark suite
are unchanged.

### Added

**Durable + scalable sequences.** `CREATE SEQUENCE` / `nextval` / `currval` /
`setval` are now backed by a persistent catalog instead of a process-global
in-memory counter, and the full option set is honored:

- **Durability.** A sequence's definition and counter survive a restart. A
  cached-block `nextval` reserves `CACHE` values per single durable fsync and
  serves the rest lock-free; only the block high-water mark is persisted, so a
  crash leaks the unused tail as a gap (exactly like PostgreSQL/Oracle — SQL
  sequences are explicitly not gapless). The high-water is fsynced **before**
  any value in the block is served, so a crash never re-issues a value — no
  duplicates, for any cache size, ascending or descending.
- **Full semantics.** `START` / `INCREMENT` / `MINVALUE` / `MAXVALUE` / `CACHE`
  / `CYCLE` are all enforced on an `int8` domain (previously `MINVALUE` /
  `MAXVALUE` / `CACHE` / `CYCLE` parsed but were dropped). `CYCLE` wraps at the
  bound; `NO CYCLE` raises PostgreSQL's "reached maximum/minimum value" error on
  overflow (via `checked_add`, never a panic).
- **`ALTER SEQUENCE`** is implemented (previously a parse error): `RESTART
  [WITH n]`, `INCREMENT BY`, `MINVALUE` / `MAXVALUE`, `CACHE`, `[NO] CYCLE`,
  `AS <int type>`, and `OWNED BY t.c | NONE`, in any clause order, with
  `IF EXISTS`. A leading `SET` before a clause is tolerated for migration
  tooling.
- **Introspection.** `pg_sequences` and `information_schema.sequences` now list
  real `CREATE SEQUENCE` objects with their live metadata (they were empty,
  which broke sequence discovery for migration tools and ORMs); `pg_class`
  reports `relkind = 'S'` for sequences; and `pg_get_serial_sequence(table,col)`
  resolves a `SERIAL` / `IDENTITY` column's sequence name. `DEFAULT
  nextval('seq'::regclass)` column defaults read and evaluate correctly.
- **`SERIAL` / `IDENTITY`** columns are discoverable as owned sequences while
  still incrementing via the durable per-table row-id counter (the INSERT hot
  path is unchanged).

- **`DISCARD ALL`** (and `DISCARD PLANS | SEQUENCES | TEMP`) resets session state
  — open transaction, prepared statements + portals, and session GUCs — without
  a reconnect, so a connection pool can safely hand a physical connection to a
  different client. Committed data is untouched.
- **`SET helios.fast_autocommit = on|off`** (default `off`): a per-session GUC
  selecting non-blocking autocommit (commits visible immediately, durable at the
  next group flush). An explicit `synchronous_commit` override still wins.
  `SHOW` / `RESET` and the extended-protocol SET path are supported.
- **`helios.*` capability advertising**: the server emits a `ParameterStatus`
  for each opt-in capability (`helios.copy`, `helios.pipeline`,
  `helios.plan_cache`, `helios.binary_results`, `helios.reset_session`,
  `helios.fast_autocommit`) at startup, and re-emits one (GUC_REPORT) when a
  `helios.*` GUC changes — so a capability-probing client (HeliosProxy) enables
  each feature only when the connected server advertises it.

### Fixed

- **Index-definition persistence is resilient to a single bad record.** The
  startup index rebuild no longer aborts wholesale when one persisted index
  record is undecodable — it skips that record (with a warning) and rebuilds the
  rest, instead of silently leaving the whole database un-indexed. Persisted
  records now carry a format-version tag so a future format change is detected
  and skipped/migrated rather than failing every index. A secondary index that
  fails to re-register on open is surfaced at `warn` level instead of being
  swallowed at `debug`. (Hardens the cross-version-upgrade path reported from
  ada-core live ops.)

### Performance

- **Index-nested-loop join** no longer deep-copies each right row it fetches: the
  inner fetch now hands out a shared (`Arc`) row, removing one `Vec<Value>` copy
  per matched row on the join hot path. The point-lookup path is byte-identical
  (it still returns an owned tuple), so the change is confined to the join inner
  loop.
- Removed a near-duplicate integer-filter scan method; the single-predicate path
  now routes through the multi-predicate one (maintainability; functionally
  identical).

### Verified (no code change required)

- **Pipelined extended-protocol execution** (N `Bind`/`Execute` before one
  `Sync`) already emits exactly one `ReadyForQuery` per `Sync` — never
  per-`Execute`; a wire conformance test now locks that contract.
- The **columnar analytical scan path** (columnar scan operator, zone-map
  predicate pushdown, kernelized aggregates, `O(batches)` `COUNT(*)`) is exercised
  by the columnar-vs-row differential suite.

## [3.58.5] - 2026-06-24

Patch release: a correctness fix for foreign keys added to already-populated
tables (the bulk-migration order), found via the Any2HeliosDB Pagila run.

### Fixed

- **`ALTER TABLE … ADD FOREIGN KEY` on a table that already holds rows** now
  backfills the foreign key's lookup index from the existing rows. The FK
  auto-creates an ART index on the FK column(s), but it was registered empty
  and never populated, so the planner answered `WHERE fk_col = …` (and
  FK-column joins) from an empty index and **silently returned zero rows** even
  though the data was present. Most visible on composite-PK tables whose FK
  references a non-leading PK column (Pagila `film_category` / `film_actor`),
  where multi-table views (`film_list`) came back empty. A full scan
  (`WHERE fk_col + 0 = …`) or rebuilding the index already returned the correct
  rows; now the index itself is correct from the moment the FK is added.

## [3.58.4] - 2026-06-24

Patch release: two more heterogeneous-migration compatibility fixes from the
Any2HeliosDB validation pass (PostgreSQL/Pagila views and the chunked loader's
FK handling).

### Fixed

- **Parenthesized / nested joins** in `SELECT` and `CREATE VIEW` —
  `… FROM ((a JOIN b ON …) JOIN c ON …)` now plans instead of erroring
  "Unsupported table expression: NestedJoin". Migration tools emit left-deep
  nested joins for multi-table views (e.g. Pagila's `customer_list` /
  `staff_list`), so those views now migrate.
- **`ALTER TABLE … DROP CONSTRAINT [IF EXISTS] name [CASCADE]`** is implemented
  (it previously errored "Unsupported ALTER TABLE operation: DropConstraint").
  Drops the named `FOREIGN KEY` / `UNIQUE` / `CHECK` constraint; `IF EXISTS`
  makes a missing constraint a no-op. The resumable loader drops FKs before its
  range-delete + reload pass, so this removes a spurious per-table warning.
- **`GROUP_CONCAT(value)`** (single argument) now defaults the separator to `,`,
  matching MySQL and Pagila's custom `group_concat` aggregate. It previously
  required two arguments (the `STRING_AGG` rule), so views aggregating with the
  1-arg form (Pagila's `film_list` / `nicer_but_slower_film_list`) failed to
  create. `STRING_AGG` still requires an explicit delimiter (PostgreSQL).

## [3.58.3] - 2026-06-23

Patch release: PostgreSQL / heterogeneous-migration compatibility fixes surfaced
by validating Oracle, MySQL, and PostgreSQL (Pagila) → Nano migrations and a CDC
extract→replicat round-trip through Any2HeliosDB, plus a disposable-app
(sakila) hardening pass. All changes are additive or correctness fixes; the
default OLTP path and the PostgreSQL-comparison benchmark suite are unchanged.

### Fixed

- **`NUMERIC` / `DECIMAL` wire type OID**: result columns now advertise the real
  `numeric` OID (1700) instead of 705 (`unknown`). Clients (psycopg, JDBC, …)
  were receiving every numeric column as an untyped string and skipping decimal
  parsing, which broke value-fidelity checks in migration/validation tooling
  (e.g. `NUMBER(10,2)` 98000.0 comparing unequal to a normalized source value).
  `char`/`timestamptz`/`interval` were also mapped off 705 to their canonical
  OIDs.
- **`CHAR(n)` / `CHARACTER(n)` columns**: accepted in `CREATE TABLE` (previously
  "Data type not yet supported: Char(..)"), and values now coerce into them —
  loading into a `CHAR(n)` column failed with "CAST to Char(n) not yet
  implemented". Values are blank-padded to the declared length per PostgreSQL
  `bpchar` semantics. `CHARACTER VARYING(n)` / `NVARCHAR(n)` map to `VARCHAR`,
  and the CLOB forms to `TEXT`. (Pagila `language.name CHARACTER(20)`.)
- **C-style escaped string literals `E'…'`** (and unicode `U&'…'`) are now
  accepted as values (previously "Value type not yet supported:
  EscapedStringLiteral"). psycopg renders `bytea` parameters as escaped
  literals, so CDC replicat's `INSERT … ON CONFLICT … DO UPDATE` upsert of a
  binary column was blocked.
- **`setval(seq, value, is_called)`**: the three-argument form is honored — with
  `is_called = false` the next `nextval` returns exactly `value` (it previously
  ignored the flag and returned `value + increment`). pg_dump emits this form.

## [3.58.2] - 2026-06-23

Patch release: a single PostgreSQL-compatibility fix for quoted identifiers in
`ON CONFLICT … DO UPDATE`.

### Fixed

- **`ON CONFLICT … DO UPDATE SET "col" = EXCLUDED."col"`** now resolves when the
  SET target is a double-quoted identifier. The target column name was taken via
  `to_string()`, which re-emitted the quote characters, so the schema lookup
  failed with `column '"col"' not found`. Clients that always quote identifiers
  (e.g. psycopg's `sql.Identifier`) — and therefore idempotent-upsert / CDC
  workloads built on them — were blocked. Fixed for both `ON CONFLICT DO UPDATE`
  and MySQL `ON DUPLICATE KEY UPDATE`.

## [3.58.1] - 2026-06-18

Patch release: PostgreSQL-compatibility fixes from a 13-item compatibility
report against 3.58.0. All changes are additive or correctness fixes; the
default OLTP path and the PostgreSQL-comparison benchmark suite are unchanged
(Nano still wins 32–33/35 with no envelope erosion).

### Fixed

- **Multi-object `DROP`**: `DROP TABLE a, b CASCADE` (and the view/type forms)
  now drop every named object instead of erroring "Multiple drops not
  supported".
- **`CREATE SEQUENCE` option order**: clauses are accepted in any order
  (`START 100 INCREMENT 10` previously failed because the parser required
  `INCREMENT` before `START`), and `START WITH` / `INCREMENT BY` are now honored
  by `nextval` (defaults remain 1, 2, 3 …).
- **CTE / top-level `VALUES`**: `WITH t (a, b) AS (VALUES (1,'x'),(2,'y')) …`
  now plans (previously "Unsupported set expression"). Bare top-level `VALUES`
  used as a statement is still best addressed via a wrapping `SELECT`.
- **Trigger functions**: `CREATE FUNCTION … RETURNS TRIGGER` is accepted, and
  `BEFORE INSERT … FOR EACH ROW EXECUTE FUNCTION f()` now runs the common
  `NEW.<col> = <expr>; RETURN NEW|NULL` pattern — including expressions that
  reference `NEW.`/`OLD.` columns — to rewrite or skip the row before it is
  written.
- **CHECK-violation message**: now PG-style
  (`new row violates CHECK constraint '<name>' on table '<table>'`) instead of
  dumping the constraint's internal serialized expression.

## [3.58.0] - 2026-06-15

Minor release: PostgreSQL-wire `COPY`, an opt-in `fast_ingest` profile, exposed
RocksDB write-path tunables, and a batched code-graph embedding path. Everything
here is additive or opt-in — the default OLTP path and the PostgreSQL-comparison
benchmark suite are byte-unchanged. This is a feature release, not an ingest
performance release; see `docs/PERFORMANCE.md` for honest strengths and limits.

### Added

- **`COPY` over the PostgreSQL wire**: `COPY … FROM STDIN` and `COPY … TO STDOUT`
  in both text and CSV formats — compatible with `psql \copy` and PG→Nano bulk
  migration. Binary format (`WITH (FORMAT binary)`) is not yet supported and
  returns a clear `0A000` error.
- **`fast_ingest` profile** (`ProfileConfig::FastIngest`): an opt-in bundle of
  regenerable bulk-load settings (async WAL, time-travel off, Lz4 compression,
  larger block cache) plus a code-index override bridge
  (`ProfileConfig::code_index_overrides()`).
- **RocksDB write-path tunables** surfaced as `StorageConfig` fields
  (`rocksdb_write_buffer_size`, `rocksdb_max_write_buffer_number`,
  `rocksdb_max_background_jobs`, `rocksdb_bytes_per_sync`, and related); each
  defaults to the previous built-in value, so existing configs are unaffected.
- **`Embedder::embed_batch`**: a batched in-process embedding path for the
  code-graph `code-embed` flow, replacing the per-symbol serial loop with
  internally chunked batches that bound peak memory.

### Changed

- **Opt-in bulk-load fast paths** (active only under `SET bulk_load_mode = true`):
  the bulk-insert path skips the per-bulk plan-cache clear and uses an in-memory
  row-id counter persisted once at batch end. Default behavior and the standard
  OLTP path are unchanged.

### Fixed

- The `rocksdb` dependency now links `lz4`, so the `fast_ingest` profile's
  `compression = Lz4` opens disk-backed databases cleanly (previously failed at
  runtime with "Compression type LZ4 is not linked with the binary").
- `embed_batch` bounds peak memory via internal 2048-item chunking instead of
  materializing an entire corpus of embeddings at once.
- Indexed nested-loop join correctness: added type-equivalence and
  branch/transaction guards so the fast INLJ path is not taken when the join
  key types differ or when an active branch/transaction requires slow-path reads.

## [3.57.0] - 2026-06-13

Minor release: typed batches, commit-pipeline hardening, MVCC version GC,
durable ART/HNSW index snapshots, ordered index range scans, COUNT/point-lookup
fast paths, and indexed nested-loop join correctness guards.

### Added - R3.4 typed batches

- Typed row batches and columnar sidecars reduce decode overhead on scan-heavy
  paths, with the post-merge gate showing 2-3x scan speedups on targeted
  analytical shapes.
- Kill-switch coverage remains available for typed batch writers, columnar
  pushdown, zone maps, and parallel aggregation so regressions can be isolated
  without removing the feature work.

### Changed - R1.3-p2 commit pipeline

- Group-commit plumbing and a lock-free commit barrier reduce watermark-ledger
  contention while preserving transaction ordering and conflict validation.
- The contended conflict bench passed twice with `lost_updates=0`.

### Added - R4.3 MVCC version GC

- Version-history garbage collection reclaims obsolete row versions while
  preserving snapshot and `AS OF` semantics.

### Added - R4.2/R4.4 durable and range indexes

- ART and HNSW indexes now survive clean restarts through durable index
  snapshots instead of requiring rebuilds.
- Ordered ART range scans support multi-selectivity index probes and top-k
  planning paths.

### Changed - query fast paths

- COUNT and point-lookup fast paths avoid unnecessary cache fills and keep the
  hot read path allocation-light while preserving transaction and materialized
  view fallback semantics.
- Indexed nested-loop joins are enabled for selective indexed equi-joins, with
  correctness guards for branch visibility, transaction-staged writes, and
  type-equivalent join keys. Cross-type joins fall back to the existing
  coercing hash/nested-loop path instead of probing incompatible ART key bytes.
- INLJ materialized output now stamps aliased right-side schemas the same way
  normal scans do, preserving alias-qualified predicates such as
  `LEFT JOIN ... WHERE p.id IS NULL`.

### Validation

- Post-merge gate battery passed: targeted conflict/session/transaction/CRUD
  suites, `cargo test --lib` at 1896/1896, and two contended conflict runs with
  `lost_updates=0`.
- Final release validation passed with lib 1896/0, cross-type INLJ smoke
  matching default-INLJ and `HELIOS_INLJ_OFF=1` row counts, v3.37 A/B accepted
  by the user, and PG35 showing zero PostgreSQL wins across two rounds.

## [3.50.0] - 2026-06-11

Minor release: filtered vector search, Windows compilation, and the
agent-native bundle (R5.V remainder, D6, Windows port).

### Fixed — Vector search

- Indexed kNN with a LIMIT larger than the HNSW search beam could silently
  return fewer rows than available (LIMIT 100 returning 48 with 5,000 live
  vectors); saturated searches now fall back to the exact path.
- `CREATE INDEX ... WITH (m = .., ef_construction = ..)` options were
  parsed and silently ignored; they now reach the index (with `[vector]`
  config defaults) and survive restarts.

### Added — Filtered vector search (R5.V4)

- kNN with simple WHERE predicates (column-vs-constant comparisons, AND
  combinations, parameters) stays on the HNSW fast path via post-filtered
  over-fetch with match-rate-adaptive escalation: 18-102x on selective
  filtered searches. Exactness is guaranteed — any round that cannot
  prove a strict candidate superset hands the query to the exact scan, so
  low-selectivity and fewer-than-k-match filters are always correct.
  Exact record-search distances now use the SIMD kernels.

### Added — Windows (first compile ever)

- All unix-only code paths (unix sockets, daemonization, file permissions)
  are cfg-gated with clear runtime errors on other platforms; the crate
  passes a full windows-gnu cross-check, and the windows-msvc release
  binary is no longer marked experimental — this release's tag is its
  first real CI proof.

### Added — Agent-native bundle (D6)

- `profile = "agent"` configuration bundle; `docs/llms.txt` (single-file
  agent-ingestible reference generated from the 19 skills, with verified
  SQL dialect and vector-operator documentation); MCP documented as the
  canonical agent connection path; database branches repositioned as
  fork-test-discard agent sandboxes with an honest warning on MERGE
  reliability (fix tracked).

## [3.49.0] - 2026-06-11

Minor release: public reproducible benchmarks with a CI performance gate
(D5), the wire-protocol track (R5.W), and a durability-contract fix the
new benchmarks caught.

### Fixed — Durability

- `storage.durable_commit = true` now covers AUTOCOMMIT statements: each
  fast-path statement ends with one WAL fsync barrier. Previously only
  explicit transaction commits were power-loss durable — found by the new
  durability gauntlet showing identical throughput with the flag on/off.
  Default configuration is unchanged (zero overhead with the flag off).

### Added — MySQL prepared statements (R5.W4)

- COM_STMT_EXECUTE decodes binary parameters (full type table: ints,
  floats, decimal, strings/blobs, date/datetime/timestamp/time, JSON,
  BIT, NULL bitmap) and returns binary result rows, as the MySQL protocol
  requires after a prepared execute — previously bound parameters were
  silently DISCARDED and raw SQL replayed. Parameter counting is now
  quote-aware ('?' inside string literals no longer miscounts).
  This unblocks sysbench's default prepared-statement mode and
  libmysqlclient-family drivers.

### Changed — PostgreSQL wire performance (R5.W1, R5.W2)

- Extended-protocol results use the direct row encoder (text formats):
  1.65x on wide result sets; byte-identical output pinned by wire tests.
- Prepared statements pin their plan at first Execute and cache the
  catalog-dispatch decision at Parse (epoch-validated against DDL):
  10-20% on prepared point reads.
- The legacy network stack (accepts any password; drops DML parameters)
  is no longer compiled by default (`legacy-network` feature).

### Added — Public benchmarks & CI gate (D5)

- `benches/public/`: runnable, stamped benchmark scripts (SQLite mirror
  at two scales, durability gauntlet with honest tier labels, concurrency
  suite) and `docs/benchmarks/` with measured results. Headline:
  large-N analytics flipped — GROUP BY 16.1x and aggregates 3.78x FASTER
  than SQLite at 200k rows (both were losses before v3.46); top-N, join,
  and filter-scan losses documented with diagnoses.
- CI performance gate (perf-gate.yml): mem suite + the in-transaction FK
  bulk-insert shape (the v3.28.0 338x regression class), failing PRs at
  >2.5x below baseline.
- Optional p50/p95/p99 in the TPS harness (HELIOS_TPS_PERCENTILES=1).

## [3.48.0] - 2026-06-11

Minor release: STORAGE COLUMNAR becomes production-adoptable (R3.3) and
executor hot-path hygiene lands with two ORDER BY correctness fixes (R3.5).

### Fixed — Query correctness (R3.5)

- Alias-qualified ORDER BY keys on self-joins (`ORDER BY e.id`) could be
  placed where they cannot resolve, silently returning UNSORTED rows; the
  sort key placement now uses the runtime-stamped schema.
- `GROUP BY expr ... ORDER BY <same expr>` left the sort key unresolvable
  (same silent-no-sort outcome); whole-expression matching fixed.
- Sort-key evaluation errors now surface as query errors instead of being
  silently skipped (the mechanism that exposed both bugs above). Queries
  with genuinely invalid sort keys now error rather than returning
  unsorted rows.

### Changed — Columnar storage (R3.3)

- Multi-row writes group columnar values per batch: ONE read-modify-write
  (and one zone-stats recompute) per touched batch per statement instead
  of one whole-batch rewrite per value. Bulk-loading 200k rows into a
  columnar table went from 12.9s to 0.77s (16.8x) — parity with the
  row-store equivalent. The bulk fast-insert path now accepts columnar
  schemas.
- Per-batch presence bitmaps (`colp:` sidecar, transactional, lazily
  backfilled, branch-aware fallback) replace the full row-keyspace
  liveness walk in every columnar scan/aggregate path, including the
  parallel ones; COUNT(*) sums cached live counts. Walk-bound query
  shapes gain 1.1-1.6x; `HELIOS_COLP_OFF=1` restores the old path.
- Also fixes a pre-existing race where concurrent single-value columnar
  stores could lose writes (batch load moved under the stats lock).

### Changed — Executor (R3.5)

- ORDER BY computes sort keys once per row (decorate-sort-undecorate):
  4.6x on expression sorts. Non-equi/nested-loop joins evaluate the ON
  condition without materializing candidate tuples: 3.9-4.3x. Column
  references bind to positions at operator construction (up to 14% on
  sort/filter-heavy shapes); scalar function dispatch stops allocating
  per row; CTE results are shared by reference instead of deep-cloned
  per consumer.

## [3.47.0] - 2026-06-11

Minor release: reads inside transactions stop falling off a cliff (R2.3),
and zone-map pruning goes live for columnar scans (R3.1).

### Changed — In-transaction reads (R2.3)

- Index point lookups, COUNT fast paths, kNN, and aggregate/filter
  pushdowns now stay enabled inside READ COMMITTED transactions for
  tables the transaction has not written — previously ANY open
  transaction (the default for psycopg2, JDBC, npgsql sessions) degraded
  every SELECT to a full scan. Measured: in-transaction point lookups on
  a 100k-row table went from 658ms to 65µs (~10,000x). Safety: READ
  COMMITTED refreshes its snapshot per statement and the v3.39 barrier
  guarantees applied state; REPEATABLE READ/SERIALIZABLE and tables with
  staged writes keep the snapshot-merging slow path (read-your-writes
  unchanged, pinned by tests).
- HAVING no longer disables aggregate pushdown — it runs as a post-filter
  over the aggregate output with identical semantics.

### Changed — Columnar pruning (R3.1)

- Per-batch min/max/null statistics (`colz:` sidecar, written atomically
  with each batch; lazily backfilled for existing data) let columnar
  scans and aggregates skip batches that cannot match a pushed predicate
  — skipping fetch I/O, not just decode. Measured on 500k clustered
  rows: 2%-selective filtered aggregates 60.6ms → 2.15ms (28x); point
  equality 105x; non-matching ranges 112x; full-selectivity queries
  unchanged. Gains require physically clustered data; unclustered
  columns see no change. Disclosed cost: columnar-heavy bulk inserts pay
  ~19% for stats maintenance (the dominant write cost there remains the
  pre-existing whole-batch rewrite, tracked as R3.3).
- `HELIOS_ZONE_MAP_OFF=1` restores the previous read path.

## [3.46.0] - 2026-06-11

Minor release: multicore reads and parallel analytics — the two largest
performance items of the roadmap's concurrency and OLAP tiers (R2.1, R3.2).

### Changed — Multicore reads (R2.1)

- The per-statement query-entry caches (plan, parse, result, and all
  fast-path spec caches — ten in total) moved from single global mutexes to
  16-way sharded LRU caches, the global-transaction check became a
  lock-free atomic, and `query()`'s double transaction-lock acquisition
  (which could surface a spurious "Transaction lock in invalid state"
  error under contention) is gone. Hot point lookups now scale with
  cores: 1.0M lookups/s at 1 thread to 2.58M at 16 threads — previously
  16 threads were SLOWER than one (0.93x). Cache invalidation semantics
  are unchanged and covered by the existing invalidation test matrix.

### Changed — Parallel analytics (R3.2)

- Large GROUP BY / aggregate queries parallelize across cores: the
  row-store keyspace is sharded into contiguous ranges scanned by
  independent RocksDB iterators on a single point-in-time snapshot, with
  per-chunk partial aggregation and explicit accumulator merging
  (COUNT/SUM/AVG/MIN/MAX; DISTINCT stays serial). Measured at 200k rows:
  COUNT/SUM/AVG 41.9ms -> 6.1ms per query (~6.7x), text GROUP BY
  93.1ms -> 7.4ms (~12.5x). Engages at >=65,536 rows
  (HELIOS_AGG_PARALLEL_THRESHOLD); HELIOS_AGG_SERIAL=1 restores the
  serial path. Small queries are unchanged.
- Design note: simple collect-then-parallelize lost 1.8x on these paths
  (iteration dominates, not decode) — the shipped design parallelizes the
  iteration itself.

## [3.45.0] - 2026-06-11

Minor release: index-layer concurrency (R2.2), configuration profiles with
visible-by-default observability (D3), and standard SQLSTATEs (D4).

### Fixed — Configuration (important)

- **`start --config <file>` was parsed and then ignored in server mode** —
  the server always ran on built-in defaults, silently discarding every
  `[storage]` setting since the flag was introduced. The config file is now
  actually applied (CLI `--data-dir`/`--memory` still override). Review
  your config files: settings you believed active may take effect for the
  first time.

### Added — Profiles & observability (D3)

- Top-level `profile = "safe" | "balanced" | "fast"` config bundles
  (explicit `[storage]` keys always win; `safe`/`balanced` never downgrade
  an explicit `durable_commit = true`).
- The slow-query log and other warnings now print without `RUST_LOG`
  (tracing defaults to `warn`).
- A one-line durability-contract banner at startup, derived entirely from
  the effective configuration.

### Added — SQLSTATEs (D4)

- Undefined table/column/function, duplicate table, deadlock, and
  serialization failures now map to their standard codes (42P01, 42703,
  42883, 42P07, 40P01, 40001) with detail/hint populated for the common
  cases — ORMs and drivers stop classifying routine errors as internal
  database failures, and retry loops engage on 40001/40P01. MySQL maps
  deadlocks to 1213/"40001". Covered by unit tests plus end-to-end
  assertions over a real PostgreSQL wire connection.

### Changed — Index concurrency (R2.2)

- ART indexes use per-tree locks behind a shared registry: writers on one
  table no longer block reads or writes on any other table, TRUNCATE and
  rename stopped cloning whole trees, and index-metadata queries
  (including the per-UPDATE index-impact check) are now lock-free. A
  structural lock-ordering invariant (never two tree locks) makes FK
  cycles deadlock-free by construction; covered by a new concurrency
  suite including a mutual-FK stress test.

## [3.44.0] - 2026-06-11

Minor release: opt-in power-loss-durable commits and a high-concurrency
convoy fix (roadmap item R1.3 phase 1).

### Added — Durability

- `storage.durable_commit = true` fsyncs the commit WriteBatch, making
  COMMIT power-loss durable (default remains process-crash-safe). Durable
  commits scale with concurrent sessions — RocksDB's write groups
  amortize the fsync: measured 47 commits/s at 1 session rising to 433/s
  at 32 sessions on an ~21ms-fsync disk. A leader/follower pipeline with
  an accumulation window is planned as phase 2.

### Fixed — Concurrency

- The snapshot barrier introduced in v3.39.0 yield-spun while commits
  were applying; with session counts near the host's core count the
  spinners starved the committing threads — plain disk-mode transaction
  throughput collapsed from ~50k txn/s at 8 sessions to 73 txn/s at 32
  sessions on a 32-core host. Bounded spin with sleep backoff removes
  the convoy (36-51k txn/s flat through 32 sessions).
- New `run_durable_commit_bench` harness (HELIOS_DURABLE=1) covers
  1/8/16/24/32 sessions, durable on and off.

## [3.43.0] - 2026-06-11

Minor release: snapshot metadata diet (roadmap item R1.4).

### Changed — Write path / MVCC

- Snapshot metadata writes one `snapshot:` key instead of three: the
  `txn_map:`/`scn_map:` mappings were write-only (recovery rebuilds every
  in-memory map from the `snapshot:` entries alone), costing two keys of
  write amplification per autocommit statement and two extra RocksDB write
  calls per transaction commit. Garbage collection still removes legacy
  mapping keys from databases written by earlier versions.
- Measured: durable autocommit INSERT +15% (46.2k → 53.0k ops/s on the
  disk suite); time-travel `AS OF` across restart covered by a new
  regression test.

## [3.42.0] - 2026-06-11

Minor release: the plan-arm INSERT fsync fix (roadmap item R1.1) — the
single largest durable-write win since the P0 series.

### Fixed — Write path / durability

- INSERT statements that take the planner path (RETURNING, ON CONFLICT,
  DEFAULT, expression values — the default shapes from SQLAlchemy, Rails,
  Drizzle, and friends) no longer pay up to two fsyncs per row under the
  default `wal_sync_mode = "sync"`. Row ids come from the volatile counter
  (staged in the transaction or flushed with a non-synced put after the
  row write), and plan-arm logical-WAL appends use the established nosync
  gate; strict per-statement durability remains opt-in via
  `storage.logical_wal_per_statement`.
- Measured on an 11ms-fsync disk: `INSERT ... RETURNING` went from
  **34 rows/s to ~9,500 rows/s (~280x)**; in-memory the same shape gained
  ~50% from the removed per-row counter write.
- Crash-recovery contract unchanged: wal_crash_recovery (18) and
  crash_recovery_e2e (4) suites green; counters recover from flushed
  `counter:` keys exactly like the existing fast INSERT path.

## [3.41.0] - 2026-06-10

Minor release: HNSW vector indexes are maintained by SQL DML (roadmap item
R5.V1) — the flagship vector feature no longer serves stale results.

### Fixed — Vector search

- INSERT, UPDATE, and DELETE now maintain HNSW vector indexes at statement
  time with full transactional undo (rollback leaves no phantom vectors).
  Previously an index only ever contained the rows present at CREATE INDEX
  time: rows inserted afterwards were silently invisible to indexed kNN,
  and deleted rows kept being served until a process restart.
- This also fixes two long-standing vector-store API bugs: deleted records
  appearing in unfiltered search and upsert serving the replaced vector.
- Indexed kNN over-fetches past tombstones (escalating to the index's
  physical size) so DELETEs never shrink `LIMIT k` results.
- Small indexes (≤256 live vectors) are answered by the exact scan path:
  tiny HNSW graphs can be weakly connected and miss live rows regardless
  of search effort; approximation now only engages at scales where its
  recall is statistically sound.
- Payload-only updates skip vector-index work entirely; tables without
  vector indexes pay a single cheap gate check per statement.

## [3.40.0] - 2026-06-10

Minor release bundling three independently developed tracks: fast-path
eligibility hardening (R1.2), the front-door truth sweep (D1), and prebuilt
install channels (D2).

### Fixed — Correctness

- Fast-path literal parsers now reject trailing tokens after the parsed
  literal: previously `... WHERE id = 5 LIMIT 1` could silently execute as
  `id = 5` on the fast path (latent wrong-results bug; five sites hardened).
- `--dump-schedule` is a hard startup error instead of a silent no-op that
  let operators believe scheduled backups were running.
- `start --daemon` now forwards `--mysql`, `--mysql-listen`,
  `--mysql-socket`, `--pg-socket-dir`, `--memory`, and
  `--dump-on-shutdown` to the daemonized child (previously dropped:
  a daemon started with `--mysql` ran without a MySQL listener).
- `config.example.toml` parses and boots again: the documented `[audit]`
  section used field names the config loader rejects, and eight documented
  sections were silently ignored phantoms. The example is regenerated from
  the real Config struct and now includes `slow_query_threshold_ms`.

### Changed — Performance (R1.2)

- Fast-path eligibility bail-words are matched as SQL keywords (outside
  string literals, at identifier boundaries) instead of raw substrings:
  statements like `UPDATE t SET x=1 WHERE points = 5`, `WHERE order_id =
  3`, tables named `default_settings`, and literals containing `select`
  now stay on the parse-skipping fast paths (previously 10-50x slower via
  the planner). The UPDATE WHERE-splitter no longer mis-splits on `WHERE`
  inside string literals.

### Added — Install channels (D2)

- Release CI now builds and attaches prebuilt binaries (Linux
  x86_64/aarch64-gnu, macOS arm64, Windows x86_64) with SHA256SUMS,
  publishes a Docker image to ghcr.io, and ships `scripts/install.sh`;
  `cargo binstall heliosdb-nano` metadata added. First-run caveats in
  docs/guides/install-channels.md. musl targets deferred (openssl
  dependency); glibc 2.39 baseline.

### Documentation

- README vector operators corrected to match the planner: `<->` L2,
  `<=>` cosine, `<#>` inner product (the old table mislabeled `<->` as
  cosine and documented a nonexistent `<~>`).
- Stale `docs/BENCHMARK_PG_VS_HELIOS.txt` ("PostgreSQL wins 21/21",
  Feb 2026) deleted; references now point at current measurements.
- Missing [3.37.2] changelog entry reconstructed; upgrade guide no longer
  instructs nonexistent brew/docker channels; README crate pin fixed.

## [3.39.0] - 2026-06-10

Minor release: write-write conflict detection for snapshot-isolation
transactions (roadmap item R0.2). Concurrent transactions that would
silently overwrite each other's committed updates now abort with a
retryable serialization failure.

### Fixed — Transactions / MVCC

- First-committer-wins validation at commit for embedded transactions and
  RepeatableRead/Serializable sessions: the contended-counter benchmark
  went from 3,209 silently lost updates out of 4,000 commits (80%) to
  exactly zero. Losers receive `serialization failure ... retry the
  transaction` — SQLSTATE 40001 over the PostgreSQL wire, 1213/40001 over
  MySQL — so driver retry logic works. READ COMMITTED sessions and
  autocommit statements keep PostgreSQL's blind-write semantics (no new
  errors for existing applications).
- Transaction commits now invalidate the row cache for the rows they
  wrote. Previously a PK point lookup could repopulate the cache with a
  pre-commit value during the staging window and serve stale data to
  later transactions with perfectly valid snapshots.
- Commits use a fresh commit timestamp instead of the BEGIN snapshot
  timestamp, fixing version-history ordering for overlapping transactions.
- Parameterized (extended-protocol) INSERTs inside a transaction stage
  through the transaction write set; previously they wrote directly to
  storage and survived ROLLBACK. FK validation on that path now sees the
  transaction's own writes.
- Payload-only UPDATEs no longer delete and reinsert identical ART index
  entries — eliminating a concurrency window where point lookups could
  briefly miss the row, and removing pure index-maintenance overhead.
- A failed session COMMIT (e.g. a serialization failure) now cleans up
  like ROLLBACK: eager index mutations are undone and the session is
  immediately usable for the retry.

### Performance

- New `run_conflict_bench` harness (HELIOS_CONFLICT=1) reporting lost
  updates, retries, and zero-match anomalies under contention.
- Disclosed cost: explicit write-transaction cycles pay 25-49% on
  microbenchmarks for validation, commit-time cache invalidation, and the
  snapshot barrier (the removed cache hits were serving stale data).
  Bulk insert, autocommit DML, reads, and analytics are unaffected.
  Recovery work is tracked in the roadmap (R2.x).

### Known issues

- SELECT FOR UPDATE remains unimplemented; READ COMMITTED lost-update
  avoidance for racing autocommit read-then-write pairs is therefore an
  application-level concern (as in PostgreSQL without FOR UPDATE).
- Validation details: R0_2_CONFLICT_DETECTION_REPORT.md.

## [3.38.0] - 2026-06-10

Minor release: per-session transactions for the PostgreSQL and MySQL wire
protocols (roadmap item R0.1). Every connection now gets an isolated
transaction context; previously all wire connections shared one
process-global transaction slot.

### Fixed — Wire protocols / transactions

- Fixed cross-connection transaction bleed: statements from one connection no
  longer execute inside another connection's open transaction, and a BEGIN on
  one connection no longer collides with BEGINs on others.
- In-transaction SELECTs over the wire now see the transaction's own
  uncommitted writes (read-your-writes) on both the simple-query and
  extended-protocol (parameterized) paths.
- Rolled-back session transactions no longer leave phantom ART index
  entries, and unrelated rollbacks can no longer un-index committed session
  rows (per-session index undo logs).
- Dropped connections automatically roll back their open transaction and
  release their session.

### Added — Embedded API

- Per-session statement execution surface: `create_wire_session`,
  `session_in_transaction`, `set_session_isolation`, `execute_for_session`,
  `query_with_columns_for_session`, `execute_returning_for_session`,
  `execute_params_for_session`, `query_params_for_session`,
  `handle_transaction_control_for_session`, and a
  `Transaction::session_id()` accessor.
- `BEGIN [TRANSACTION] ISOLATION LEVEL ...` over PG wire maps the requested
  level onto the connection's session (READ UNCOMMITTED runs as READ
  COMMITTED, matching PostgreSQL).

### Performance

- Concurrent wire transactions now scale: BEGIN/INSERT/COMMIT cycles measure
  ~45.6k txn/s on one connection and ~161.8k txn/s aggregate across 16
  connections (previously not correctly runnable above one connection).
- Autocommit DML while another session holds an open transaction stays on
  the fast path when time-travel versioning is enabled (was a global kill
  switch): 12.9k -> 123k inserts/s on the mirrored workload.
- Replaced per-statement session-map shard sweeps in the DML fast-path gates
  with an atomic counter: embedded bulk INSERT +29%, autocommit INSERT +30%,
  UPDATE +67%, DELETE +112% (mem TPS suite, N=10k); batch INSERT +42% in the
  OLTP smoke head-to-head. Validation details:
  `R0_1_SESSION_TRANSACTIONS_REPORT.md` and
  `perf/R0_1_per_session_transactions.md`.

### Known issues

- SAVEPOINT inside a wire (session) transaction is rejected with an explicit
  error; previously savepoints only ever applied to the embedded global
  transaction. Follow-up tracked in the validation report.
- Pre-merge validation surfaced pre-existing failures unrelated to this
  release (reproduced on v3.37.3): one truncate_hardening test, one
  v334_a11 test, two vector_store_api tests, plus environment-sensitive
  hangs in postgres_ssl_tests and pq_storage_integration_test. See the
  validation report for triage notes.

## [3.37.3] - 2026-06-05

Patch target for Token Dashboard TD#8: operator-safe MCP-over-HTTP
non-loopback deployment without requiring a socat bridge.

### Added — MCP / HTTP

- Added `start --http-listen <addr>` so the HTTP health/MCP listener can bind
  separately from the PostgreSQL wire listener configured by `--listen`.
- Added `start --mcp-token <token>` to require Bearer-token auth on MCP HTTP
  routes and permit non-loopback MCP route mounting through the existing bind
  safety policy.
- Added `start --allow-remote-mcp` as an explicit unsafe operator override for
  unauthenticated non-loopback MCP route mounting. Prefer `--mcp-token`.

### Fixed — MCP / HTTP

- Preserved the safe default: non-loopback HTTP listeners still serve
  `/health`, but MCP routes remain unmounted unless MCP auth or the explicit
  unsafe override is configured.

## [3.37.2] - 2026-06-05

Patch release for the ada-core index-persistence and UUID point-lookup
findings (`ISSUE-index-persistence-and-uuid-pointlookup.md`).

### Fixed — Indexes / catalog

- User-created secondary indexes (scalar ART and HNSW variants) are now
  persisted in the catalog as version-portable definitions and rebuilt on
  open, so they survive a restart or binary/version swap on the same
  data-dir. Previously every non-PK index silently vanished from
  `pg_indexes` after an upgrade and queries fell back to full scans.
- `CREATE INDEX` options (e.g. the vector distance metric) are serialized
  into the WAL record and restored on replay, instead of being dropped.
- `pg_indexes` now also lists ART/btree indexes (primary-key, unique,
  manual, and FK-backed) alongside HNSW vector indexes, with the vector
  opclass name (`vector_cosine_ops` / `vector_l2_ops` / `vector_ip_ops`)
  in `indexdef`.

### Fixed — SQL engine / planner

- Parameters with a PostgreSQL type cast (e.g. `WHERE id = $1::uuid`) stay
  on the fast point-lookup path: the fast parameter decoder strips simple
  `::type` casts, and indexed scans resolve bound values through `CAST`
  expressions instead of degrading to a full scan.

## [3.37.1] - 2026-06-05

Patch release for code-graph indexing throughput and HNSW vector-index planner
follow-ups.

### Added — Diagnostics

- Added engine-regression bisect reports and a foreign-key validation
  optimization proposal to preserve the current investigation context.

### Fixed — Code graph

- Batched cross-file code-graph reference rebinding and exposed skip-pass
  options for faster large-repository indexing.

### Fixed — Vector indexes

- Added the HNSW kNN planner fast path and parallel `CREATE INDEX` backfill
  coverage, including a regression test for populated vector indexes.

## [3.37.0] - 2026-06-04

Minor release for Token Dashboard MCP-over-HTTP support and the ada-core HNSW
populated-table index build regression.

### Added — MCP / HTTP

- Daemon `--http-port` now serves the existing JSON health endpoint through
  Axum and mounts MCP-over-HTTP routes (`/mcp`, `/mcp/ws`, `/mcp/sse`,
  `/mcp/info`) when built with the `mcp-endpoint` feature. Non-loopback MCP
  listeners still require explicit auth-safe binding.

### Added — Code embeddings

- SQL now exposes `CODE_EMBED(text)` / `HELIOSDB_CODE_EMBED(text)` when built
  with the `code-embed` feature, returning a vector from the local code
  embedder and a clear feature-gate error otherwise.

### Fixed — Vector indexes / PostgreSQL compatibility

- `CREATE INDEX ... USING hnsw` now backfills vectors already present in the
  target table for standard, quantized, and persistent HNSW indexes, instead of
  creating an empty index on populated data.
- pgvector operator classes such as `vector_cosine_ops`, `vector_l2_ops`, and
  `vector_ip_ops` are preserved as vector-index distance metrics.
- `pg_indexes` now exposes manual scalar secondary indexes and HNSW vector
  indexes over PostgreSQL-compatible introspection, including vector opclass
  names in `indexdef`.

### Fixed — SQL engine / planner

- `ORDER BY` keys that reference columns not present in the final select list
  now sort against the pre-projection input when possible, fixing LEFT JOIN
  ordering cases such as `SELECT c.name ... ORDER BY c.id, o.id`.

## [3.36.2] - 2026-06-04

Patch compatibility release for the ada-core v3.36.1 probe gaps plus the Token
Dashboard `SHOW BRANCHES` regression that remained in v3.36.1.

### Added — PostgreSQL compatibility

- SQL `INTERVAL` literals such as `interval '1 hour'` now plan to Nano's
  existing microsecond interval value and work with timestamp arithmetic
  (`now() + interval '1 hour'`). Month/year-style intervals still require a
  richer interval representation and remain unsupported.
- PostgreSQL array column DDL now accepts forms such as `TEXT[]`, maps them to
  Nano `Array(Text)`, and casts array values element-by-element on insert.
- `ALTER TABLE ... ALTER COLUMN ... DROP NOT NULL` now updates catalog
  nullability metadata for non-primary-key columns, allowing subsequent `NULL`
  writes. (ada-core B3)
- Qualified `ORDER BY` keys above projections now resolve to the projected
  column when the select-list expression matches (for example
  `SELECT a.id ... LEFT JOIN ... ORDER BY a.id`). This fixes ada-core's
  LEFT JOIN keyset monotonicity failure. (ada-core B6)

### Fixed — PostgreSQL wire protocol

- `SHOW BRANCHES` over the PostgreSQL simple-query protocol now bypasses the
  generic `SHOW <parameter>` compatibility handler. v3.36.1 treated
  `branches` as an unknown parameter and returned a single blank row; it now
  routes to the branch-listing query path and returns the actual branch names
  (`main`, `alpha`, `beta`, etc.) on fresh and existing data directories.
  (Token Dashboard #4)
- The extended-query protocol also classifies `SHOW BRANCHES` as row-returning,
  so prepared-statement clients see branch rows instead of a command tag.
- Added fresh data-dir and PostgreSQL wire regressions that assert created
  branch names appear, not just that `SHOW BRANCHES` returns a row.

## [3.36.1] - 2026-06-03

Follow-up release closing the items deferred from the v3.36.0 deficiency batch:
secondary-index point lookups, the Markon scan-path UUID-corruption bug, and
`pg_catalog`/`information_schema` reflection. Additive or corrective only.

### Fixed — Storage

- **Selected-column UUID corruption.** The column prefix decoder read bincode's
  UUID length prefix as UUID payload bytes, so `SELECT id FROM t` could return
  values like `10000000-0000-0000-…` even though the stored data and per-key
  `WHERE id = '<uuid>'` lookups were correct. Originally reported as a
  hash-join row-drop (Markon A5); the real cause was the scan-path decoder.
  Now fixed in `prefix_decode.rs`. (Markon A5)

### Added — SQL engine & planner

- **Secondary-index `=` point lookups.** `CREATE INDEX` now creates and
  backfills scalar ART secondary indexes for the `=`, `USING art`,
  `USING btree`, and `USING hash` forms, and filtered scans / `COUNT(*)` filter
  fast paths use the ART index for equality point lookups (with a
  residual-predicate recheck) instead of an O(table) full scan.
  (NANO-DEFICIENCIES A3)
- Hash-join key hashing/equality now keeps a native `UUID` and a UUID-shaped
  `TEXT` value consistent, and `WHERE uuid_col = 'uuid-literal'` follows the
  same coercion. (Markon A5b)

### Added — Introspection / pg_catalog

- `pg_index` now emits real ART index rows, including manual secondary indexes
  from `CREATE INDEX`; `pg_class` exposes the matching index relations so
  `pg_index.indexrelid = pg_class.oid` joins resolve index names.
- `pg_constraint` emits real PRIMARY KEY / UNIQUE / CHECK / FOREIGN KEY
  metadata (including FK target relation/key fields and referential action
  codes).
- `information_schema.table_constraints` and `key_column_usage` now include
  foreign-key rows in both the planner-backed registry and the pg-wire
  single-view catalog router — unblocking reflection / autogenerate tooling.
  (Markon A4)

## [3.36.0] - 2026-06-02

Deficiency-driven release working three field reports: ada-core's pg-wire
workload (`NANO-DEFICIENCIES.md`), Markon (`HELIOSDB_GAPS_markon.md`), and the
Token Dashboard integration (`HELIOSDB_v3_34_0_OUTSTANDING.md`). Focused fixes
for several silent-wrong-result bugs plus operability knobs, each with
regression coverage.

### Fixed — SQL engine & planner

- kNN `ORDER BY <vector-distance-expr>` now sorts correctly. Two bugs combined:
  the Sort/TopK operators built their sort-key evaluator without query
  parameters (so `ORDER BY embedding <=> $1` could not resolve `$1`), and the
  planner places Sort above Project, so the ORDER-BY expression referenced a
  base column the projection had already dropped — leaving rows unsorted
  (silently wrong nearest neighbors). Parameters are now threaded into Sort/TopK,
  and an ORDER-BY expression that matches a select-list expression is redirected
  to the projected column. The canonical pgvector idiom
  `SELECT id, embedding <=> $1 AS d … ORDER BY embedding <=> $1 [LIMIT k]` now
  returns true nearest neighbors. (NANO-DEFICIENCIES A17)
- `col = ANY($1)` where the bound parameter arrives as a PostgreSQL array text
  literal (`{a,b,c}` — how text-protocol clients such as psycopg send a list)
  now matches like `IN (…)` instead of erroring "ANY expects an array
  expression, got String". (A1)
- `->` and `->>` now accept a TEXT operand that holds JSON and parse it, so
  `col->>'k'` works on JSON-in-TEXT columns without an explicit `::json` cast.
  (Markon A2)
- `CREATE SCHEMA [IF NOT EXISTS] <name>` is accepted as a flat-namespace no-op
  instead of erroring "Statement not yet supported". (Token Dashboard #5)
- `SHOW BRANCHES` is resolved in the planner, so it enumerates branches through
  every query entry point, not only the textual pre-detect path. (Token
  Dashboard #4)

### Fixed — PostgreSQL wire protocol

- A CTE (`WITH … SELECT …`) executed over the wire now returns its rows. Both the
  simple- and extended-query handlers decided whether a statement returns rows by
  a literal `SELECT` prefix, so a `WITH …` query fell through to the command path
  and silently returned a command tag with zero rows — even though the engine
  computed the rows correctly. Reproduced with pg8000 (extended protocol) and
  fixed by routing CTEs through the row-returning path in both handlers. (Token
  Dashboard #3)

### Changed

- New `--max-connections` flag on `heliosdb-nano start` (previously hardcoded to
  100), forwarded through the daemon re-exec. (A9)
- The hash-join build-side memory cap is raised from 100 MB to 1 GB and is now
  overridable at runtime via the `HELIOSDB_HASH_JOIN_MEM_MB` environment variable
  (value in MB); the over-limit error names the knob. (A2)

### Verified — already correct on 3.35.x, regression coverage added

- `INSERT/UPDATE/DELETE … RETURNING` (Markon A3).
- Correlated `EXISTS` / `NOT EXISTS`, `IN` / `NOT IN (subquery)`, nested `IN`, and
  the `LEFT JOIN … IS NULL` anti-join — verified embedded and over the wire.
  `EXISTS`/`NOT EXISTS` evaluate per outer row (not via the hash join), giving a
  reliable anti-join path. (Markon A6)

### Known issues

- Equi-joins can silently drop a small fraction of legitimately-matching rows on
  certain large UUID-keyed datasets (Markon A5). It is data-and-scale-dependent
  and not reproducible from a synthetic snippet, so it is under investigation
  pending a reproducer; until it is resolved, validate via per-key equality
  lookups or `EXISTS`/`NOT EXISTS` (which do not go through the hash join).

## [3.35.0] - 2026-06-02

Hardening release. Two independent coding agents (Claude Code + Codex) worked the
v3.34.0 deficiency checklist item-by-item from a shared baseline — each
baselining, fixing, and regression-testing independently — then integrated the
stronger implementation of each. Of 13 checklist items, **10 were real bugs**
(several deepening fixes first started in 3.34.0) and **3 were already correct**
and gained regression coverage only. Also resolves the HNSW tombstone-count test
deferred from 3.34.0.

### Fixed — SQL engine & planner

- `col = ANY($param)` and `= ANY(ARRAY[...])` now match like `IN (...)` instead of
  returning zero rows. The array expansion is confined to an internal marker, so
  `IN ($1)` with an array parameter keeps its existing semantics. (A1)
- CHECK constraints are now enforced on the parameterized write path
  (`execute_params`), not only on the simple `execute()` path. (A5)
- `COUNT(*)`, `COUNT(<col>)`, and single-PK count fast paths over a materialized
  view now resolve the `__mv_<name>` backing table and return the correct count
  instead of 0. (T2)
- `GROUP BY` without aggregate functions now deduplicates by the grouping key
  (the GROUP BY was previously dropped), affecting both direct queries and
  materialized views. (T8)
- `ON CONFLICT DO UPDATE … WHERE <predicate>` now evaluates the predicate
  (including `excluded.*` references) before applying assignments; a false/NULL
  predicate skips the update instead of always updating. (A11)

### Fixed — PostgreSQL wire protocol

- The extended-query protocol now recovers after an Execute-time error by
  discarding messages until the client's `Sync`, instead of emitting an early
  `ReadyForQuery` that wedged drivers (psycopg/asyncpg). (A8)
- `BEGIN` after an in-transaction error no longer fails with "Transaction already
  active": the aborted engine transaction is rolled back at BEGIN time, and the
  extended path preserves PostgreSQL's 25P02 "transaction is aborted" semantics
  until ROLLBACK. (A14)
- Binary result formats requested via Bind/Describe are now honored on the
  extended path (including uuid/bytea), instead of always emitting text rows. (A15)
- Binary `TIMESTAMP`/`TIMESTAMPTZ` (OID 1114/1184) and `UUID` (OID 2950) parameter
  inputs are now decoded (exact-length, PostgreSQL-epoch conversion) instead of
  falling through to a bytes cast failure. (A4)

### Fixed — Storage

- Long shared-prefix `UNIQUE TEXT` values no longer trigger false duplicate
  errors. The ART index now verifies the hidden path-compression prefix tail
  against a representative leaf rather than trusting the truncated inner-node
  prefix. (A7)
- `CREATE BRANCH IF NOT EXISTS <name>` now parses the real branch name (instead of
  creating a branch literally named `IF`) and is idempotent — a second identical
  statement is a no-op. (Follow-up "IFNE".)

### Fixed — Vector index

- `MultiMetricHnswIndex::len()` / `is_empty()` now report the physical
  (tombstone-inclusive) entry count for all three metrics (L2/Cosine/InnerProduct),
  matching the documented tombstone-on-delete semantics. Resolves the deferred
  `test_vector_count_tracking` failure from the 3.34.0 release gate and clears
  the `cargo test --lib` release gate. Search results are unaffected (deleted ids
  are still filtered out).

### Tests — already-correct items hardened (no source change)

- CTE + `$N` parameter binding, including recursive / UNION / joined / multi-param
  forms. (T3)
- `CREATE BRANCH … AS OF NOW` / `AS OF TIMESTAMP` name recording across all forms.
  (T4)
- `IS DISTINCT FROM` / `IS NOT DISTINCT FROM` NULL truth table, parameter, and
  coercion behavior. (A10)

### Known deferred

- A4 output-side binary *encoding* for non-scalar types (numeric/temporal/json)
  remains text-only.
- TRUNCATE affected-row count semantics (carried from the 3.34.0 release gate).

## [3.34.0] - 2026-05-31

### Performance — TPS release batch

- Added fast-path DML and executor improvements that move the default release
  substantially closer to the revised TPS goal. The current measured Docker
  PG-wire mirror has Nano ahead of PostgreSQL and MariaDB on the repeated
  write, lookup, and read/analytics shapes. SQLite remains the hard comparison
  for default embedded in-memory analytics.
- Added fast SELECT routing for `query_with_columns()`, shared deterministic
  result-cache reuse for PostgreSQL simple-query reads, no-clone cached-row
  protocol encoding, and batched PostgreSQL DataRow streaming.
- Added row-store and executor hot-path reductions for primitive aggregates,
  Top-N projection, projected inner hash-join output, direct projection moves,
  row-cache invalidation misses, no-index UPDATE moves, and DELETE logical-WAL
  key deferral.
- Added a guarded row-store text-group `COUNT(*)` + `SUM(integer)` aggregate
  path that decodes the group and sum columns directly from row bytes for the
  measured `GROUP BY status` workload while preserving generic fallback.
- Added columnar analytics diagnostics and improvements: columnar range
  predicate pushdown, direct columnar Top-N, and small-group/direct
  `COUNT(*)` + `SUM(integer)` grouped aggregate handling.
- Added a mixed projected scan path for gated analytics schemas that filter on
  columnar side-data while projecting default row-store columns, with fallback
  for dictionary/content-addressed storage modes.
- Added a guarded batch-driven columnar projected-filter scan for text
  predicates, improving the gated `columnar_analytics` join path without
  routing integer filter scans through the slower batch driver.
- Added benchmark harness support for Dockerized PostgreSQL/MariaDB client
  comparisons and explicit embedded in-memory profiles:
  `HELIOS_TPS_EMBEDDED_PROFILE=columnar_analytics` and
  `HELIOS_TPS_EMBEDDED_PROFILE=oltp_fast`.
- Added `SQLITE_TPS_BINDINGS=literal` to the SQLite mirror so the default Nano
  literal-SQL TPS suite can be compared against SQLite driven through literal
  SQL, while retaining SQLite's bound-parameter mode as the best-path reference.
- Short-circuited repeated parameterized UPDATE/DELETE execution through cached
  fast DML specs before entering the parameterized plan cache, with a one-entry
  hot spec cache for prepared-style loops. This improves the remaining
  bound-parameter write gap without changing the first-execution planning path
  or transaction/branch/RLS/trigger safety gates.
- Added a fast autocommit `execute_many_params()` path for eligible
  parameterized UPDATE/DELETE batches, so batch clients avoid per-row
  `execute_params()` dispatch and result-cache invalidation while preserving the
  existing DML safety gates.
- Short-circuited pure equi hash joins when a probe key maps to a single build
  tuple, avoiding the pending-match state machine on the common one-to-one /
  many-to-one join case.

### Release notes

- Crates.io currently has `heliosdb-nano` `3.33.0` as the latest published
  version. This verified release candidate is intentionally published as
  `3.34.0` because the TPS batch is a broad performance release rather than a
  patch-only fix.
- Known deferred items for this release: TRUNCATE affected-row count semantics and HNSW
  tombstone/physical-count test semantics. SQLite embedded analytical scans and
  bound-parameter UPDATE remain post-release performance targets. These are not
  regressions from the TPS batch.

### Fixed — Release gate

- Kept `FilteredScan` correct for planner-backed system views by materializing
  `sqlite_master` and `information_schema.*` views before applying pushed-down
  filters.
- Gated storage predicate pushdown to type-exact predicates and fell back to the
  SQL evaluator for mixed/coercive comparisons, restoring string-to-int,
  int-to-decimal, decimal range/IN, date/timestamp string, and UUID string
  `WHERE` behavior.
- Fixed a timing-fragile benchmark assertion by computing transaction overhead
  from nanoseconds instead of millisecond-truncated durations.

### Fixed — Engine FR batch for FK loading, DML routing, Unicode, and MCP tools

- Wired `SET bulk_load_mode = true|false` and `RESET bulk_load_mode` to the storage engine, and deduplicated deferred FK parent probes at commit by referenced table, referenced columns, and parent key.
- Routed generic query surfaces through the DML executor for `INSERT` / `UPDATE` / `DELETE`, so multi-row `VALUES` inserts on user tables no longer fall into the SELECT executor and `ON CONFLICT` / `RETURNING` semantics are preserved.
- Fixed Graph-RAG `WITH CONTEXT` detection on SQL containing multibyte characters before the clause, and added a batch execution regression for multibyte string literals.
- Made MCP `tools/list` omit `inputSchema` unless `verbose=true`, reducing terse tool-list payload size while keeping verbose discovery complete.
- Added PostgreSQL-compatible `IS DISTINCT FROM` / `IS NOT DISTINCT FROM` null-safe comparisons.
- Fixed `ON CONFLICT DO UPDATE SET` expression evaluation for nested `CASE` / `COALESCE` forms that mix `EXCLUDED.*` with table-qualified existing-row references.
- Fixed PostgreSQL wire error-state recovery after constraint violations so an open transaction enters failed state and `ROLLBACK` clears the connection cleanly.
- Added a regression covering long shared-prefix `UNIQUE TEXT` values to guard against false duplicate detection.

## [3.33.0] - 2026-05-26

### Added — In-process Python binding (`bindings/python`, issue #1)

A PyO3 binding exposing `EmbeddedDatabase` directly to Python — no `heliosdb-nano
repl` subprocess, no wire protocol, no serialization hop. Surface: `EmbeddedDatabase(path)`
/ `.in_memory()`, `query(sql, params)` → `list[dict]`, `execute(sql, params)`,
`execute_many(sql, rows)`, `vector_search(store, q, k)`, `create_vector_store` /
`insert_vectors`, `flush()`. The GIL is released around every engine call, so Python
threads can query concurrently. Builds a single abi3 wheel (CPython ≥ 3.8) via maturin.

On the reporter's 448k-row workload the in-process path is **1.5–4.7× faster than
PG-wire** (the access mode their cutover currently uses): `COUNT(*)` 715→153 ms,
`COUNT(DISTINCT)` 1489→725 ms, `GROUP BY+SUM` 1439→902 ms. It is **not** yet
sqlite-competitive on full-table aggregates — that gap is the row-store reading and
materializing whole rows, which needs columnar scans (see `docs/proposals/PROPOSAL_COLUMNAR_STORAGE.md`),
not the access mode.

### Added — `EmbeddedDatabase::query_params_with_columns`

The column-aware, parameter-binding query entry point (rows + output column names with
`$1..$n`), which the binding routes every `query()` through. Fills the gap between
`query_with_columns` (columns, no params) and `query_params` (params, no columns).

### Performance — projection-aware prefix decode for table scans

Single-table, non-filtered scans now decode only the leading columns a query actually
references (via `StorageEngine::scan_table_with_schema_prefix`), stopping before the
costly tail columns instead of deserializing every column of every row. The needed-column
analysis is conservative — it falls back to a full decode on any wildcard, subquery,
join, multi-table plan, or unresolved column, so results are unchanged (verified by the
full suite plus `tests/scan_prefix_decode.rs`). ~25% on narrow aggregates like
`COUNT(DISTINCT col)`; larger wins the fewer columns a query needs.

## [3.32.2] - 2026-05-25

### Fixed — Materialized-view aggregates wrong at scale

`CREATE` / `REFRESH MATERIALIZED VIEW` over a large base table produced wrong
aggregates — e.g. `COUNT(DISTINCT session_id)` materialized as 4 instead of 265
on a 448,573-row table. Two compounding causes, both fixed:

1. **Stale snapshot.** The view materialized through the DDL statement's implicit
   transaction, whose snapshot was a small stale slice of the table rather than the
   current branch-aware view a direct autocommit query sees. It now materializes
   (and re-materializes on `REFRESH`) via a fresh executor with no active
   transaction, and persists the optimized plan so `REFRESH` stays consistent.
2. **Orphaned `__mv_` data rows.** `store_view_data` only purged the view's data
   table when catalog metadata still existed; a prior run that dropped the metadata
   while leaving rows behind caused the freshly computed value to be layered on top
   of the stale one (and read back first). New `StorageEngine::purge_table_data`
   does an unconditional key-range delete before re-populating.

Verified against the reporter's 448k-row data dir across `COUNT(DISTINCT)`,
`COUNT(*)`+`SUM`, `GROUP BY`, `REFRESH`, and reopen (issue #2). Existing
materialized-view suites (integration, incremental, concurrent, auto-refresh,
scheduler) all still pass.

### Added — `EmbeddedDatabase::flush()`

Forces the memtable→SST split so reads, aggregates, and materialized-view
materialization can be exercised across the full LSM tree.

### Docs

Design notes for an in-process PyO3 binding
over `EmbeddedDatabase` (issue #1: the shipped Python "embedded" mode is a REPL
subprocess pipe that loses to sqlite3 on aggregates).

## [3.32.1] - 2026-05-24

### Fixed — PQ default sub-quantizer count collapsed recall

`ProductQuantizerConfig::default_for_dimension` chose far too few sub-quantizers
for common embedding dimensions (dim 384 → 4 → ~0.026 recall@10; dim 128 → 2 →
~0.14), silently collapsing recall for any caller relying on the default
config — the SQL `CREATE INDEX … USING hnsw` PQ path, `quantized_hnsw`, and the
v3.32.0 `create_with_pq` fallback. It now targets ~4 dimensions per sub-vector
(≈16× compression), the validated recall-safe operating point (~0.987 recall@10
vs ~0.989 exact). Added a default-path recall regression test (every prior PQ
test passed an explicit config, which is how this slipped through).

**Behaviour change:** existing default-config PQ indexes now use larger codes
(~16× compression instead of up to ~96×) with substantially better recall.

Found by the codekb-mcp team's adoption testing of v3.32.0.

## [3.32.0] - 2026-05-24

### Added — Persistent PQ-HNSW vector index (opt-in, `vector-persist` feature)

A new durable, crash-recoverable vector index that unifies graph navigation,
Product-Quantization compression, persistence, online deletes, filtered KNN,
and a multi-precision rerank dial in a single index. **Off by default** — the
existing in-RAM vector path (`hnsw_index`, `quantized_hnsw`) is byte-for-byte
unchanged (the default build is identical to v3.31.2).

- **Persistence & recovery** — RocksDB-backed (`__vidx:` keyspace); `open()`
  restores the graph with no rebuild. Coarse per-index locking.
- **In-house HNSW** — graph build/search (Malkov & Yashunin); no third-party
  graph crate on the durable path; public-domain SplitMix64 level assignment.
- **Online deletes + compaction** — `remove()` repairs neighbours so recall
  stays stable under churn; `compact()` reclaims tombstoned space.
- **PQ + ADC + two-stage rerank** — codes resident in RAM, full vectors on
  disk; ~16× less resident RAM at equal recall (measured 0.987 vs 0.989 @10).
- **Filtered KNN** — `search_filtered()` evaluates a row predicate *during*
  traversal, preserving top-k quality where post-filtering a top-k falls short.
- **Multi-precision rerank** — F32 / F16 (hand-rolled IEEE half) / I8 scalar
  quantization; zero new dependencies.

API: `heliosdb_nano::vector::persistent::PersistentVectorIndex`
(`create` / `create_with_pq` + `insert` / `search` / `search_filtered` /
`remove` / `compact`). PQ is L2-only; library API (not yet wired into SQL DDL).

Validated: 26 unit tests; full lib suite green with the feature on (1796
passed); head-to-head vs main shows performance parity on OLTP / vector paths
(default binary unchanged). Design, validation, and head-to-head benchmark were
completed during development.

## [3.31.2] - 2026-05-22

### Fixed — in-txn FK validation ~338× regression (codekb-mcp / KanttBan bulk ingest)

`EmbeddedDatabase::check_referencing_rows_exist` carried an
`active_txn.is_none()` guard on its ART-index fast path, added with the
Quirk H fix in v3.30.0 to preserve read-your-own-writes on a
hypothetical "ART only reflects committed state" model. But the engine
has been updating ART eagerly inside transactions all along
(`on_insert` at `lib.rs:1433`, `on_delete` at `lib.rs:2303`,
`art_undo_log` reversal at `lib.rs:578`), so the guard added no
correctness while pinning the v3.28.0 per-write FK validator
(`check_fk_constraints_on_write`) to the O(parent_size) scan-and-merge
path inside every explicit transaction.

For the heliosdb-codekb-mcp plugin's bulk-ingest workload (117k
FK-bearing INSERTs in one txn × ~10k mean parent scan = ~1.15B tuple
deserialisations), the observed runtime was 3,279 s vs 9.7 s
pre-v3.28.0 — a ~338× regression. Bisect localised the originating
commit to `20169f4` (KanttBan Bug #6 fix on v3.28.0, which added the
FK-on-write check).

End-to-end validation on the same corpus and harness:

| Engine             | `code_index ms write=` | Total ingest |
|--------------------|------------------------|--------------|
| v3.22.2 (baseline) | 9,709 ms               | 35.1 s       |
| v3.30.0 (regressed)| 3,279,362 ms           | ~55 min      |
| **v3.31.2**        | **10,226 ms**          | **59.1 s**   |

~321× speedup on the write phase; within 6% of the pre-v3.28.0
baseline. The four-tier follow-up roadmap (session GUC
`helios.fk_validation`, per-FK `NOT ENFORCED`, HeliosProxy `fk-cache`
WASM plugin) is tracked in `docs/proposals/PROPOSAL_FK_VALIDATION_OPTIMIZATION.md`.

### Fixed — pgvector bare `::vector` cast accepts inferred dimension (ada-core)

ada-core's `HELIOSDB-NANO-COMPATIBILITY.md` flagged that
`'[1,2,3]'::vector` (no dimension) was rejected with
`VECTOR type requires dimension: VECTOR(n)` — but pgvector accepts it
and infers the dimension from the literal's element count. Every
pgvector tutorial / asyncpg snippet / drizzle example uses the bare
form, so this was a high-visibility friction.

The CAST site in `sql/planner.rs` now matches pgvector: bare
`::vector` on a single-quoted string literal counts the elements and
produces `DataType::Vector(N)`. Errors for bare `::vector` on
non-literal sources (parameters / column refs) recommending the
explicit `::vector(N)` form. DDL still requires explicit dimension
(`CREATE TABLE t (v vector)` still errors at
`sql_data_type_to_data_type` — column types must be fixed-dim).

### Fixed — `CAST(uuid AS text)` returns bare canonical form (ada-core)

ada-core observed that `CAST(some_uuid AS text)` wrapped the result
in single quotes (`'<uuid>'`), breaking client-side string comparisons
in psycopg / asyncpg flows. Root cause: `Value::Uuid`'s `Display` impl
at `types.rs:306` emits the SQL-literal form `'<uuid>'`, and the text
cast handler at `evaluator.rs` used the unmodified `to_string()`
output.

Now matches Postgres semantics: `CAST(uuid AS text)` returns the bare
36-char canonical hex form (`550e8400-e29b-41d4-a716-446655440000`)
with no quote characters. Other paths that need the SQL-literal form
(e.g. logging, error messages) are unchanged — the fix is scoped to
the cast handler only.

### Tests

- `tests/fk_in_txn_perf_regression.rs` — 5 in-txn FK correctness cases +
  one perf guard (3k FK INSERTs/txn under 5s).
- `tests/compat_ada_core.rs` — 7 cases covering vector bare cast
  (with literal, mismatched explicit dim, explicit dim guard, DDL
  rejection, empty literal rejection) and UUID-cast-to-text (canonical
  form, unaffected non-cast paths).
- Lib test suite: 1770/1770 pass (109s on the release-host class).

### Known limitations (carried forward to v3.32+)

From ada-core's compatibility doc, the following remain deferred —
each requires multi-week feature work:

- `TEXT[]` array column types (workaround: `JSONB` with JSON array body)
- `ALTER COLUMN DROP NOT NULL` (workaround: table recreate)
- `interval` type (workaround: compute timestamps in app code with
  `timedelta`, bind as `::timestamptz`)
- `LISTEN` / `NOTIFY` pub-sub (workaround: short-interval polling)
- asyncpg binary `int4` protocol mismatch (workaround:
  `postgresql+psycopg://` with `prepare_threshold=None`)
- SQLAlchemy txn-frame "Explicit rollback() forbidden" (workaround:
  ada-core ships a custom migration runner; alembic's online runner is
  non-functional today)
- "Transaction already active" stuck-connection state (workaround:
  `psycopg_pool` with short `max_lifetime` + error-aware retry)
- `ON CONFLICT DO UPDATE` patchiness (workaround:
  `SELECT … FOR UPDATE` + branch)
- Bare `::vector` cast → column-dim safety check at INSERT/UPDATE
  storage time (pgvector does this; Nano's INSERT path doesn't yet
  validate value-vector-dim against column-vector-dim — surfaced by
  the new compat fix, tracked as a follow-up)

## [3.31.1] - 2026-05-17

### Fixed — KanttBan #23: drizzle-kit push introspection layer

KanttBan filed #23 against v3.31.0 (commit `2a8c8cb`) after the
empty-DB `drizzle-kit push` succeeded but the second push — running
the per-table `getColumnsInfoQuery` introspection — failed on
~10 distinct catalog surfaces drizzle-kit reads that Nano didn't yet
expose. 14 surgical phases landed in v3.31.1 to chip away at the
gap. **End-to-end `drizzle-kit push` against a populated DB is
still incomplete** — a remaining JOIN-with-USING planner bug
(cartesian-like cardinality on system-view joins) blocks the final
introspection query. The 14 fixes here are real underlying
improvements; the rest is tracked for v3.31.2.

#### Type plumbing (phase 1, 2.3, 2.5, 2.11)

- `regclass` / `regtype` / `regrole` / `regnamespace` / `regoper` /
  `regoperator` / `regproc` / `regprocedure` / `regconfig` /
  `regdictionary` accepted as type names (cast to TEXT). Both
  first-class `SqlDataType::Regclass` and `Custom("regtype")` are
  matched.
- `Value::String → DataType::Text` cast preserves contents (was
  wrapping in single quotes via `Display::fmt`, breaking downstream
  regtype lookups).
- `x = ANY('{a,b,c}'::T[])` rewritten to `LogicalExpr::InList` at
  plan time. When `T` is a regtype family, labels are mapped to
  OIDs via `regtype_label_to_oid`. Non-literal arrays (column
  references / subqueries) fall back to a constant `false` so the
  surrounding expression doesn't error.

#### Catalog table surfaces (phase 1, 2, 2.4, 2.6, 2.8, 2.9)

- `pg_attrdef` registered as an empty stub view. Populated with
  `nextval('<table>_<col>_seq'::regclass)` rows for IDENTITY
  columns (drizzle's EXISTS subquery against pg_attrdef gates its
  SERIAL detection).
- `pg_sequences` populated with synthetic rows per IDENTITY column
  (from a new `meta:identity:<table>` side-table written by
  CREATE TABLE GENERATED AS IDENTITY).
- `pg_attribute` extended with `attisdropped` / `attndims` /
  `attidentity` / `attgenerated` / `atthasdef` / `attcollation`
  columns drizzle reads per column.
- `pg_type` extended with `typnamespace` / `typtype` / `typowner` /
  `typrelid` / `typbasetype` (drizzle LEFT-JOINs pg_namespace on
  `enum_t.typnamespace`).
- `information_schema.columns` extended with `udt_name` /
  `is_generated` / `generation_expression` / `is_identity` /
  `identity_generation` / `identity_start` / `identity_increment` /
  `identity_maximum` / `identity_minimum` / `identity_cycle`.
  Identity columns now report `is_identity = 'YES'` with the
  correct generation strategy.
- `information_schema.tables` migrated to the SystemViewRegistry.
- `information_schema.table_constraints` / `key_column_usage` /
  `referential_constraints` / `constraint_column_usage` migrated.
- `pg_database` registered with the standard 10-col shape (returns
  the implicit `heliosdb` database only — tenant enumeration
  deferred).

#### Storage layer (phase 2)

- `Catalog::register_identity_columns` / `list_identity_columns` /
  `is_identity_column` / `drop_identity_columns` (side-table over
  `meta:identity:<table>`). Chosen over adding `is_identity` to the
  `Column` struct because that would have propagated through ~200
  struct literals across the codebase.
- CREATE TABLE handler reads the new `is_identity` flag on
  `ColumnDef` (planner-internal) and persists the matching column
  names. DROP TABLE cleans up the record.

#### Scalar functions (phase 1, 2.4)

- `pg_get_expr(adbin, adrelid, ...)` — returns NULL.
- `pg_get_serial_sequence(table, col)` — returns NULL.
- `format_type(oid, typmod)` — maps PG type OID to readable name
  (`23 → 'integer'`, `25 → 'text'`, etc.). Accepts both integer
  OIDs and regtype text (e.g. `'int4'::regtype`).
- `pg_catalog.` function-name prefix stripped before dispatch so
  every helper works with or without the schema-qualifier.

#### Wire / parser / executor fallbacks (phase 2.2, 2.5b, 2.7, 2.10)

- PG's `SELECT FROM <table>` empty-projection shorthand (valid
  inside `EXISTS` subqueries) rewritten to `SELECT 1 FROM <table>`
  before the parser sees it. sqlparser-rs doesn't accept the empty
  form.
- Multi-statement splitter skips trailing comment-only segments
  (`-- …` after a terminating `;` no longer errors with
  "No SQL statement found").
- Correlated `EXISTS (SELECT … WHERE outer.col = inner.col)`
  catches the "Column not found" error and returns `false` instead.
  Same fallback for correlated scalar subqueries
  (`= (SELECT … WHERE outer.col = inner.col)` → NULL). Real
  correlated-subquery support (nested-loop or dependent rewrite)
  remains future work.

### Known limitations (deferred to v3.31.2)

- **JOIN with USING produces cartesian-like cardinality on system
  views**: `tc JOIN ccu USING (constraint_schema, constraint_name)
  WHERE tc.table_name = 'users'` returns 17 rows where the
  equivalent explicit ON returns 1. This is the next concrete
  blocker for drizzle-kit push end-to-end.
- **`SELECT count(*)` over joined system views** returns 0
  (aggregate-over-join planner issue, surfaced during this work).
- **True correlated subqueries** (the proper fix, not the
  fallback-to-false hack) — needs nested-loop join or
  dependent-rewrite design.

### Tests

1771 lib tests + 5 kanttban_quirks_v3_28 integration tests stay
green throughout. KanttBan runtime (vitest 9/9 / 73/73) unaffected.
First-push `drizzle-kit push` against an empty database still
succeeds; second push (introspection of populated tables) gets much
further than v3.31.0 (clears the parse-error stage, populates
identity metadata, joins more catalog views) but doesn't yet
complete due to the USING-JOIN bug above.

## [3.31.0] - 2026-05-16

### Fixed — KanttBan #22 + #20: catalog architecture + CREATE TYPE AS ENUM

KanttBan filed two bugs against v3.30.1 that v3.30.1's surgical
patches couldn't address structurally. v3.31.0 closes both. End-to-end
acceptance test (the actual goal KanttBan wanted): **`drizzle-kit
push` against KanttBan's full schema now succeeds** — all 7 tables
(teams / users / api_tokens / password_reset_tokens / tasks /
task_assignments / task_events) introspect, diff, and apply cleanly.

#### Bug #22 — pg_catalog reads now flow through the regular planner

The v3.30.x catalog handler (`src/protocol/postgres/handler.rs:674`)
short-circuited every pg_catalog query to a substring-routed,
fixed-shape response **before** the planner ran. It couldn't handle
SELECT aliases (`SELECT nspname AS table_schema`), column-alias
prefixes (`SELECT n.nspname FROM pg_namespace n`), JOINs across
catalog tables (the dispatcher picked one table and discarded the
rest), or complex WHERE clauses (limited to `=`, `<>`, `IN`,
`NOT IN`, plus `IS NULL` added in v3.30.1).

Fix is architectural — register pg_catalog tables in the Phase 3
`SystemViewRegistry`, route them through the planner via
`LogicalPlan::Scan`, materialise rows at scan time. Now WHERE / JOIN
/ projection / alias / aggregate all "just work" because the same
operators handle them as for user tables.

Touched files (~250 line net):

- `src/sql/planner.rs`: `pg_catalog.X` → `X` rewrite in
  `dealias_schema`. `table_factor_to_plan` SystemView arm now emits
  `LogicalPlan::Scan` with the registry's schema + the SQL alias,
  not a terminal `LogicalPlan::SystemView` that didn't compose.

- `src/sql/executor/scan.rs`: `handle_scan` checks the registry
  before the storage lookup; on hit, materialises rows via
  `registry.execute()` and wraps in a `ScanOperator`. Mirrors the
  existing CTE branch.

- `src/sql/phase3/system_views.rs`: registered 11 new catalog
  views — `pg_user` / `pg_roles` (paired with the existing
  pg_namespace JOIN target), `information_schema.tables`,
  `pg_database`, plus 9 empty-stub tables drizzle-kit introspects
  (pg_sequences, pg_proc, pg_description, pg_policies, pg_policy,
  pg_matviews, pg_inherits, pg_publication, pg_statistic_ext).

- `src/protocol/postgres/catalog.rs`: stripped the substring
  branches for the migrated tables. Tightened the `\l` matcher
  signature (was eating drizzle-kit's
  `SELECT d.datname AS x FROM pg_database d`). Carved out three
  exceptions (pg_inherits, pg_publication, pg_statistic_ext) for
  psql `\d` sub-queries that use `::pg_catalog.regclass` casts the
  planner doesn't parse yet — direct ORM queries against those
  tables still hit the registry path.

Closes the only remaining tooling-blocker for `drizzle-kit push`,
which v3.30.1 closed at the leaf-symptom level but kept the
architectural problem.

#### Bug #20 — `CREATE TYPE … AS ENUM`

drizzle wraps enum creation in an idempotent DO+EXCEPTION block. The
v3.30.1 SQL parser accepted the syntax but `Statement::CreateType`
fell to the planner's catch-all "Statement not yet supported".

Design: enum labels persist in the catalog at
`meta:enum_type:<name>` → bincode `Vec<String>`. At `CREATE TABLE`
plan time, columns whose declared type is a Custom-name matching a
registered enum get rewritten to TEXT and an implicit
`CHECK (col IN ('a','b','c'))` constraint is appended to the
TableConstraint list. The CHECK persists in storage, so labels are
enforced even if the enum is later dropped.

Touched files:

- `src/storage/catalog.rs`: `register_enum_type` /
  `get_enum_labels` / `enum_type_exists` / `drop_enum_type` +
  `enum_type_key(name) → meta:enum_type:<lower(name)>`.

- `src/sql/logical_plan.rs`: new `CreateEnumType { name, labels }`
  and `DropEnumType { name, if_exists }` variants.

- `src/sql/planner.rs`: `Statement::CreateType { Enum }` arm,
  `Statement::Drop { object_type: Type }` arm,
  `sql_data_type_to_data_type` enum-lookup fallback,
  `create_table_to_plan` synthesises the `InList` CHECK.

- `src/sql/executor/mod.rs`: dispatch arms call the catalog
  methods. `DropEnumType` honours `IF EXISTS`; non-IF-EXISTS drop
  of a missing type errors with `type "<name>" does not exist`.

Composite / range / domain `CREATE TYPE` representations are
intentionally out of scope; they emit a clear "only AS ENUM is
supported" error. `ALTER TYPE ADD VALUE` and `pg_type` / `pg_enum`
catalog rows are future work.

### Tests

1771 lib tests pass (4 v3.30.1 catalog aggregate tests refreshed to
assert the new `Ok(None)` fall-through contract instead of the
legacy `apply_aggregate` `Some` shape). 5 KanttBan v3.28 integration
tests still green. End-to-end `drizzle-kit push` against KanttBan's
schema returns `[✓] Changes applied`.

### Known gaps (deferred to v3.31.1)

- `pg_sequences` still returns 0 rows for IDENTITY columns. Nano
  uses synthetic row-counters rather than catalog sequences, and
  the table schema doesn't currently retain the IDENTITY marker on
  individual columns — surfacing one synthetic sequence row per
  IDENTITY column needs a small schema-side change. drizzle-kit's
  sequence-diff path will show every IDENTITY column as a missing
  sequence (cosmetic — `drizzle-kit push` itself still applies the
  table successfully).
- `SELECT * FROM pg_class` (without the `pg_catalog.` prefix)
  errors with `Table 'pg_class' does not exist`. Drizzle / Prisma
  always qualify; non-blocking. Fix is to register schemaless
  aliases for the catalog views.

### Known nits (not introduced by v3.31.0, deferred to a polish pass)

- `psql \d <table>` renders Default values as JSON-encoded
  LogicalExpr blobs (e.g. `{"Literal":{"String":"backlog"}}`) rather
  than the human-readable SQL (`'backlog'`).
- Identity-column rows in `\d` show "generated by default as
  identity" but the `Nullable` column is blank when it should be
  "not null".

## [3.30.1] - 2026-05-09

### Fixed — KanttBan v3.30 re-test: 3 of 4 still-open quirks

Repros and re-test report from
`/home/app/Personal/KanttBan/HELIOSDB_v3_30_0_RETEST.md`. The
remaining item, Bug #20 (`CREATE TYPE … AS ENUM`), needs a
new planner arm and is deferred to v3.31.0.

#### Bug #21A — Aggregates on `pg_catalog` / `information_schema`

The PG-wire catalog handler short-circuits introspection
queries to a substring-matched response *before* the planner
runs (`src/protocol/postgres/handler.rs`), so `count(*)` and
`GROUP BY` were never executed — drivers got the underlying
catalog rows back instead of a scalar / bucketed shape, and
`drizzle-kit push` rejected the malformed schema-set.

`src/protocol/postgres/catalog.rs` now:
- recognises `col IS NULL` / `col IS NOT NULL` in
  `apply_where_filter` (the previous matcher only handled `=`,
  `<>`, `IN`, `NOT IN`);
- runs an `apply_aggregate` stage after WHERE filtering that
  detects `count(*)` (with optional single-column `GROUP BY`)
  and emits the `count`-shaped response. Anything more complex
  (multiple GROUP BY columns, HAVING, custom aggregates) falls
  through to ordinary projection, matching the existing
  graceful-degradation path.

Closes the only remaining tooling-blocker for `drizzle-kit push`.

#### Bug #7 — `psql \d <table>` end-to-end

`\d <table>` sends ~9 catalog sub-queries in sequence, several
of which our handler matched too loosely or not at all. The
previous matcher only fixed the second (15-col pg_class
header); end-to-end smoke against the freshly-built v3.30.1
binary surfaced four more issues that all manifested as
"column number N is out of range 0..M" libpq errors followed
by a psql segfault. Now handled in `try_psql_metacommand`
(`src/protocol/postgres/catalog.rs`):

- **Q1 (relation OID lookup)** — `c.relname OPERATOR(pg_catalog.~)
  '^(<name>)$'`. Was falling through to the generic 5-col
  `query_pg_class`, which returned every table; psql then
  iterated `\d` over each one in turn. Added a regex-aware
  matcher that returns exactly the matching relation.
- **Q3 (per-column descriptor)** — the previous 7-col matcher
  was keyed on `pg_catalog.pg_attribute` + `attnum` +
  `attisdropped`, which false-fired on the `pg_statistic_ext`
  query that JOINs `pg_attribute` in a subquery. Tightened to
  require the OUTER `from pg_catalog.pg_attribute a` plus
  `a.attrelid = '<oid>'`.
- **Q4 (index-list, 12 cols)** — `pg_get_indexdef` +
  `pg_get_constraintdef` + `c2.relname`. The generic
  `query_pg_index` returned 5 cols. New matcher emits one row
  per PRIMARY KEY / UNIQUE column with the full 12-col shape.
- **Q5 / Q6 / Q7 / Q8** — added empty-shape stubs for
  `pg_statistic_ext` (9 cols), `pg_policy` singular catalog
  table (6 cols), `pg_publication` (1 col), `pg_inherits`
  (3 cols). Without these the queries fell through to
  `query_pg_class` / `query_pg_attribute` / `query_pg_roles`
  and rendered bogus inheritance / partition / RLS sections.

Net result: `psql -c '\d users'` now renders the full
`Table "public.users"` panel with column types, defaults,
nullability, and indexes — no error, no segfault, no spurious
inheritance info.

#### Bug #14 — `DO $$ BEGIN CREATE TABLE IF NOT EXISTS … END $$`

Two layered issues, both in
`src/protocol/postgres/handler.rs`:

1. `pg_detect_plpgsql` did a substring scan for ` IF ` and
   flagged the SQL DDL idiom `IF NOT EXISTS` as PL/pgSQL
   control flow. Now strips ` IF NOT EXISTS ` / ` IF EXISTS `
   from the body before the keyword scan; genuine PL/pgSQL
   `IF cond THEN` is still detected (regression-tested).
2. `pg_split_exception` used a needle ` EXCEPTION `
   (space-bounded), so when drizzle-kit emitted the EXCEPTION
   clause on its own line — `\n` before EXCEPTION — the split
   missed it. The unstripped clause then leaked through to
   `pg_split_sql_respecting_quotes` which handed
   `EXCEPTION WHEN duplicate_object THEN null` to sqlparser as
   a top-level statement. Replaced with a `match_indices` walk
   that accepts any ASCII whitespace before/after the keyword.

drizzle-kit's idempotent `CREATE TABLE IF NOT EXISTS` wrapped
in a DO block now executes; the rerun catches via the
EXCEPTION handler (or is a no-op when IF NOT EXISTS suppresses
the error first).

## [3.30.0] - 2026-05-04

### Fixed — Token-Dashboard perf carry-over: Quirks H + I

Both deferred from v3.27 and v3.28. Field repros from the
Token-Dashboard cutover at
`~/.claude/projects/-home-app-websites-token-dashboard/memory/heliosdb_v326_quirks.md`.

#### Quirk H — DELETE / DROP on FK-referenced tables (>5 min hang → milliseconds)

`EmbeddedDatabase::check_referencing_rows_exist` did a full
`storage.scan_table` of the referencing table for every parent
row being deleted. With ~11k rows on each side that was
~121 M tuple checks per DELETE, dominated by per-row bincode
deserialisation. Now uses the existing PK / UNIQUE / FK ART
index for the lookup when available — O(log N) per call. The
slow scan-and-merge fallback stays for the in-transaction path
(`active_txn = Some(_)`) so read-your-own-writes semantics from
the v3.22.1 fix are preserved.

Bench: DELETE 1000 FK-parent rows: **~950 ms** (vs >5 min);
DROP populated FK'd parent + child: **~5 ms**.

#### Quirk I — `INSERT … ON CONFLICT (col) DO UPDATE` (~400× slower → instant)

The DO UPDATE branch's existing-row lookup ran
`self.query("SELECT pk FROM tbl WHERE col = 'val'")` per
conflicting row — a full SQL planner round-trip ending in a
table scan. Now goes through `art.find_column_index` +
`art.index_get_all` directly: O(log N) per row,
ACID-correct (the UNIQUE index was already proven by
`check_unique_constraints` immediately above).

Bench: 1000 ON CONFLICT DO UPDATE on a populated 1000-row
table: **812 ms** (~1230 ops/sec). v3.27 baseline was
0.4 ops/sec — **~3000× speedup**.

### Tests

- New: `tests/quirks_h_i_perf.rs` (4 tests, all pass with the
  bounds documented above). Includes
  `fk_validation_still_blocks_orphan_deletes` to pin
  correctness alongside the perf bound.

### Documentation

- `tests/kanttban_quirks_v3_28.rs` carry-forward intact.
- KanttBan team's `BUGS_HELIOSDB.md` (#18 — intermittent
  ParseComplete corruption) updated with a `RUST_LOG=trace`
  capture recipe pointing at `docs/TRACING_GUIDE.md`. Next
  reproduction will have enough state to triage.

### Investigation closed

- **Bug #16 — `CREATE DATABASE` per-tenant routing.** Confirmed
  `IsolationMode::DatabasePerTenant` is metadata-only: the
  PG-wire startup handler validates the requested DB name
  (Bug 5 / v3.25) but doesn't set per-connection tenant context,
  and the storage layer has no per-tenant key prefix. v3.29
  visibility fix (`\l` lists CREATE-DATABASE'd tenants) is
  enough for KanttBan's single-DB use. Real per-DB isolation
  needs (a) per-connection tenant context wiring and (b) a
  per-tenant storage namespace; both are bigger scope and
  carry forward to v3.31 / later.

### Carry-forward to v3.31

- **Bug #17** — SQL-level `PREPARE` / `EXECUTE` / `DEALLOCATE`.
  Wire-protocol extended-query already works.
- **Bug #18** — once a fresh trace lands, debug the FK
  validator vs response-writer ordering.
- **Bug #16 strict isolation** — per-DB storage namespace +
  `current_database()` resolution from connection state.

## [3.29.0] - 2026-05-04

### Fixed — KanttBan / Drizzle ORM follow-up (BUGS_HELIOSDB.md re-test)

The KanttBan team verified v3.28.0 end-to-end and reported 7 new
issues plus 2 partial fixes. v3.29.0 closes the deferred #7 and
all of the new #12–#16; #17 (PREPARE/EXECUTE) and #18 (intermittent
ParseComplete corruption) carry forward.

- **Bug #7 — `psql \d <table>` "column number 5 is out of range 0..4"**
  (DEFERRED from v3.28). The PG-wire catalog dispatcher now intercepts
  psql 13's per-column descriptor query and emits the 7-column shape
  it expects (attname, format_type, default_expr, attnotnull,
  collation, attidentity, attgenerated). Filtered by the `attrelid`
  literal in the WHERE clause. `\d <table>` now lists columns
  interactively.

- **Bug #12 — `pg_policies` + `pg_matviews` missing.** drizzle-kit's
  introspection sweep advanced past v3.28's `pg_sequences` fix and
  tripped on these two views. Both now stub with the standard PG
  column shape and zero rows. Added `pg_policies`, `pg_matviews`
  to `is_catalog_query`'s marker list.

- **Bug #13 — schema-qualified `"public"."tbl"` references.**
  `Planner::normalize_object_name` now strips both `public.` and
  `pg_catalog.` schema prefixes during ObjectName resolution.
  `REFERENCES "public"."teams"("id")` resolves to `teams` the same
  way the bare `REFERENCES "teams"("id")` did. `_hdb_code.<t>` /
  `_hdb_graph.<t>` aliases are unchanged.

- **Bug #14 — `DO $$ BEGIN … EXCEPTION WHEN duplicate_object THEN
  null; END $$;`.** New `pg_split_exception` parser splits the body
  into BEGIN-body + named exception list, then runs the body and
  swallows only errors whose message matches one of the listed
  conditions (mapped via `pg_exception_matches`: `duplicate_object`
  / `duplicate_table` / `unique_violation` / `undefined_table` /
  `OTHERS` etc.). drizzle-kit's idempotent ALTERs now apply.
  Full PL/pgSQL control flow (DECLARE / IF / LOOP / RAISE / `:=`)
  still errors loudly via `pg_detect_plpgsql`.

- **Bug #15 — extended-query UPDATE silently bypassed FK** (HIGH,
  correctness). v3.28 fixed FK enforcement on the simple-query
  path; the parameterised path (Drizzle's
  `db.update().set().where()`) silently accepted orphan rows. The
  third Update arm in `execute_plan_with_params_inner` now runs
  `check_fk_constraints_on_write` — same hook the simple-query
  path and parameterised INSERT use. Drizzle's UPDATE FK violations
  now error correctly.

- **Bug #16 — `CREATE DATABASE` silent no-op (regression from v3.27).**
  Partial fix. `CREATE DATABASE foo` already registered a tenant via
  the v3.25 wrap, but `query_pg_database` returned hardcoded
  "heliosdb" so `\l` and ORM probes never saw it. Now appends every
  registered tenant to the `pg_database` catalog response. Full
  per-DB isolation (separate storage namespaces, `current_database()`
  resolution) is a larger architectural change deferred to v3.30.0;
  the regression-from-v3.27 is the visibility issue, which is closed.

### Tests

- New: `tests/kanttban_quirks_v3_28.rs` (5 tests covering #7, #13,
  #14, #15, #16 at the embedded API level). All pass.
- Lib 1758/1758. All cross-feature suites green
  (kanttban_v3_27 9/9, dashboard_quirks 10/10, repl_meta 4/4,
  info_schema 9/9, create_database 8/8, scram_gs2 13/13).

### Carry-forward to v3.30.0

- **Bug #17** — SQL-level `PREPARE` / `EXECUTE` / `DEALLOCATE`. Wire
  protocol extended-query works; only the SQL statement form errors.
  LOW priority (most ORMs use the wire form directly).
- **Bug #18** — intermittent extended-query INSERT FK-violation
  ParseComplete corruption. Flaky single-occurrence; needs a
  reliable repro before deeper investigation.
- **#16 strict per-database isolation** — `current_database()`
  resolution + per-tenant storage namespace.
- **Token-Dashboard Quirks H + I** — DELETE/DROP hang on 11k-row
  tables; ON CONFLICT DO UPDATE 400× slower than INSERT on
  populated tables. Need targeted benches.

## [3.28.0] - 2026-05-04

### Fixed — KanttBan / Drizzle ORM migration quirks

The KanttBan team migrated their full Drizzle-ORM auth + tasks API to
HeliosDB-Nano v3.27.0 and filed nine bugs in
`/home/app/Personal/KanttBan/Kanttban/BUGS_HELIOSDB.md`. v3.28.0 closes
all of them (#7, #8 — psql `\d` col-count and CREATE SCHEMA — deferred
to v3.29.0; both have working substitutes in `information_schema`).

- **Bug #1 (CRITICAL) — `--daemon` reports success even when worker
  never starts.** The parent now polls the child's PG port for up to
  5 s and returns a non-zero exit + cleans up the pidfile if the
  worker dies at startup or fails to bind. Removes the need for
  caller scripts to poll `ss` themselves.
- **Bug #2 (CRITICAL) — `--http-port` collision silently kills the
  whole server.** The HTTP health endpoint now binds eagerly (so bind
  failures surface at ERROR before the "Server ready!" banner), runs
  in a detached task (so a late-life failure can't tear the database
  listener down), and supports `--http-port 0` to opt out entirely.
- **Bug #3 (CRITICAL) — `pg_sequences` missing.** The PG-wire catalog
  dispatcher (`src/protocol/postgres/catalog.rs`) now responds to
  `pg_sequences` with the standard 11-column shape (empty rows —
  Nano's BIGSERIAL is a synthetic counter, not a sequence object).
  Unblocks `drizzle-kit pull / push` introspection.
- **Bug #4 (CRITICAL) — `GENERATED ALWAYS AS IDENTITY (sequence name
  … INCREMENT BY …)` parse error.** New
  `Parser::preprocess_strip_identity_options` quote-aware preprocessor
  strips the parenthesized sequence-options block before sqlparser
  sees it. The bare IDENTITY auto-generates monotonically as before.
- **Bug #5 (CRITICAL) — `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY`
  not supported.** New `LogicalPlan::AlterTableAddForeignKey` variant
  routes the sqlparser ForeignKey shape through to the same
  `catalog.add_foreign_key` API the inline-`REFERENCES` path in CREATE
  TABLE already uses. Drizzle / Prisma / Flyway / Liquibase now run
  unmodified migrations.
- **Bug #6 (HIGH) — FK enforcement asymmetric (DELETE checked,
  INSERT/UPDATE skipped).** New
  `EmbeddedDatabase::check_fk_constraints_on_write` runs before every
  INSERT and UPDATE, walking each outgoing FK and verifying the
  referenced parent row exists (PG MATCH SIMPLE — NULL FK columns
  trivially satisfy the constraint). The fast UPDATE path (line 4187)
  also bails to the normal path when persisted FK metadata exists,
  even if no FK ART index is registered.
- **Bug #9 (LOW) — `current_setting()` not implemented.** Now a
  scalar function returning a curated set of GUCs (`server_version`,
  `client_encoding`, `datestyle`, `timezone`, `search_path`, …).
  Two-arg form with `missing_ok = true` returns empty string for
  unknown settings; one-arg form errors. PG-correct semantics.
- **Bug #10 (cosmetic) — banner / init / trace strings said
  "HeliosDB-Lite".** Flipped to "HeliosDB Nano" across `src/main.rs`,
  `src/api/server.rs`, `src/api/openapi/mod.rs`, `src/api/supabase`,
  `src/protocols/server_manager.rs`, `src/repl/`, `src/storage/dump`,
  `src/network/session.rs`, `src/git_integration/hooks/`. Module names
  and crate name unchanged (still `heliosdb-nano`).
- **Bug #11 (cosmetic) — `--version` flag rejected.** Added
  `#[command(version = env!("CARGO_PKG_VERSION"))]` to the clap
  derive. `heliosdb-nano --version` now prints
  `heliosdb-nano 3.28.0`.

### Tests

- New: `tests/kanttban_quirks_v3_27.rs` — 9 unit tests covering
  bugs #3–#6, #9 end to end. All pass.
- Lib 1758/1758, dashboard_quirks 10/10, repl_meta_commands 4/4,
  information_schema_completion 9/9, create_database 8/8, scram 13/13.

### Deferred

- **Bug #7** — `psql \d <table>` "column number 5 is out of range
  0..4" — needs the catalog query to return the full 8-column
  shape psql expects. Workaround: `SELECT column_name, data_type
  FROM information_schema.columns WHERE table_name = '<t>'` works
  today.
- **Bug #8** — `CREATE DATABASE` / `CREATE SCHEMA` not supported.
  v3.25.0 added per-tenant CREATE DATABASE through the tenant
  manager, but plain `CREATE SCHEMA` for namespacing within a
  database is a larger scope. v3.29.0.

### Dashboard cutover quirks (v3.27.0) — perf items still open

- **Quirk H** (DELETE/DROP hang on 11k-row table) and **Quirk I**
  (DO UPDATE 400× slower than INSERT on populated table) carry
  forward unchanged. v3.29.0 with targeted benches.

## [3.27.0] - 2026-05-04

### Fixed — Token-dashboard cutover quirks B + C + D + E

The Token-Dashboard team filed five new quirks against v3.26.0 during
their high-volume cutover (`/home/app/websites/token-dashboard/HELIOSDB_CUTOVER_RESULTS.md`,
`HELIOSDB_INTEGRATION_RESULTS.md`). v3.27.0 closes the four
correctness ones in a single release.

#### Quirk B + Quirk E — Bind values no longer fed back into the parser

The PG-wire extended-query Bind/Execute path at
`src/protocol/postgres/handler_extended.rs:195-200` was textually
splicing parameter values into the SQL string before execution
(`substitute_parameters` from `prepared.rs`). Two fallout symptoms:

- **Quirk B** — `WITH spend AS (…) SELECT … FROM spend WHERE x >= $1`
  returned 0 rows even when the flat `WHERE x >= $1` form returned the
  expected rows. (Some shapes of the substituted SQL didn't round-trip
  through the planner identically to the parameterised form.)
- **Quirk E** — payloads with control chars (`\0`, `\x01`–`\x1f` other
  than `\t\n\r`) or large length (>~50k chars) failed Bind with
  `Unterminated string literal at Line: N, Column: N`. The substituted
  SQL was malformed because the value-side bytes weren't sanitised for
  re-parsing.

Now the extended-query path threads parameters through to the planner
as `Value`s via `db.query_params` / `db.execute_params_returning` /
`db.execute_params`. Textual substitution is preserved only for the
catalog dispatcher (`PgCatalog::handle_query`) which is regex-based
and predates parameter support.

This is the dashboard-cutover-blocker fix.

#### Quirk C — `CREATE BRANCH 'name'` strips surrounding quotes

`src/sql/parser.rs::parse_create_branch_sql` was including the
surrounding single or double quotes in the branch name. The
dashboard's call:

```sql
CREATE BRANCH 'verify-branch' AS OF NOW;
```

stored the branch under `'verify-branch'` (with quotes), making it
unfindable via `SHOW BRANCHES` filtered against `verify-branch`. The
parser now strips both `'…'` and `"…"` wrappers, accepting all three
forms:

- `CREATE BRANCH foo AS OF NOW` (bare identifier)
- `CREATE BRANCH 'foo' AS OF NOW` (single-quoted)
- `CREATE BRANCH "foo" AS OF NOW` (double-quoted)

#### Quirk D — `SUBSTRING (s FROM x [FOR y])` special form supported

The SQL-standard `Substring { special: false }` AST node hit the
planner's catch-all `Expression not yet supported`. Now desugared into
a `substr(s, x, y)` ScalarFunction call (the same node the function
form `SUBSTR(s, x, y)` already produced). Three shapes covered:

- `SUBSTRING(s FROM x FOR y)` → `substr(s, x, y)`
- `SUBSTRING(s FROM x)`       → `substr(s, x)` (open-ended slice)
- `SUBSTRING(s FOR y)`        → `substr(s, 1, y)` (start at column 1)

### Investigated — Quirk A (autocommit visibility)

The dashboard's pg8000 `autocommit=True` repro (INSERT-then-SELECT
sees 0 rows until explicit COMMIT) was investigated. The embedded
API repro at `tests/dashboard_quirks_v3_26.rs::embedded_insert_then_select_*`
passes — the fix may live in the wire-protocol session-state
machine which our in-process test harness can't exercise (the
existing `tests/server_mode_integration_test.rs` in-process harness
hits a separate stack-overflow). The wire-side investigation is
deferred; if the symptom persists after the dashboard team re-tests
v3.27.0 against a real daemon, escalate.

The Quirk B/E param-threading refactor incidentally moves writes
through a cleaner code path that may resolve A as a side effect.

### Tests

- New: `tests/dashboard_quirks_v3_26.rs` — 10 unit tests covering all
  four fixed quirks plus the A repro that already passes at the
  embedded API level.

### Known limits — perf quirks deferred to v3.28.0

- **Quirk H** — DELETE / DROP on a table with ~11k rows hangs the
  daemon for >5 min, requiring kill -9. Likely a per-row WAL fsync
  issue. Workaround: avoid bulk DELETE on large tables; recreate the
  data dir for full resets.
- **Quirk I** — `INSERT … ON CONFLICT … DO UPDATE` is ~400× slower
  than bare `INSERT` on a populated table (0.4 ins/sec at 11k rows
  vs 181). Workaround: use `DO NOTHING` if upsert semantics aren't
  required, or pre-truncate target tables.

Both will be addressed in v3.28.0 with targeted benches.

### Validation

Lib tests 1758/1758 pass. Doc tests 47/47 pass. All targeted
integration suites (dashboard_quirks 10/10, repl_meta_commands 4/4,
information_schema_completion 9/9, create_database 8/8,
scram_gs2 13/13) pass.

## [3.26.1] - 2026-05-03

### Fixed — REPL meta-commands now display real data instead of hint text

`\branches`, `\snapshots`, `\dmv`, `\dmv <view>`, `\compression`, and
`\indexes <table>` were returning a "use this SQL query instead" hint
message rather than running the corresponding query themselves.
`SHOW BRANCHES` was hitting the executor and computing rows correctly,
but the REPL routed it through `db.execute()` which discards rows and
only surfaces the count, producing `Query OK, 2 row(s) affected` with
no table.

- `\branches` now runs `SELECT * FROM pg_database_branches()` and
  pretty-prints the table.
- `\snapshots` now reads `db.storage.snapshot_manager().list_snapshots()`
  directly (the `pg_snapshots` view is PG-wire-only, not embedded-path).
- `\dmv` now runs `SELECT * FROM pg_mv_staleness()`.
- `\dmv <view>` now runs the same with `WHERE view_name = '<view>'`
  (single-quote-escaped per SQL-92).
- `\compression` now runs `SELECT * FROM pg_vector_index_stats()`.
- `\indexes <table>` now walks `db.storage.art_indexes().list_indexes()`
  filtered to that table.
- `SHOW BRANCHES` in the REPL is now classified as a query (not a
  command) so its rows are pretty-printed; the same input via the
  embedded API (`db.query_with_columns`) also now succeeds — previously
  sqlparser parsed it as a generic `SHOW <variable>` and the planner
  rejected it.

### Tests

- New: `tests/repl_meta_commands.rs` — 4 unit tests covering the
  underlying queries (`pg_database_branches()`, `pg_mv_staleness()`,
  `pg_vector_index_stats()`) and the `SHOW BRANCHES` query-path
  routing.

## [3.26.0] - 2026-05-03

### Fixed — SCRAM-SHA-256 GS2 header parsing (Bug 2)

The SCRAM client-first-message parser at
`src/protocol/postgres/handler.rs:768-777` was splitting on commas and
indexing `parts[1]` as the username, which only worked when the GS2
header was missing. Real clients (libpq, asyncpg, node-postgres, JDBC,
sqlx, psycopg2) per RFC 5802 always send `n,,n=user,r=nonce` — the
leading `n,,` is the GS2 channel-binding flag + (empty) authzid header.
The old parser misaligned every offset and rejected every compliant
client with `Invalid SCRAM client-first-message`.

The new parser (`auth::parse_scram_client_first`) walks the GS2 header
properly via `splitn(3, ',')` and scans the bare body for `n=` /
`r=` tokens (order-independent). Channel-binding flag (`p=`),
authzid (`a=`), and the `y` flag are all accepted.

Closes Bug 2 from the dashboard-migration triage. With this fix and
the v3.25.0 connection-name validation, the dashboard team's
`--auth scram-sha-256` deployment path is fully unblocked.

### Changed — same-host-only `trust` enforcement

**Behaviour change**: `PgServer::new` and `PgServer::with_auth_manager`
now refuse to construct when `auth_method = AuthMethod::Trust` and the
listener is non-loopback. Per the dashboard-migration triage's
auth-defaults resolution, silently accepting any client on a public
interface is a footgun:

- `127.0.0.1` and `::1`: trust is allowed.
- `0.0.0.0`, `::`, `192.0.2.x`, etc.: trust is refused with a clear
  error naming the safe alternatives (`password`, `scram-sha-256`).
- `with_auth_manager` checks the AuthManager's method (the actual
  runtime behaviour), not just `config.auth_method`.

If you currently rely on `--listen 0.0.0.0 --auth trust`, switch to
`--auth scram-sha-256` (recommended) or `--auth password`. Loopback
deployments are unchanged.

### Tests

- New: `tests/scram_gs2_and_trust_loopback.rs` — 13 unit tests covering
  the GS2 parser (libpq, authzid, `y`-flag, truncated, missing-username,
  missing-nonce) and the trust-loopback gate (loopback-allowed,
  IPv6-loopback-allowed, 0.0.0.0-refused, public-IP-refused,
  password-on-public-allowed, scram-on-public-allowed,
  with-auth-manager-also-enforced).

### Validation

Followed the 8-phase merge-validation methodology
(`.claude/skills/heliosdb-nano-merge-validation/SKILL.md`). Lib tests 1758/1758 pass,
all targeted tests pass, no regression in v3.24.0 / v3.25.0 surfaces.

## [3.25.0] - 2026-05-03

### Added — `CREATE DATABASE` + `DROP DATABASE` (Bug 1)

Closes the dashboard-migration triage's Bug 1. ORM bootstraps that emit
`CREATE DATABASE testdb` (TypeORM, sqlx with `database_create=true`,
node-pg admin connections) now succeed end-to-end. The new SQL surface
is a thin metadata-only wrapper around the existing `TenantManager` API
— there is no storage-layout change.

- `Statement::CreateDatabase` and `Statement::Drop { object_type:
  Database }` are routed through new `LogicalPlan::CreateDatabase` /
  `LogicalPlan::DropDatabase` nodes.
- `IF NOT EXISTS` succeeds silently on duplicate names. `IF EXISTS`
  succeeds silently on missing names. Bare `CREATE DATABASE foo` errors
  on duplicate; bare `DROP DATABASE foo` errors when `foo` doesn't
  exist.
- Reserved names (`heliosdb`, `postgres`) cannot be created or dropped.
  `CREATE DATABASE IF NOT EXISTS heliosdb` succeeds silently
  (idempotent shape).

The new tenant is registered with `IsolationMode::DatabasePerTenant`
on the `free` plan. Tenants are tracked in the in-memory
`TenantManager`; cross-restart persistence is a follow-up.

### Changed — PG-wire StartupMessage validates `database` parameter (Bug 5)

**Behaviour change**: previously, the PG-wire startup handler at
`src/protocol/postgres/handler.rs:236-239` accepted any `database`
parameter without validation, silently routing every connection to the
default `heliosdb` keyspace. v3.25.0 now validates the requested name:

- `heliosdb` and `postgres` are accepted (reserved system databases).
- Any registered tenant name is accepted.
- An empty / missing `database` parameter falls back to the
  `user` parameter (libpq default).
- Anything else returns `database "x" does not exist` and the
  connection is refused.

This closes Bug 5 from the dashboard-migration triage. Clients
configured to hit a deliberately-named database now get a clear error
on a typo; clients using the libpq defaults are unaffected because
`postgres` and `heliosdb` are both recognised.

### Tests

- New: `tests/create_database_and_dbname_validation.rs` — 8 unit tests
  covering CREATE / DROP / IF [NOT] EXISTS / reserved-name protection /
  StartupMessage validation.

### Validation

Followed the 8-phase merge-validation methodology
(`.claude/skills/heliosdb-nano-merge-validation/SKILL.md`). Lib tests 1758/1758 pass,
doc tests 47/47 pass, all targeted CREATE-DATABASE tests pass,
v3.24.0 information_schema tests still pass (no regression).

## [3.24.0] - 2026-05-03

### Added — `information_schema` completion (Bug 4)

Closes the dashboard-migration triage's Bug 4. ORM bootstraps that probe
the SQL-standard `information_schema.*` views (TypeORM's `hasTable`,
sqlx's metadata reflection, Drizzle's introspection, etc.) now get
well-formed responses instead of misleading silent-empties.

- **`information_schema.routines`** — 16-column SQL-standard schema, zero
  rows. Nano supports `CREATE FUNCTION` but does not yet surface the
  runtime function catalog through this view; an empty result is
  correct (signals "no user-defined routines visible").
- **`information_schema.referential_constraints`** — populated from real
  FK metadata. One row per `FOREIGN KEY` constraint with `update_rule`
  and `delete_rule` mapped from `ReferentialAction` (`NO ACTION`,
  `RESTRICT`, `CASCADE`, `SET NULL`, `SET DEFAULT`).
- **`information_schema.check_constraints`** and
  **`information_schema.views`** — SQL-standard schemas, zero rows.
- **Whitelist of SQL-standard placeholder views** — `triggers`,
  `parameters`, `sequences`, `domains`, `character_sets`, `collations`,
  `*_privileges`, `role_*_grants`, `constraint_*_usage`,
  `view_*_usage`, `applicable_roles`, `enabled_roles`,
  `element_types`. All return zero rows with the right column shape
  so ORM probes don't break.

### Changed — `information_schema.<unknown>` now errors loudly

**Behaviour change**: previously, any unrecognised `information_schema.*`
query returned an empty schema with empty rows. ORMs that strict-check
saw a misleading empty result rather than an actionable error. Now
unknown view names return a `QueryExecution` error that names every
supported view and points users to file an issue.

If your client relies on the silent-empty behaviour for an
SQL-standard view, that view is in the whitelist and behaviour is
unchanged. If it relies on it for a non-standard / made-up name,
file an issue and we'll add the placeholder.

### Tests

- New: `tests/information_schema_completion.rs` — 9 unit tests covering
  every new view, the whitelist, the error-loudly behaviour, and a
  regression check for the four pre-existing views (`tables`,
  `columns`, `schemata`, `key_column_usage`, `table_constraints`).

### Validation

Followed the 8-phase merge-validation methodology
(`.claude/skills/heliosdb-nano-merge-validation/SKILL.md`). Lib tests 1758/1758 pass,
doc tests 47/47 pass, info_schema completion 9/9 pass, system_views
22/22 pass.

## [3.23.2] - 2026-05-03

### Confirmed fixed in-range — five more bugs from the dashboard-migration report

Verified by direct PG-wire repro (`target/release/heliosdb-nano start
--port 15441 --http-port 18081 --auth trust|password`, queried via
psql + psycopg2). Bumps the in-range-fixed count from 2 to 7.

| # | Repro | Status |
|---|-------|--------|
| 3 | `--auth password`, correct/wrong PGPASSWORD | FIXED — correct password authenticates, wrong is rejected with `Invalid password` |
| 6 | `psql -f synth_pg_dump.sql` (full pg_dump-shaped SET preamble + DDL + INSERT) | FIXED — completes in <1s; rows restored |
| 7 | `psql -c "CREATE TABLE a (x INT); CREATE TABLE b (y INT)"` simple-query path | FIXED — both DDLs execute (extended-query path still has its own gap, lower priority) |
| 8 | psycopg2 `cur.execute("SELECT COUNT(*) FROM pings WHERE week_bucket = %s", ...)` | FIXED — returns correct rows |
| 9 | psycopg2 `COUNT(DISTINCT col) WHERE x = %s` (parameterised) vs literal | FIXED — both forms agree |

### Added — `tests/extended_query_planner_schema.rs`

Locks in the planner-side correctness for parameterised SELECT
schema derivation: column names + types match between the literal
and `$N` forms for COUNT(*), COUNT(DISTINCT), aliased aggregates,
and multi-column projections. The unit tests catch any future
planner regression that would re-open Bug 8 / 9.

### Updated — dashboard-migration bug triage

Re-triaged with the live verification table. Net status:
- ✅ Fixed: 3, 6, 7 (simple-query), 8, 9, 10, 11 (and 4 basic shape).
- ❌ Still present: 1 (`CREATE DATABASE`), 2 (SCRAM-SHA-256), 5 (DB-name validation).
- 🟡 Partial: 4 — `routines` + `referential_constraints` views still empty.

The previously-planned v3.24.0 fix milestone (Bug 8 + 9 + 6) is now
a no-op. Revised release plan: v3.24.0 → Bug 4 completion;
v3.25.0 → Bug 1 + 5 (tenant-API DDL surface); v3.26.0 → Bug 2 SCRAM
fix + same-host-only `trust` enforcement.

### Note for the dashboard team

If your TypeORM bootstrap doesn't need `CREATE DATABASE`, doesn't
need SCRAM-SHA-256, and you can connect over loopback / Unix
socket with `--auth trust`, **all known migration blockers are
already closed in v3.23.2**. Please re-test —
`cargo install heliosdb-nano@3.23.2` or pull
`heliosdb-nano-v2:3.23.2` once the image is rebuilt.

## [3.23.1] - 2026-05-03

### Added — Documentation & skill catalogue updates (no code changes)

- **`heliosdb-nano-tenant` skill** — covers the existing multi-tenancy
  surface (isolation modes, plans, RLS, `\tenant` REPL commands, library
  API). Closes a gap in the skill catalogue.
- **`heliosdb-nano-merge-validation` skill** — eight-phase pre-merge
  validation methodology distilled from the v3.23.0 release work. Required
  reading before any non-trivial change to engine code (planner, executor,
  storage, parser, optimiser).
- **Dashboard-migration bug triage** — full triage of the 11 bugs
  filed by the Claude-Dashboard team against v3.19.1, verified against
  v3.23.0. Two bugs (10, 11 — column-projection / aggregate-alias) are
  confirmed already fixed in-range; the remaining nine are scheduled
  across v3.24.0 → v3.27.1.

### Confirmed fixed in-range (v3.19.1 → v3.23.0)

- **Bug 10** — `SELECT COUNT(*) AS xyzzy FROM t` correctly returns the
  column named `xyzzy` (alias preserved on aggregate output).
- **Bug 11** — `SELECT col FROM t` returns only the requested column,
  not the entire row.

Dashboard team can re-test against v3.23.x to confirm — no re-engagement
needed for these two specifically.

## [3.23.0] - 2026-05-03

### Fixed — JOIN with one-sided ON predicate no longer degenerates to cross product

Closes the latent planner bug previously tracked in
`FEATURE_REQUEST_cte_in_join_constant_predicate.md` (now removed).

A `JOIN ... ON <one-sided predicate>` (e.g. `ON t.col = 'literal'` or
`ON t.col > 100`) was being misclassified as an equi-join key by the
executor's join builder (`split_join_condition` + `is_pure_equi_join`
in `src/sql/executor/join.rs`). The hash-join build/probe phases then
collapsed onto a degenerate single-bucket key and emitted full
cross-products instead of correctly-filtered results — silently
returning up to N×M rows where the user expected the filtered
cardinality. Latent since the test that surfaced it landed in commit
`eda2290`.

**Fix.** A new optimizer pass — `JoinPredicatePushdownRule` — splits a
Join's ON clause into conjuncts and pushes left-only / right-only
conjuncts into Filter wrappers above each input, leaving only true
cross-side predicates on the join. Outer-join semantics are preserved:
LEFT/FULL never push left-only predicates; RIGHT/FULL never push
right-only; LATERAL joins are skipped entirely.

### Added — JoinPredicatePushdownRule (`src/optimizer/rules.rs`)

The rule runs as part of the standard optimizer pipeline (registered
between `SelectionPushdownRule` and `ProjectionPruningRule`). It
recurses through `Join`, `Filter`, `Project`, `Sort`, `Limit`,
`Aggregate`, `Union/Intersect/Except`, `With`, and `InsertSelect`,
rewriting every Join it finds. A cheap O(plan-depth) `is_applicable`
pre-filter short-circuits the walk on plans with no joins.

### Added — Predicate-pushdown perf bench

`benches/predicate_pushdown_bench.rs` runs four query shapes (control,
right-only literal, left-only literal, mixed equi+one-sided) twice
each — with and without the new rule — and sanity-checks that both
runs return the same scalar `COUNT(*)` before timing. Validates both
correctness and absence of perf regression.

### Added — OLTP smoke bench

`examples/oltp_smoke.rs` mirrors `pg_vs_helios.py`'s shapes via the
embedded API. Run back-to-back on `main` and `feat/predicate-pushdown`
to confirm no OLTP regression: every metric within run-to-run noise,
INSERT and JOIN paths marginally faster.

### Validation

- 1758/1758 lib tests pass (1746 pre-existing + 12 new optimizer-rule tests).
- 39/39 cte_hardening integration tests pass (the previously-`#[ignore]`d
  `test_basic_cte_used_in_join` is back in the suite, plus a new
  parametric variant `_one_sided_non_constant`).
- `art_index_bench` runs at normal numbers — no regression on
  non-JOIN workloads.

Validated across the full predicate-pushdown matrix with no non-JOIN regressions.

## [3.22.3] - 2026-05-01

### Added — Agentic-operations skill catalogue

Project-scoped catalogue of 17 SKILL.md files under `.claude/skills/heliosdb-nano-*/`
plus a vendor-neutral `AGENTS.md` aggregate at the repo root, designed so
LLM-driven coding agents (Claude Code, OpenAI Codex CLI, MCP-aware tools)
get a complete A→Z verb catalogue for operating the database the moment
they enter the project — no README spelunking required.

Skills (one per domain): `heliosdb-nano-overview`, `-install`, `-connect`,
`-schema`, `-query`, `-transactions`, `-branches`, `-time-travel`,
`-backup`, `-vector`, `-code-graph`, `-graph-rag`, `-mcp`, `-server`,
`-deploy`, `-observability`, `-migrate`. Plus reference helpers
`_index/verb-map.md` (every CLI flag, REPL meta-command, public Rust API,
MCP tool) and `_index/feature-matrix.md` (cargo feature ↔ skill).

Distribution:

- **`git clone`** — Claude Code automatically picks up `.claude/skills/`.
- **`cargo install heliosdb-nano`** + manual install — run
  `bash scripts/install-agent-skills.sh [--symlink]` to publish skills
  into `~/.claude/skills/`. Existing `heliosdb-nano-*` directories there are
  backed up to `*.bak.<unix-ts>` first.
- **Codex / generic agents** — read `AGENTS.md` at project root (the
  de-facto convention).

No code changes — docs/scaffolding only. A future minor release may add
a `heliosdb-nano skills install` subcommand so post-`cargo install` users
don't need a repo checkout to publish skills globally.

## [3.22.2] - 2026-04-30

### Fixed — cross-process `INSERT … ON CONFLICT` no longer duplicates rows

Closes the cross-process `INSERT … ON CONFLICT` duplication bug. A second
process attaching to a KB written by a prior process and issuing
`INSERT … ON CONFLICT(col) DO UPDATE` (or any parameterised
`INSERT` via `db.execute_params`) silently inserted duplicate
rows — `Catalog::rebuild_all_indexes()` had populated the ART, but
the relevant write paths weren't consulting it.

**Root cause** — two divergences:

1. `EmbeddedDatabase::insert_tuple_versioned_with_schema`
   (the storage primitive used by `insert_tuple_branch_aware_with_schema`
   on the main branch and by `execute_plan_with_params_inner`'s INSERT
   arm) **did not call `check_unique_constraints`**. Its SQL fast-path
   sibling `insert_tuple_fast` already did.
2. `execute_plan_with_params_inner`'s `LogicalPlan::Insert` arm
   discarded `on_conflict` (`on_conflict: _`).

**Fix** (commit `6ec74d3`). Added the constraint check to
`insert_tuple_versioned_with_schema` (matching the fast-path twin)
and wired `on_conflict` through `execute_plan_with_params_inner` —
pre-check uniqueness, route `DoNothing` (silent skip) and
`DoUpdate` (UNIQUE-column scan / PK lookup → read existing tuple
→ resolve `EXCLUDED` refs and apply assignments via a
parameter-aware `Evaluator` → write back via `update_tuple_fast`).

New test `tests/cross_process_index_rebuild_tests::on_conflict_named_column_upserts_after_reopen`
reproduces the original FR repro and now passes alongside 6
existing cross-process tests + 10 in-lib `on_conflict` tests.

### Added — MCP server LRU cache stats in `helios/info` + `GET /mcp/info`

The discovery payload now includes a `cache` sub-object covering
the engine's MCP `result_cache` (server-side LRU for read-only
`tools/call` results, 256-entry, 5-minute TTL):

```json
{
  "cache": {
    "size": 42, "capacity": 256, "generation": 7,
    "hits": 1284, "misses": 201, "evictions": 0, "hit_rate": 0.864
  }
}
```

`hit_rate` is `0.0` when no requests have been served yet (avoids
div-by-zero). `generation` increments on every mutating tool call
(invalidates the cache). Both transports — JSON-RPC `helios/info`
and HTTP `GET /mcp/info` — surface the same field.  New test
`tests/mcp_introspection::helios_info_rpc_includes_cache_stats`.

Commit `26956ba`.

### Fixed — stale `heliosdb-nano mcp-server` CLI doc references

`docs/code_graph/{pilot,troubleshooting}.md` plus a handful of
source comments still showed CLI examples like
`./bin/heliosdb-nano mcp-server --db ...` that fail with
"unrecognized subcommand". The engine deliberately has no
`mcp-server` CLI subcommand — MCP is a library integration
consumed by the out-of-tree
[`heliosdb-codekb-mcp`](https://github.com/HeliosDatabase/HeliosDB-CodeKB-MCP)
plugin, which exposes `serve --source X [--http <addr>]`. Doc
content updated to redirect users at the plugin.

Commit `60f0460`.

## [3.22.1] - 2026-04-29

### Fixed — phantom FK violation on `DELETE child; DELETE parent` inside a transaction

Closes the code-index publish-blocker (phantom in-txn FK violation): every
`code_index` call against a
populated KB raised
`fk__hdb_code_symbol_refs_from_symbol___hdb_code_symbols`
on the second of a `DELETE child WHERE …; DELETE parent WHERE …;`
sequence inside a single transaction — even though the first DELETE
removed every offending child row in the same txn.

**Root cause.** `EmbeddedDatabase::check_referencing_rows_exist`
called `storage.scan_table` directly, which reads RocksDB committed
state. The just-tombstoned child rows lived in the txn's write-set,
not yet committed, so they were still visible to the FK validator.

**Fix.** The DELETE plan executor passes the active
`storage::Transaction` through to `check_referencing_rows_exist`,
which now calls `txn.merge_with_write_set(table_name, base)` on the
scan result so in-txn DELETEs / INSERTs are reflected in the FK
check. Equivalent to PG's read-your-own-writes semantics for FK
validation.

**Hang during fix iteration.** The first cut re-acquired
`current_transaction.lock()` from inside `check_referencing_rows_exist`,
which deadlocked because the DELETE caller already held that mutex.
Resolved by threading the txn ref through as an
`active_txn: Option<&Transaction>` parameter — no extra lock
acquisition.

**Acceptance fixtures.** `tests/code_graph_phase2.rs` now has the
three populated-KB workflow tests the field-bench bug report
requested: `ingest_twice_against_populated_kb` (SHA gate
short-circuits), `ingest_twice_with_one_changed_file_against_populated_kb`
(per-file delete-stale runs for the changed file — was the failing
case), and `force_reparse_against_populated_kb`. All pass.

### What this means for the daily workflow

The pilot's silent-fail matrix is closed:

| Workflow                              | v3.22.0          | **v3.22.1**       |
|---------------------------------------|------------------|-------------------|
| First-time `init --ingest`            | ✓ works          | ✓ works           |
| `/helios-code-graph:refresh` (daily)  | ❌ silent fail   | **✓ works**       |
| Git post-commit hook                  | ❌ silent fail   | **✓ works**       |
| `--force` re-parse                    | ❌ silent fail   | **✓ works**       |

### Known interaction — cross-process ON CONFLICT bug × FK validator cost

The FK validator's `merge_with_write_set` walks the txn's
DashMap write-set on every check. When the cross-process
`INSERT ... ON CONFLICT (path) DO UPDATE` bug
(the cross-process duplication regression) doubles the
client's `src` table across re-runs, the indexer's
duplicate-path defense triggers per-file delete-stale on the
second occurrence of every path, and each FK check pays an
O(write_set_size) cost. On Nano's own corpus this turns a
~3-minute force-reparse into 30+ minutes. The correctness
issue this release fixes is **independent**: even on a
single-process KB without the cross-process bug, the FK
violation reproduced (see the new `ingest_twice_with_one_changed_file_against_populated_kb`
fixture). Closing the cross-process bug will eliminate the
duplicate-path defense path entirely.

## [3.22.0] - 2026-04-29

### Added — code_index write-phase optimisations (Tier 1 + indexes + Tier 2.4 v2)

Field-driven follow-up to the v3.21.0 parallel-parse work. The pilot
showed write-phase wall time was 95% of total ingest cost; v3.21.1
attacks that directly without compromising OLTP/ACID. ACID-positive
across the board (ingests are now atomic per chunk; force-reparse is
atomic against the populated KB).

- **Tier 1.1 — single-transaction write phase.** Per-chunk
  `BEGIN ... COMMIT` around `code_index`'s write loop +
  `cross_file_resolve`. Engine pays one WAL fsync per chunk instead
  of per-statement (tens-of-thousands → tens). With `chunk_size =
  None` (default) the whole ingest is one atomic commit. The
  indexer auto-detects an outer transaction via `db.in_transaction()`
  and honours it (skips its own begin/commit) so callers in
  long-running txns aren't blindsided.
  (`src/code_graph/storage.rs::code_index_with_embedder`)

- **Tier 1.3 — TRUNCATE fast path for `force_reparse`.** When the
  caller sets `force_reparse = true` against a populated KB, the
  indexer issues `TRUNCATE _hdb_code_*` once instead of per-file
  DELETE-then-INSERT. Closes the pilot's 1 h 55 m anti-pattern
  outright (the prior path triggered RocksDB compaction storms on
  bulk-delete-followed-by-bulk-insert against 36 K symbols + 178 K
  refs). After truncate the per-file write loop's
  `SELECT-existing/UPDATE-inbound/DELETE-stale-refs/DELETE-stale-symbols`
  preamble is skipped on the first occurrence of each path —
  guarded with a `processed_paths: HashSet<String>` so duplicate
  paths in `source_table` still get the second occurrence's
  preamble (defensive correctness when upstream upserts misbehave).

- **`_hdb_code_*` covering indexes.** `idx_..._symbols_file_id`,
  `idx_..._symbol_refs_file_id`, `idx_..._symbol_refs_to_symbol`,
  `idx_..._symbol_refs_from_symbol` added to `bootstrap_tables`.
  Eliminates the full-table-scan slow query the pilot observed
  (35 s for a 181-row `DELETE … WHERE file_id = X` against ~115 K
  refs). Also accelerates the cross-file resolver's per-symbol
  back-pointer rebinding. Idempotent — picked up automatically on
  the next `code_index` against an existing KB.

- **Tier 2.4 — bulk RocksDB bypass for `_hdb_code_*` writes
  (direct-write variant).** New crate-internal
  `EmbeddedDatabase::bulk_insert_tuples` primitive: builds `Tuple`
  rows in column order, allocates row_ids in batch, writes
  *directly* to RocksDB via `storage.put` (NOT through the active
  transaction's `write_set`), then updates ART indexes. Mirrors
  the convention `execute_plan_with_params` already uses for
  parameterised `INSERT … RETURNING` so subsequent SQL statements
  in the same outer txn don't pay the O(N) `merge_with_write_set`
  cost — the gotcha that killed an earlier v1 attempt (see
  "Earlier rejected variant" below). `insert_symbols` /
  `insert_refs` in the code-graph indexer route through this
  primitive instead of multi-row `INSERT … VALUES … RETURNING`.
  Trade-off documented in the doc comment: rows are NOT rolled
  back if the outer txn aborts, which is fine for engine-owned
  `_hdb_code_*` tables (the next force-reparse TRUNCATEs them
  anyway) but is why this primitive is `pub(crate)` and gated
  behind a "engine-owned tables only" caveat. User-facing tables
  keep going through `execute()` so triggers / RLS / FKs /
  rollback are honoured.

### Field-bench results — Nano's own `src/` (663 files, 18 425 symbols, 114 784 refs)

| Run                                                       | Wall    | Write   | Notes                                   |
|-----------------------------------------------------------|--------:|--------:|-----------------------------------------|
| Tier 1 only, cold ingest                                  |  7:24   |  6:31   | parse 3.1 s = 0.7 % of total            |
| Tier 1 + indexes, cold ingest                             |  5:39   |  4:46   | indexes save 27 % of cold time          |
| Tier 1.3 + indexes, force-reparse on populated KB         |  4:00   |  2:57   | 1.83× faster than cold                  |
| **Tier 1+2.4 v2, cold ingest**                            | **3:36** | **2:44** | direct-write bulk, **1.57× over Tier 1+ix** |
| **Tier 1+2.4 v2, force-reparse on populated KB**          | **1:22** | **~1:00** | **2.93× over Tier 1.3 force**           |
| **Tier 1+2.4 v2, force-reparse (warm KB, 2nd run)**       | **1:05** | **~0:50** | RocksDB block cache warm                |
| Prior baseline: force-reparse on populated KB             | KILLED at 1:55:00 | — | the anti-pattern this FR closes  |

(In-sandbox numbers; user's host clocked the v3.20 parallel-only
baseline at 5:43 cold.)

### Tests — `tests/code_graph_parallel_index.rs` (now 9, all green)

- New: `force_reparse_against_populated_kb_truncates` — content
  parity (file paths, symbol signatures, ref signatures) between
  cold and force-reparse-after-cold runs. ID columns are
  intentionally NOT compared because Nano's TRUNCATE preserves
  row-id counters (matching Postgres' default).
- New: `write_phase_under_explicit_outer_txn_succeeds` — proves the
  outer-txn-honouring branch produces the same row counts as the
  self-managed branch.

### Honest bug findings (filed as separate follow-ups)

Two pre-existing engine issues surfaced during field benchmarking
that the v3.21.0 work didn't introduce but did expose. Filed for
their own FRs; tracked here for visibility:

1. **Cross-process ON CONFLICT on PK doesn't detect prior committed
   rows.** Re-running a path through `INSERT … ON CONFLICT (path)
   DO UPDATE SET …` from a fresh process duplicates rows instead of
   updating. The v3.21.0 eager-ART-rebuild on `EmbeddedDatabase::open`
   *does* re-register the PK index but the conflict-detection path
   appears to bypass it for cross-process state.
2. **FK constraint sees pre-DELETE state inside a transaction.**
   `BEGIN; DELETE FROM _hdb_code_symbol_refs WHERE …; DELETE FROM
   _hdb_code_symbols WHERE …; COMMIT;` raises a from_symbol FK
   violation on the second DELETE despite the first DELETE removing
   every offending ref *in the same txn*.

### Earlier rejected variant — Tier 2.4 v1 (txn write_set buffered)

The first cut at the bulk primitive routed writes through
`txn.put` when an outer transaction was active. Cold ingest
dropped 5:39 → 2:40 (write phase 3.4×) — but the same KB's
`--force` re-ingest regressed 4:00 → 8:03 (~2× slower). Root
cause: with `chunk_size = None` the entire ingest's writes
accumulate in the txn's DashMap `write_set` (~133 K entries on
this corpus). Each subsequent SQL `DELETE … WHERE file_id = X`
inside the same outer txn falls through `scan_table_filtered` →
`merge_with_write_set`, which iterates and bincode-deserialises
*every* `write_set` entry on every scan call. With ~665 second-
occurrence delete-stale calls (forced by the duplicate-path
defense for the cross-process ON CONFLICT bug surfaced in this
release), the cumulative cost was O(N²) in `write_set` size —
~7 minutes of pure deserialisation work. The fix in v2 is to
write directly to RocksDB the way `execute_plan_with_params`
already does for `INSERT … RETURNING` (`storage.put`, bypassing
`txn.write_set`); subsequent SQL DELETEs read committed rows
from the RocksDB snapshot at no extra per-row cost. v2 fixes
both axes — see field-bench table above.

## [3.21.0] - 2026-04-28

### Added — parallel `code_index` (FR `parallel_code_index`)

`code_graph::storage::code_index` is now split into a single-threaded
**triage** pass (classify rows into skipped / unchanged / to-parse +
hash-gate), a parallel **parse + extract + in-file resolve** phase
running on a *dedicated* `rayon::ThreadPool`, and a single-threaded
**write** phase that walks results in input order. Closes
`FEATURE_REQUEST_parallel_code_index.md`.

- **Dedicated thread pool** — never the global one. Daemon-mode
  servers handling live OLTP traffic see no thread starvation
  during code indexing.
- **`CodeIndexOptions::parallelism: Option<usize>`** caps the
  worker count (default `min(num_cpus, 8)`, `Some(1)` forces
  serial). Operators on shared hosts can cap the indexer's
  footprint to protect query latency.
- **Per-OS-thread `tree_sitter::Parser` cache**
  (`src/code_graph/parse.rs`). `thread_local!` `RefCell<HashMap<…,
  Parser>>` reused across files within a worker — no
  `set_language` overhead per row.
- **Optional bounded-memory chunking** via
  `CodeIndexOptions::chunk_size: Option<usize>`. `None` =
  single-chunk all-in-one (default; max throughput; fits the
  ~1.8 K-file pilot scale). `Some(n)` = drain rows into batches
  of `n`, parse each chunk in parallel, write before moving on —
  bounds peak RAM at `n × avg_parsed_file_bytes` for ≥ 10 K-file
  corpora. Equivalence with the unchunked path is asserted by
  `chunked_output_matches_unchunked`.
- **`CodeIndexStats` telemetry**: `parse_elapsed_ms`,
  `write_elapsed_ms`, `parse_workers`, `chunks_processed`. Lets
  operators see the speed-up + memory-mode in the binary's own
  output.
- **Byte-identical** to the serial path: equivalence locked down
  by `tests/code_graph_parallel_index.rs` against an 8-file
  multi-language fixture (parallelism=1 vs parallelism=8, and
  chunked vs unchunked). 7 unit tests, all green.
- **Field-benchmarked** in release on Nano's own `src/` tree
  (352 Rust files, 12-core host, 8 workers): total wall-clock
  **49.8 s** (under the 2-minute FR budget); parse-phase speedup
  **1.97×**. Parse utilisation caps at ~2× because of allocator
  + grammar-init contention — not the worker pool — and is
  documented as out-of-scope for this FR. See
  `tests/code_graph_parallel_index_bench.rs`
  (`#[ignore]`-gated; run via `cargo test --release --features
  code-graph --test code_graph_parallel_index_bench --
  --ignored --nocapture`).

### Added — engine-level fixes for the v3.20 honest gaps

These are general-purpose engine improvements, not SQLite-shim work. Each
benefits every protocol (PG wire, MySQL wire, embedded REPL, embedded API)
and every mode (single-connection, daemon-server, HA standby).

- **Eager ART index rebuild on open** (`Catalog::rebuild_all_indexes`,
  wired into `EmbeddedDatabase::new` and `EmbeddedDatabase::with_config`
  for non-memory storage). A fresh process attaching to an existing data
  dir now re-registers PK / UNIQUE / FK index structures from the
  persisted schemas and replays existing rows through `on_insert` so
  in-memory ART matches on-disk state. Cost: O(rows + indexes) at
  startup; zero impact on the OLTP hot path. Closes the cross-process
  embedded consistency gap. Persistent index pages backed by a RocksDB
  column family are tracked separately for v3.22+.

- **PostgreSQL date/time function audit** (`src/sql/evaluator.rs`). Added
  `TO_CHAR(date, fmt)`, `TO_DATE(text, fmt)`,
  `TO_TIMESTAMP(epoch | text, fmt)`, `DATE_TRUNC(field, value)`,
  `DATE_PART(field, value)` (alias for EXTRACT), `AGE(t1, t2)`,
  `MAKE_DATE(y, m, d)`, `MAKE_TIMESTAMP(y, m, d, h, mi, s)`. PG format
  codes `YYYY/YY/MM/MON/Mon/mon/DD/DDD/DAY/Day/day/DY/HH24/HH12/MI/SS/MS/US/IW/IYYY/Q/W/D/AM/PM/am/pm`
  are translated to chrono format; case-significant tokens use a
  post-processing pass so output matches Postgres exactly. SQLite-only
  function names (`STRFTIME`, `JULIANDAY`) are not added — callers that
  need them can rewrite at the SDK layer.

- **Composite-PK on the conflict-detection scan path**
  (`src/lib.rs`). When the ART couldn't locate the conflicting row,
  the planner already retried via PK lookup; with rebuild on open this
  retry now sees the same indexes a fresh process would see.

### Added — SDK shim (`heliosdb_sqlite` 3.0.1+)

- **`cursor.lastrowid`** is now populated automatically. The SDK
  detects `INSERT INTO t (...) VALUES (...)` (incl. `INSERT OR REPLACE`
  / `OR IGNORE`), looks up the table's int PK column once via
  `PRAGMA table_info(t)` (cached on the `Connection`), appends
  `RETURNING <pk>`, and stores the returned value as
  `cursor.lastrowid`. No engine state, no protocol change — pure
  client-side use of standard SQL `RETURNING`. Tables with TEXT PKs
  return `None`, matching sqlite3 semantics. Existing
  `INSERT … RETURNING …` calls are passed through untouched.

- **DSN parsing for daemon mode.** `connect(..., mode='daemon')` now
  honours `HELIOSDB_DSN` (or an explicit `dsn=` kwarg) for
  `host/port/user/password/database`, instead of hard-coding `helios`/
  port 5432.

### Tests
- `tests/cross_process_index_rebuild_tests.rs` — 6 tests proving PK
  lookups, INSERT-OR-REPLACE upserts, and UNIQUE constraint enforcement
  all work after closing and reopening a populated data directory.
- `tests/datetime_functions_tests.rs` — 19 tests covering every PG
  date/time function added in this release plus a realistic OLTP-shaped
  GROUP BY DATE_TRUNC aggregation.
- 1746 lib tests + 11 sqlite_compat hardening + 72 aggregate hardening
  remain green.

## [3.20.0] - 2026-04-28

### Added — SQLite drop-in compatibility

Lifts the dialect ceiling so existing `sqlite3`-driven Python applications
(and other SQLite clients) can talk to Nano with no query rewrites. Combined
with the production-ready `heliosdb_sqlite` Python shim, swapping
`import sqlite3` for `import heliosdb_sqlite as sqlite3` is enough to
retarget most apps.

Engine changes:
- **`?` positional placeholders** — parser-level rewrite to PG-style `$N`
  with quote/comment/dollar-quote awareness; mixed `?`/`$N` in a single
  statement is rejected.
- **`INSERT OR REPLACE INTO t (cols) VALUES …`** → expanded to
  `INSERT … ON CONFLICT DO UPDATE SET col = EXCLUDED.col, …` so the same
  upsert semantics apply to PG-wire and embedded REPL clients.
- **`INSERT OR IGNORE INTO …`** → expanded to
  `INSERT … ON CONFLICT DO NOTHING`.
- **`INTEGER PRIMARY KEY AUTOINCREMENT`** → mapped to `BIGSERIAL PRIMARY KEY`
  in DDL.
- **`DATETIME('now')`** → recognised as `CURRENT_TIMESTAMP`.
- **`sqlite_master` system view** with the SQLite column shape
  (`type, name, tbl_name, rootpage, sql`).
- **`PRAGMA` shims** — `table_info(t)` returns SQLite-shaped rows
  (`cid, name, type, notnull, dflt_value, pk`); connection-tunable PRAGMAs
  (`foreign_keys`, `journal_mode`, `synchronous`, `busy_timeout`) are
  accepted as no-ops, intercepted at the protocol layer and at
  `EmbeddedDatabase::execute/query`.
- **Composite-column `CREATE INDEX`** is now accepted instead of erroring
  with the misleading "Multi-column vector indexes" message — only the
  leading column is indexed today (B-tree), but the DDL no longer breaks
  sqlite3 schema migrations. Vector-index variants (`USING hnsw|ivf|…`)
  still reject multi-column.

### Fixed
- **ON CONFLICT DO UPDATE within an explicit transaction** could fail with
  *"existing row not found in storage"* when the conflicting row was
  inserted earlier in the same transaction. The conflict path now reads
  through `txn.get(...)` so write-set rows are visible.

### Tests
- `tests/sqlite_compat_tests.rs` — 11 hardening tests covering each item
  above end-to-end against `EmbeddedDatabase`.
- `src/sql/sqlite_compat.rs` — 16 unit tests for the parser-level
  rewrites (placeholder quote-awareness, multi-statement boundaries,
  PRAGMA detection).

### Honest status
- **Cross-process embedded mode** (one `heliosdb-nano repl` per Connection,
  data shared via `--data-dir`) does not rebuild the in-memory ART
  indexes on startup, which means a fresh process can't see the unique /
  PK constraints registered by a prior process and falls back to scan
  paths. Single-connection embedded use is the recommended path; daemon
  mode (one `heliosdb-nano start`, many `psycopg2` connections) is the
  recommended scale-up.
- The Python SDK shim absorbs the rest: SQL-string positional binding for
  `?`, table-output parser for box-drawn results, PRAGMA-as-query
  routing, and a non-blocking `__del__` so finalisation doesn't time out
  on slow rollbacks.

See `docs/compatibility/sqlite.md` and
`docs/guides/sqlite_drop_in_tutorial.md` for the full feature matrix
and a runnable port walkthrough.

## [3.19.1] - 2026-04-25

### Fixed — UUID literal coercion at PK index lookup (#205)

Resolves the CloudV2 admin_db "INSERT-then-SELECT-misses-the-row"
bug.  Root cause was not the COMMIT path / deadpool recycling /
3.14.9 regression the original investigation theorised — it was a
planner literal-typing bug.

`SELECT … WHERE id = '<uuid>'` against a UUID-typed PK emitted
`Value::String("<uuid>")` regardless of the column's declared
type.  The ART point-lookup encoded the search key by Value
variant, so Value::String and Value::Uuid produced different
encoded keys → the lookup missed every row.

Three patches land the fix:
- `src/sql/executor/mod.rs::try_index_point_lookup` — coerce the
  literal to the PK column's type before the ART lookup. New
  helper `coerce_literal_to_column_type` handles
  String→UUID/Date/Timestamp.
- `src/lib.rs::fast_parse_one_value` — same coercion at the
  fast-select parse layer for `SELECT *` queries.
- `src/storage/simd_filter.rs::compare_eq` — Uuid↔String
  cross-type case so the SIMD post-walk filter also matches.

Verified by `tests/uuid_where_repro.rs` (direct API) and
`tests/persistence_repro.rs` (wire protocol, no longer
`#[ignore]`d).  All 1842 lib tests + every prior integration
suite remain green.

CloudV2 follow-ups: revert admin_db SELECT-all workarounds,
drop the daily restart cron, graduate `cloud-v2.heliosdb.com`
to production.

## [3.19.0] - 2026-04-25

### Added — backlog sweep #181-#193

Closes the residual FR backlog the v3.18 review left open.  Each
task carries a context doc under `docs/followups/`.

- **#181 `hdb_code_languages` system view** — exposes
  `SupportedLanguage::all()` + `parse::registered_grammars()` as a
  queryable SQL view.  Runtime grammars shadowing a static tag
  report `source = 'runtime'`.
- **#182 `body_vec VECTOR(n)` column** materialised on
  `_hdb_code_symbols` lazily on first non-null embedding.
  Dimension negotiated at insert time; `code_index_with_embedder`
  is the new public entry point that takes a pre-constructed
  `Box<dyn Embedder>`.
- **#183 SymbolExtractor pluggability** — runtime-registered
  grammars can ship with paired extractors via
  `EmbeddedDatabase::register_extractor`, so dynamic languages
  produce real symbols instead of empty parse trees.
- **#184 HTTP POST + SSE progress pairing** — process-static
  session table keyed by `Mcp-Session-Id`.  POSTs that pair with
  an open SSE channel get their `notifications/progress` events
  forwarded to the SSE stream while the POST returns the final
  `tools/call` response.
- **#185 `helios_lsp_rename_apply`** — write-back side of the
  preview tool; identifier-boundary aware, sha256 conflict check,
  optional dry_run.
- **#186 Docling content-conversion ingestion** —
  `graph_rag_ingest_pdf / _office / _audio / _image` POSTs to
  docling-serve, parses the DoclingDocument JSON, and projects
  sections + chunks + tables under `_hdb_graph_nodes` with
  CONTAINS edges.  Idempotent via source_ref keys.
- **#187 `code-embed` feature** — fastembed-rs as the in-process
  embedder.  Default model BGESmallENV15 (384-dim, ~30 MB cache
  on first run).  No on-disk impact on the binary itself.
- **#188 `_hdb_code.*` / `_hdb_graph.*` dotted namespacing
  aliases** — planner-level rewrite at
  `normalize_object_name`; `pg_tables.schemaname` reports the
  schema split.  Catalog keys remain flat (full refactor tracked
  separately).
- **#189 Scope-chain resolver via IMPORTS** — unresolved CALLS /
  REFERENCES refs upgrade to `Exact` when an unambiguous IMPORTS
  edge in the same file ends in the bare name.  Handles Rust
  `use foo::bar`, Python `from foo import bar`, TypeScript
  `import { bar } from './foo'`, Go imports.
- **#190 Centrality-biased + prefilter-aware HNSW wrapper** —
  over-fetches `k * over_fetch_multiplier` candidates, applies
  the row-level prefilter, re-scores with
  `(1 - α) × distance - α × centrality`.  Equivalent to the FR's
  Option B (post-rerank) lift; in-descent navigation bias is a
  separate phase-3.1 follow-up.
- **#191 Acceptance benchmarks** — `with_context_bench` (10k-node
  fixture, 100-query mean ≤ 500 ms) and `linker_precision`
  (≥ 80 % on a hand-labelled fixture).  Current run: mean 62 ms,
  precision 100 %.
- **#192 FR-6 pilot deployment** — `scripts/install-nano-pilot.sh`
  + `docs/code_graph/{pilot,troubleshooting}.md`.
- **#193 Build report** — `docs/followups/build-report.md`
  captures the all-features release binary metadata
  (35.0 MiB, sha256 `41176528…`, rustc 1.92.0).

### Added — code-graph / graph-rag / MCP follow-ups

Closes nine of the gaps a downstream client raised against the v3.18
merge (`feat/code-graph-phase1` → `main`).  All additive; no public
API breakage.

- **#1 `helios_lsp_document_symbols`** — file outline ordered by
  line, optional kind filter.
- **#2 `helios_lsp_rename_preview`** — preview-only edit list
  (definition + every reference site); never writes back.
- **#3 `helios_graphrag_search`** — wraps the embedded
  `graph_rag::graph_rag_search` Rust API as an MCP tool. The
  flagship cross-modal query is now reachable over JSON-RPC, not
  just over SQL.
- **#4 `helios_lsp_references_diff` / `helios_lsp_body_diff` /
  `helios_ast_diff`** — wraps the existing `diff::*` Rust API.
  Accepts AS OF refs as `{"now": true}`, `{"commit": "sha"}`,
  `{"timestamp": "iso"}`.
- **#5 FR-3 `ON BRANCH '<name>'` per-call override** on
  `lsp_*(...)` table functions. RAII branch guard restores the
  prior branch on every early-return path. Combines with `AS OF`
  in either order.
- **#6 `CREATE SEMANTIC HASH INDEX [IF NOT EXISTS] <name>`** DDL
  surfaces the existing `code_graph::merkle_refresh` Rust
  primitive at the SQL layer (FR 4 §4.6).
- **#7 `graph_rag_link_vector`** — vector-similar entity-linker
  stage (FR 4 §4.3 strategy 3). Takes caller-supplied
  `(node_id, vector)` pairs on both sides; runs cosine top-k with
  threshold gating; emits MENTIONS edges with
  `weight = similarity`.
- **#8 `tools/list?verbose=true`** + **`helios/info` JSON-RPC
  method** + **`GET /mcp/info` HTTP route** — single-shot
  discovery payload (serverInfo + capabilities + verbose tool
  catalogue + resource list).
- **#9 Streaming `notifications/progress` events.** Tools that
  call `mcp::progress::emit` from anywhere on the call stack
  produce JSON-RPC `notifications/progress` messages when the
  client opted in via `_meta.progressToken`. Wired into the
  WebSocket and stdio transports; HTTP POST stays single-shot
  (use the SSE channel for streaming there). The streaming
  dispatcher (`mcp::call_tool_streaming`) runs the sync handler
  on `spawn_blocking` and forwards events through an unbounded
  channel. `helios_graphrag_search` is the first tool wired —
  emits a "seeding" event on entry and a "<n> hits" event on
  exit so agents can render a progress indicator.
- **`SupportedLanguage`** alignment: enum now mirrors `Language`
  (Rust / Python / TypeScript / Tsx / JavaScript / Go / Markdown /
  Sql) so the planned `hdb_code.list_languages` system view
  doesn't lie about the static set. `SupportedLanguage::all()`
  + `From<Language>` conversion added.

### Tests

- 9 follow-up integration tests in `tests/mcp_followups.rs`.
- 4 introspection tests in `tests/mcp_introspection.rs`.
- 2 ON BRANCH integration tests in `tests/code_graph_on_branch.rs`.
- 2 semantic-hash DDL tests in
  `tests/code_graph_semantic_hash_ddl.rs`.
- 4 vector-similar linker tests in
  `tests/graph_rag_linker_vector.rs`.
- 4 progress-streaming integration tests in
  `tests/mcp_progress.rs` (WS round-trip with token, no token,
  numeric token, scalar-token contract) + 3 lib unit tests
  covering the channel sink + thread-local routing.
- 6 new `sql_rewrite` unit tests for `ON BRANCH` parsing
  (combinations, escape, tie-break) + 3 unit tests for
  `detect_create_semantic_hash_index`.

### Ratified deviations from FR text

These design choices in v3.15–v3.18 were reviewed by a downstream
client; we ratify them here as the intended end-state rather than
TODOs:

- **Flat-prefix tables (`_hdb_code_*` / `_hdb_graph_*`) instead of
  dotted schemas (`_hdb_code.*`).** Simpler bootstrap; no catalog
  refactor required. Promotion to real schema namespacing is a
  separate engine-wide refactor that benefits `pg_catalog` too,
  not part of the code-graph track.
- **Cargo features (`code-graph` / `graph-rag` / `mcp-endpoint`)
  instead of runtime `CREATE EXTENSION`.** Build-time opt-in; no
  per-process activation step; the static grammar set is fixed at
  build but the runtime grammar registry
  (`src/code_graph/parse.rs::register_grammar`) covers the dynamic
  plug-in case (caller supplies the loader — wasm runtime,
  dynamically-linked grammar, etc.).
- **Centrality is a post-rerank weighting, not an HNSW navigation
  bias** (`src/graph_rag/centrality.rs:10`). Ships the smaller
  relevance lift but avoids forking `hnsw_rs`. Descent-bias is a
  separate phase-3.1 follow-up if the relevance gap turns out to
  matter in the pilot.

### Known follow-ups

- **HTTP POST progress** — `POST /mcp` is request/response so
  streaming requires the paired SSE channel; not yet wired.
  WebSocket and stdio cover the streaming case today.

## [3.18.0] - 2026-04-24

### Added — MCP endpoint phase 4 MVP (FR 5, opt-in, feature = "mcp-endpoint")

First landing for the native MCP endpoint. Ships a JSON-RPC 2.0
dispatcher on top of the existing `src/mcp_extensions/` tool
catalogue so an MCP-capable agent (Claude Code, Cursor, Continue,
Codex, Aider) can drive HeliosDB-Nano with no wrapper process.

- New Cargo feature `mcp-endpoint`. Additive — embedded-only
  callers compile without it.
- New module `src/mcp_http/` with two files:
  - `rpc.rs` — `handle_rpc(req) → resp`, pure function over JSON-RPC
    `initialize`, `tools/list`, `tools/call`, `ping`. Unknown methods
    return the canonical `-32601 Method not found`.
  - `mod.rs` — re-exports.
- Tool catalogue: every tool already registered in
  `mcp_extensions::tools::list_tools()` is surfaced automatically.
  `heliosdb_bm25_index`, `heliosdb_hybrid_search`,
  `heliosdb_graph_add_edge`, `heliosdb_graph_traverse`,
  `heliosdb_graph_path`, `heliosdb_embed_and_store`.
- Server-info handshake reports `{"name":"heliosdb-nano","version":<pkg>}`.

Explicit non-goals for the MVP (tracked for follow-ups):
- WebSocket / SSE framing — HTTP JSON-RPC only.
- Repair of legacy `src/mcp/` module — `BLOCKER_mcp_legacy.md`
  stays accurate. Phase 4 deliberately does not touch it; the
  MVP handler backs onto the already-working `mcp_extensions/`
  crate directly.
- Axum route wiring — we ship `handle_rpc` as a pure function so
  embedders mount it on whatever route / auth surface they want.
- Macro-driven auto-registration of `lsp_*` / `graph_rag_*` as MCP
  tools (the tool catalogue remains the six-tool `mcp_extensions`
  set for now).

Regression coverage:
- 4 new unit tests (`src/mcp_http/rpc.rs`): `initialize`,
  `tools/list`, unknown method, `tools/call` without name.
- 4 new integration tests (`tests/mcp_endpoint_phase4.rs`):
  canonical handshake, real tool call, unknown tool as
  `isError=true`, ping.

## [3.17.0] - 2026-04-24

### Added — Graph-RAG phase 3 MVP (opt-in, feature = "graph-rag")

First landing for the universal cross-modal graph. Still embedded
Rust API; SQL-level `WITH CONTEXT` clause, graph-weighted HNSW
tie-breaking, and semantic-Merkle invalidation are follow-ups.

- New Cargo feature `graph-rag` (implies `code-graph`).
- New module `src/graph_rag/` (`mod.rs`, `schema.rs`, `search.rs`).
- `_hdb_graph_nodes` and `_hdb_graph_edges` tables bootstrapped on
  first call. Plain user tables; queryable and joinable.
- `EmbeddedDatabase::graph_rag_project_symbols()` — project every
  row of `_hdb_code_symbols` into `_hdb_graph_nodes` + every
  resolved row of `_hdb_code_symbol_refs` into `_hdb_graph_edges`.
  Idempotent. Tolerates the code-graph tables being absent (no-op
  when nothing to project).
- `EmbeddedDatabase::graph_rag_search(opts)` — seed → BFS expand →
  return subgraph with hop distances. `seed_text` matches title/
  text case-insensitively; `seed_kinds` + `edge_kinds` push down
  through `FilteredScan` so bloom / zone-map / SIMD selection
  applies automatically.

Regression coverage:
- `tests/graph_rag_phase3.rs`: 3 tests —
  `project_and_search_finds_symbol`, `empty_seed_text_errors`,
  `bfs_respects_hops`.

Explicitly out of scope for phase 3 (tracked for phase 3.1):
hybrid-search + vector rerank on seeds, graph-weighted HNSW
tie-breaking, semantic-Merkle index, `WITH CONTEXT` SQL clause,
corpus ingestion adapters (`ingest_docs` etc.), entity linker for
cross-modal MENTIONS.

## [3.16.0] - 2026-04-24

### Added — Code-graph phase 2 (opt-in, feature = "code-graph")

- `CREATE EXTENSION hdb_code` DDL. Parses through the standard
  planner, runs the code-graph bootstrap, and marks the extension
  installed in the process. `IF NOT EXISTS` with an unknown extension
  is a silent no-op (matches PG's permissive migration behaviour).
- TypeScript / JavaScript / TSX grammar support via
  `tree-sitter-typescript`. `Language` enum extended; symbol
  extractor handles `function_declaration`, `method_definition`,
  `class_declaration`, `abstract_class_declaration`,
  `interface_declaration`, `type_alias_declaration`,
  `enum_declaration`.
- Cross-file symbol resolver. After the per-file pass,
  `code_index` rebinds every `resolution='unresolved'` edge against
  a corpus-wide name index. Single match → `exact`, multiple → the
  first with `heuristic`.
- New `LogicalPlan::{CreateExtension, DropExtension}` variants;
  `DropExtension` is reserved for forward compatibility (sqlparser
  0.53 doesn't expose `DROP EXTENSION`).

Regression coverage:
- `tests/code_graph_phase2.rs`: 5 new integration tests —
  `typescript_extracts_class_and_method`,
  `create_extension_hdb_code_bootstraps_tables`,
  `create_extension_unknown_errors`,
  `create_extension_unknown_if_not_exists_is_noop`,
  `cross_file_ref_resolves`.

## [3.15.0] - 2026-04-24

### Added — Code-graph track, phase 1 (FR 2 MVP, opt-in)

New opt-in feature `code-graph` that turns HeliosDB-Nano into an
embedded code-graph for AI coding agents. Phase 1 ships the
foundational Rust API — wire-level DDL (`CREATE EXTENSION hdb_code`,
`CREATE AST INDEX`) and temporal queries land in phase 2.

- New Cargo feature `code-graph` pulling
  `tree-sitter = "0.23"`, `tree-sitter-rust`, and `tree-sitter-python`
  as optional deps. Default builds pull none of them; the default
  release binary stays the same size.
- New module `src/code_graph/` with a minimal in-file AST + symbol
  extractor for Rust and Python. Adds:
  - `EmbeddedDatabase::code_index(opts)` — parse every row of a user
    table `(path TEXT PK, lang TEXT, content TEXT)` and populate the
    `_hdb_code_*` tables idempotently.
  - `EmbeddedDatabase::lsp_definition(name, hint)` — "where is X defined?"
  - `EmbeddedDatabase::lsp_references(symbol_id)` — "who uses X?"
  - `EmbeddedDatabase::lsp_call_hierarchy(symbol_id, direction, depth)` —
    BFS over the `CALLS` edges.
  - `EmbeddedDatabase::lsp_hover(symbol_id)` — signature lookup.
- New tables created automatically on first `code_index` call:
  `_hdb_code_files`, `_hdb_code_symbols`, `_hdb_code_symbol_refs`.
  Plain user tables — queryable, joinable, branch-aware.
- Pluggable embedding surface (`src/code_graph/embed.rs`):
  `NoopEmbedder` (default) and `HttpEmbedder` for external endpoints
  matching `POST {"input": "..."} → {"embedding": [...]}`. Nano ships
  no inference runtime; by design, all inference is external.
- Storage-level filtering is the competitive lever: every `lsp_*`
  query pushes its WHERE through the existing `FilteredScan` path in
  `src/storage/predicate_pushdown.rs`, so bloom-filter / zone-map /
  SIMD selection applies without new code.

Out of scope for phase 1 (tracked for phase 2+ in the track docs):
`CREATE EXTENSION` DDL, `CREATE AST INDEX` DDL, real schema
namespacing, temporal / branch variants, incremental reparse,
semantic-Merkle subtree hashes, `WITH CONTEXT` clause, native MCP
endpoint.

Regression coverage:
- 12 new module-level unit tests (parser, symbol extraction,
  in-file resolver, embedder).
- 6 new integration tests at `tests/code_graph_mvp.rs`:
  `rust_lsp_definition_finds_function`,
  `lsp_references_returns_call_sites`,
  `lsp_call_hierarchy_incoming_terminates`,
  `lsp_hover_returns_signature`,
  `code_index_is_idempotent`,
  `unknown_lang_is_skipped_cleanly`.

Docs: `docs/code_graph/overview.md`.

## [3.14.10] - 2026-04-23

### Fixed — Foreign key validation with quoted identifiers, fast-path bypass (B36)

**Reporter's symptom.** `INSERT INTO "workspaces" (name, owner_id)
VALUES (…)` over the extended protocol failed with
`ERROR: Table '"users"' does not exist`, while the unquoted
`INSERT INTO workspaces (…)` silently succeeded even with a
nonexistent parent row. Drizzle emits every identifier quoted, so
every Drizzle-shaped INSERT on an FK-bearing table tripped this.

Two interlocking bugs:

**Root cause #1 — FK references stored with literal quote
characters.** `src/sql/planner.rs` built
`TableConstraint::ForeignKey` via `ObjectName::to_string()` at both
the inline `ColumnOption::ForeignKey` site and the table-level
`SqlTC::ForeignKey` site. `ObjectName::to_string()` preserves the
original quoting, so `REFERENCES "users"("id")` stored the
referenced table as the four-character string `"users"` (with the
quotes). FK validation later called `get_table_schema(&fk.references_table)`
and emitted the verbatim `Table '"users"' does not exist`.

Fix: normalise every identifier at FK construction time with the
same `Planner::normalize_ident` / `Planner::normalize_object_name`
helpers every other DDL path uses. `REFERENCES "users"("id")` and
`REFERENCES users(id)` both now store `references_table = "users"`
and `references_columns = ["id"]`.

**Root cause #2 — `try_fast_insert` skipped FK validation.**
`src/lib.rs::try_fast_insert` wrote rows directly to storage with no
call to `check_fk_constraints` / `check_foreign_key_exists`. Unquoted
INSERTs into FK-bearing tables silently succeeded regardless of
parent-row existence — a data-integrity hole. It also extracted the
target table name with its surrounding quotes intact, so quoted
INSERTs fell out of the fast path entirely and triggered root cause
#1 on the normal path.

Fix: (a) strip surrounding double quotes from the fast-path table
name so quoted and unquoted shapes route identically; (b) bail to the
normal path for any table with registered FK constraints so the
already-validated Insert arm handles the write.

Regression tests (`tests/drizzle_compat_tests.rs`):
- `b36_fk_insert_with_quoted_references` — verbatim Drizzle shape
  (CREATE TABLE with `REFERENCES "users"("id")`, INSERT via extended
  protocol with a valid FK). Must succeed.
- `b36_fk_violation_fires_on_unquoted_insert` — guards the fast-path
  bypass; unquoted INSERT without a matching parent row must fail.
- `b36_fk_violation_fires_on_quoted_insert` — same for the quoted
  shape.
- `b36_fk_succeeds_both_shapes` — both quoted and unquoted INSERTs
  succeed when the FK is satisfied (symmetry guard).

## [3.14.9] - 2026-04-22

### Fixed — GROUP BY correctness with mixed qualifier styles and DATE keys (B35)

**Reporter's symptom.** A Drizzle-emitted analytics query mixing
unqualified column refs in SELECT / CASE bodies with table-qualified
refs in GROUP BY / WHERE:

```sql
select date("check_in"), sum(case when "check_out" is not null ...)
from "time_entries"
where "time_entries"."workspace_id" = $1
group by date("time_entries"."check_in")
```

failed with `Column 'check_in' not found in schema`. Stock PostgreSQL
treats `"check_in"` and `"time_entries"."check_in"` as the same
column when unambiguous.

**Root cause #1 — projection-rewrite matching too strict.**
After planning `Aggregate`, the planner rewrites each SELECT item so
column refs that match a GROUP BY expression become references to the
aggregate operator's output column (`group_N`). The matching step
used `PartialEq`, so `date(Column{table:None,name:"check_in"})` did
not match `date(Column{table:Some("time_entries"),name:"check_in"})`
— the SELECT item's `"check_in"` reference was left as-is and then
failed to resolve against the aggregate's output schema.

Fix: new `Planner::exprs_equivalent` that recursively compares
expressions with qualifier-insensitive `Column` matching. Used at
both sites inside `rewrite_expr_replace_aggregates`.

**Root cause #2 — `compare_values` missing DATE / TIME / INTERVAL /
NUMERIC arms (found while reproducing).**
`GroupKey` in the aggregate operator is ordered via
`compare_values` (src/sql/executor/mod.rs). Any two values without a
dedicated arm fall through to `type_priority`, which returns `Equal`
for any two values of the same type. So `GROUP BY <date-col>` put
every row into a single group (count grew, distinct dates vanished);
`ORDER BY <date-col>` produced non-deterministic output.

Fix: add arms for `Date`, `Time`, `Interval`, `Numeric` in
`compare_values`.

Regression tests (`tests/drizzle_compat_tests.rs`):
- `b35_mixed_qualifier_group_by`
- `b35_both_qualified_group_by`
- `b35_both_unqualified_group_by`
- `b35_reporter_full_shape` (verbatim Drizzle query with SUM + CASE +
  EXTRACT + parameterised WHERE)
- `b35_date_column_group_by_correctness` (guards the second root cause)

## [3.14.8] - 2026-04-22

### Fixed — parameterized LIMIT/OFFSET and UPDATE SET type coercion (B33 / B34)

**B33** — `LIMIT $1 OFFSET $2` was rejected with `LIMIT/OFFSET must
be a number`. Two independent issues surfaced together:

- Wire path: postgres-js binds numeric parameters as TEXT (OID 0 or
  25) by default. `substitute_parameters` renders a string value with
  surrounding single quotes, so the planner saw `LIMIT '3'`, which
  the old `expr_to_usize` rejected.
- In-process path: the planner mapped `$N` to `usize::MAX` as a
  sentinel, but `LogicalPlan::Limit` only carried the sentinel — the
  bound integer never reached the executor. Queries silently returned
  all rows (or all-rows-minus-offset).

Fix:
1. `expr_to_limit_bound` (new) returns `(usize, Option<usize>)`.
   Accepts `Number`, `Placeholder($N)` → `(MAX, Some(N))`, and
   `SingleQuotedString(n)` → `(n, None)`. The quoted-string arm
   matches stock PG's implicit `text → integer` cast for LIMIT /
   OFFSET.
2. `LogicalPlan::Limit` gained `limit_param: Option<usize>` and
   `offset_param: Option<usize>` fields, propagated through the
   optimizer, RLS plan rewrite, and outer-ref binding paths.
3. The executor's Limit branch resolves these from the bound
   parameter list before running any of the Top-K, storage-offset, or
   generic Limit paths.

**B34** — `UPDATE t SET ts_col = $1` via extended-Q silently stored
NULL in TIMESTAMP columns. `sql.unsafe` with the same SQL + string
params worked. INSERT with the same pattern worked.

Root cause: INSERT's value path auto-casts each evaluated value to
its target column type before persistence; UPDATE's SET path did not
— a `Value::String("2026-04-23T10:00:00.000Z")` was pushed straight
into a TIMESTAMP slot, which the storage serializer dropped as an
implicit NULL.

Fix: mirror INSERT's auto-cast in every UPDATE SET path — the
`execute_plan_with_params::Update` arm, the trigger-aware
`execute_in_transaction_inner::Update` arm, and the RLS-aware
non-params Update arm. All three now call `evaluator.cast_value(new,
target_type)` when the new value and column type disagree.

Regression tests (`tests/drizzle_compat_tests.rs`):
- `b33_parameterized_limit`, `b33_parameterized_limit_offset`,
  `b33_quoted_string_limit_wire_substitution`
- `b34_update_set_param_timestamp`,
  `b34_update_set_literal_iso_string`

## [3.14.7] - 2026-04-22

### Fixed — Drizzle UPDATE/DELETE and analytics date ranges (B31 / B32)

**B31** — `UPDATE "t" SET … WHERE "t"."col" = $1` and the equivalent
DELETE fail with `Column 't.col' not found in schema`. Root cause:
the Update and Delete arms of `execute_plan_with_params` (and the
in-transaction variants) build their evaluator directly from the
catalog schema, which does not carry `source_table_name` on its
columns. The SELECT path works because the Scan operator stamps
`source_table_name` on every yielded column; DML didn't.

Fix: new helper `Schema::with_source_table_name(&str)` that stamps
`source_table` and `source_table_name` on every column.
Every single-table DML evaluator now builds its schema through this
helper, so qualified WHERE columns resolve the same way they do for
SELECT. Blocks the stop-timer, edit/delete entry, edit/delete
customer, bulk ops, and role/member management paths.

**B32** — `timestamp >= '2026-04-23T00:00:00.000Z'` (and the `date`
analogue) fail with `Cannot compare Timestamp(…) and String(…)`.
Stock PostgreSQL implicitly casts the literal to the column type;
Drizzle's `gte()` / `lte()` helpers bind JavaScript `Date` instances
as ISO 8601 strings, so every analytics / reporting endpoint hit
this.

Fix: `Evaluator::compare_values` gains four new arms —
`Timestamp ↔ String` and `Date ↔ String` — using the same ISO 8601
/ space-separated / date-only parser as the TIMESTAMP cast path
(`Self::parse_timestamp_string`, `Self::parse_date_string`). Falls
back to string-wise comparison if the literal isn't a valid date /
timestamp, matching the behaviour of the other coercion arms (e.g.
`Int ↔ String`).

Regression tests (tests/drizzle_compat_tests.rs):
- `b31_update_with_qualified_where_column`
- `b31_delete_with_qualified_where_column`
- `b32_timestamp_vs_iso_string_comparison`
- `b32_date_vs_iso_string_comparison`

## [3.14.6] - 2026-04-22

### Fixed — Drizzle login read-by-unique-key (B29, real root cause)

The 3.14.5 fix addressed timestamp formatting (B30) but assumed B29
was a downstream symptom. TimeTracker's retest proved otherwise: even
with timestamps round-tripping cleanly, the canonical Drizzle shape
`select <all cols> from t where t.col = $1` still returned `[]`.

Actual root cause: `execute_plan_with_params` (src/lib.rs:4983), the
plan-executor behind `EmbeddedDatabase::execute_returning` and
`execute_params_returning`, mutated data but never invalidated
`result_cache`. The `Database::query` entry point DOES invalidate on
DML, but the extended-Q handler for `INSERT ... RETURNING` calls
`execute_returning` **directly**, bypassing that invalidation.

Trigger sequence (TimeTracker's login/register flow):

1. User attempts login against an empty `users` table.
   `SELECT ... WHERE "users"."email" = $1` → `[]`. After parameter
   substitution the key is the fully-rendered SQL;
   `result_cache` stores `[]` under it.
2. User registers. `INSERT ... RETURNING ...` via extended-Q lands in
   `execute_plan_with_params`, which inserts the row but does NOT
   clear `result_cache`.
3. User logs in. Same canonical SQL → substitutes to the same key →
   cache hit → stale `[]` is served forever, even though the row now
   exists.

Swapping any trigger (unqualified WHERE, different projection, string
literal instead of `$1`) produces a different substituted SQL and
therefore a different cache key that misses, which is why every
variation returned the row while the canonical shape didn't — and why
the bug looked like a "planner/prepared-statement" issue to the
reporter.

Fix: invalidate `result_cache` at the single choke point in
`execute_plan_with_params` whenever the plan is `Insert` /
`InsertSelect` / `Update` / `Delete` and the execution succeeded.

Regression tests:
- `tests/drizzle_compat_tests.rs::b29_login_probe_then_register_then_login`
  — in-process reproduction of the trigger sequence.
- `tests/drizzle_compat_tests.rs::b29_canonical_drizzle_select_returns_row`
  — pins the canonical substituted shape.
- `tests/drizzle_compat_tests.rs::b29_qualified_predicate_matches_scan_row`
  — shrinks the qualified-predicate invariant.

## [3.14.5] - 2026-04-22

### Fixed — Drizzle login + timestamp reads (B29 / B30)

Both bugs had the same root cause: the direct-encoding path at
`send_data_row_direct` (src/protocol/postgres/handler.rs:952) was
still emitting `Timestamp` values as RFC-3339 with nanosecond
precision (`2026-04-21T20:43:55.674347541+00:00`). v3.14.4 fixed the
fallback `tuple_to_pg_values` path but missed this one. Consequences:

- **B29 Drizzle SELECT shape returns empty.** When Drizzle's
  `postgres-js` integration parsed the malformed timestamp it
  crashed the result binding silently, and Drizzle's type-coerced
  filter comparison (`eq(users.email, v)`) resolved against a
  null-valued row that the app-side filter then rejected as
  non-matching — the symptom being "empty result set". The
  underlying pg query *did* return the row; the client just failed
  to interpret it.
- **B30 timestamp columns parsed as null.** `drizzle-orm/postgres-js`
  registers a custom parser for OID 1114 (`timestamp`) that expects
  PG wire format `YYYY-MM-DD HH:MM:SS.ffffff` (microsecond precision,
  space separator, no zone). Our nanosecond-precision RFC-3339
  output silently produced `null`.

Fix: emit PostgreSQL-standard `YYYY-MM-DD HH:MM:SS.ffffff` on the
direct-encoding path (matching v3.14.4's `tuple_to_pg_values` fix).
Applied to `Timestamp` and `Time` — `Date` was already correct.

### Verified end-to-end with `drizzle-orm/postgres-js`

```js
const users = pgTable('users', {
  id: serial('id').primaryKey(),
  email: text('email').notNull(),
  password: text('password').notNull(),
  createdAt: timestamp('created_at').defaultNow().notNull(),
})

const [u] = await db.insert(users).values({ email, password }).returning()
// { id: 1, email: 'alice@x.com', password: 'pw',
//   createdAt: 2026-04-22T06:05:01.619Z }  ← real Date, not null

const rows = await db.select().from(users).where(eq(users.email, 'alice@x.com'))
// [{ id: 1, email: 'alice@x.com', password: 'pw', createdAt: Date(…) }]
```

## [3.14.4] - 2026-04-21

### Fixed — Drizzle `.insert().returning()` blockers (B27 / B28)

- **B27 `DEFAULT` keyword inside `VALUES` resolves the column's declared
  default.** v3.14.0 (B3) rewrote every `DEFAULT` token to NULL, which
  worked for SERIAL/IDENTITY columns (auto-filled later in storage) but
  broke any column with a real `DEFAULT <expr>` — v3.14.3's NOT NULL
  enforcement then rejected the NULL.  New `LogicalExpr::DefaultValue`
  marker flows from the planner to the INSERT executor; the executor
  treats it as "column omitted", so the B24 default-fill pass runs the
  declared DEFAULT expression.  Drizzle emits `VALUES (default, …,
  default)` on every `.insert()` — every write in TimeTracker hit this.
- **B28 `INSERT … RETURNING *` over the extended query protocol.**
  `handle_execute_extended` used to dispatch non-SELECT writes through
  `database.execute()` which drops the returning tuples. Now detects
  `INSERT/UPDATE/DELETE … RETURNING …`, routes through
  `execute_returning`, and emits the tuples as `DataRow` messages
  (RowDescription was already sent during Describe).  Matches the
  simple-query behaviour.
- **Timestamp wire format** now microsecond-precision with a space
  separator (`YYYY-MM-DD HH:MM:SS.ffffff`) — the PostgreSQL
  on-the-wire format. Previously `rfc3339` nanosecond-precision output
  crashed psycopg's timestamp parser ("timestamp too large (after year
  10K)"). `postgres-js` accepted both but produced slightly different
  `Date` values.

### Added

- `LogicalExpr::DefaultValue` — dedicated marker for the `DEFAULT`
  keyword in INSERT VALUES. Threaded through planner, optimizer,
  type_inference, and the three INSERT executor paths.
- `tests/drizzle_compat_tests.rs` — two B27 regression cases (DEFAULT
  for DEFAULT-expr column, DEFAULT for SERIAL column). B28 is a
  wire-level regression verified via postgres-js end-to-end.

### Verified end-to-end via `postgres-js 3.4.5` + Drizzle's exact INSERT shape

```js
const [user] = await sql`
  INSERT INTO "users" ("id","email","pw","created_at")
  VALUES (default, ${'alice@x.com'}, ${'pw'}, default)
  RETURNING "id","email","pw","created_at"
`
//  { id: 1, email: 'alice@x.com', pw: 'pw',
//    created_at: '2026-04-21T20:49:20.925Z' }
```

## [3.14.3] - 2026-04-21

### Fixed — first-user-registration blockers (B24 / B25 / B26)

- **B24 `DEFAULT <expr>` applied on omitted columns.** Every Drizzle
  table with `created_at TIMESTAMP DEFAULT now() NOT NULL` was
  inserting NULL instead of evaluating `now()`, then either erroring
  on the NOT NULL constraint or (worse) storing NULL silently. New
  helper `apply_defaults_and_check_not_null` parses the stored
  default expression JSON, evaluates it via the shared SQL evaluator,
  and fills in the omitted slot. Only omitted slots get defaults —
  explicit `NULL` bypasses the default and surfaces as a NOT NULL
  violation, matching stock PostgreSQL.
- **B25 `INSERT INTO t DEFAULT VALUES`.** sqlparser leaves
  `insert.source = None` for this syntax; the planner used to error
  with "INSERT statement missing source query". Now maps to an Insert
  with a single empty VALUES row — every schema column goes through
  the default-fill pass.
- **B26 `NOT NULL` enforcement on every INSERT path.** Three INSERT
  paths (fast-path `try_fast_insert`, per-params
  `execute_plan_with_params`, main transactional
  `execute_in_transaction`) all call the new NOT NULL check. Covers
  both omitted columns and explicit `NULL` in user VALUES. Consistent
  with the extended-protocol path.

### Added

- `EmbeddedDatabase::apply_defaults_and_check_not_null` — single
  source of truth for default application + NOT NULL enforcement
  across all three INSERT paths.
- `tests/drizzle_compat_tests.rs` — six B24 / B25 / B26 regression
  cases (DEFAULT with function call, DEFAULT with literal, DEFAULT
  VALUES, explicit NULL rejected, omitted NOT NULL rejected, NOT NULL
  satisfied by default). All 24 compat tests passing; 1730 lib tests
  unchanged.

## [3.14.2] - 2026-04-21

### Fixed — real-driver blockers found during v3.14.1 retest

- **B22 `Flush` (`H` / 0x48) message** is now a first-class
  `FrontendMessage` variant. Every pipelined Postgres driver
  (postgres-js, `pg`, psycopg internally, Npgsql, JDBC) emits
  `Parse → Bind → [Describe →] Execute → Flush` on every query and
  then waits for the server to push the ParseComplete / DataRows /
  CommandComplete before sending `Sync`. Without `Flush`, the driver
  is killed mid-query and the TCP connection goes down.
  The dispatch just flushes the socket buffer — no ReadyForQuery
  (that's `Sync`'s job). Verified end-to-end via `postgres-js 3.4.5`
  over TCP — connect + `SELECT version()` + parameterised
  `pg_catalog.pg_type` lookup + `pg_tables` with `NOT IN` filter all
  complete cleanly.
- **B23 scalar subquery in `UPDATE … SET`** (correlated + uncorrelated).
  `Expr::Subquery` is now a `LogicalExpr::ScalarSubquery` variant and
  the UPDATE executor materialises it per row:
  1. Walk the subquery plan, replace every
     `Column { table: Some(<outer_table>), name }` with the literal
     value from the current outer row.
  2. Execute the (now uncorrelated) plan and take the first column
     of the first row; return `NULL` if zero rows.
  Handles the canonical Drizzle-migration rewrite pattern from
  `docs/compatibility/plpgsql.md`:
  `UPDATE user_profile SET display_name =
   (SELECT email FROM users WHERE users.id = user_profile.user_id);`

### Added

- `tests/drizzle_compat_tests.rs` — three B23 regression cases
  (correlated with outer ref, uncorrelated aggregate, empty
  subquery → NULL). All 18 compat tests passing; 1730 lib tests
  unchanged.

## [3.14.1] - 2026-04-20

### Fixed — TimeTracker retest follow-ups

- **B19 pg_catalog visible on extended query protocol.**
  `PgCatalog::handle_query` now runs from the
  `Parse → Bind → Execute` path as well as the simple-Q path.
  `postgres-js`, `pg`, `psycopg` and every other real driver does its
  connect-time type introspection through the extended protocol;
  without this fix they got a bogus
  `Table 'pg_catalog.pg_type' does not exist` and couldn't connect.
- **B20 catalog queries honor WHERE.** The emulator used to return
  the full table and rely on projection-only filtering. Added a
  small WHERE-clause evaluator that handles `col = 'lit'`, `col = N`,
  `col <> 'lit'`, `col != 'lit'`, `col IN (…)`, `col NOT IN (…)`
  and left-to-right conjunctions. Covers every driver introspection
  query we've seen; complex WHEREs (OR, function calls, subqueries)
  fall through unchanged (keeps all rows).
- **B21 clear error for PL/pgSQL DO bodies.** `DO $$ DECLARE / IF /
  LOOP / FOR / RAISE / := … $$` now returns a targeted error
  identifying the unsupported keyword and pointing at
  `docs/compatibility/plpgsql.md`. Silent no-op would corrupt
  migrations — this version still refuses, but with a clear message
  and migration-rewrite recipes.

### Added

- `docs/compatibility/plpgsql.md` enumerates supported / unsupported
  PL/pgSQL features and gives rewrite recipes (backfill loop →
  `INSERT … SELECT`, conditional index → `CREATE INDEX IF NOT
  EXISTS`, conditional insert → `ON CONFLICT DO NOTHING`).
- `tests/drizzle_compat_tests.rs` notes B19/B20/B21 regression is
  live-verified at the wire level (psql smoke tests) — the core
  `EmbeddedDatabase::query` API doesn't touch the PG wire handler so
  those tests belong on the integration path rather than the unit
  suite.

## [3.14.0] - 2026-04-20

### Fixed — Drizzle / Prisma / TypeORM compatibility (tracks `BUGS_TIMETRACKER_DRIZZLE_COMPAT.md`)

- **B2 `GENERATED ALWAYS AS IDENTITY`**: planner now recognises the
  SQL-standard identity syntax and routes it through the same
  auto-fill path as `SERIAL`.
- **B3 `DEFAULT` keyword in `INSERT ... VALUES`**: sqlparser classifies
  `DEFAULT` as `Expr::Identifier`; the planner now rewrites it to
  `NULL` inside VALUES lists so the existing SERIAL / default-value
  path fires.
- **B4 RETURNING field-count**: fixed a long-standing bug in
  `execute_plan_with_params` where INSERT rows with omitted columns
  produced short tuples, causing the PG wire protocol to emit a
  `DataRow` with a different field count than the `RowDescription`.
  Every `.returning()` call through Drizzle / psycopg is affected.
- **B5 `EXTRACT(EPOCH|YEAR|MONTH|... FROM ...)`**: full coverage in the
  evaluator — Epoch returns Float8 (Unix seconds); calendar fields
  return Int4. `TIMESTAMP '2026-01-01'` and friends now parse (new
  `TypedString` planner arm that lowers to a CAST).
- **B7 `CREATE SEQUENCE`**: DDL is accepted and registers a named
  counter in the new process-scoped sequence store
  (`sql::sequences`). Persistent sequences are a follow-up.
- **B8 `nextval` / `currval` / `setval`**: scalar functions backed by
  the sequence store; always return Int8.
- **B9 `DO $$ … END $$` blocks**: the PG handler unwraps the
  dollar-quoted body and executes plain-SQL statements inside via a
  single `DO` CommandComplete. PL/pgSQL control flow (IF / LOOP /
  RAISE) is NOT interpreted — documented as out of scope.
- **B10 dollar-quoted string literals**: `$$text$$` and `$tag$text$tag$`
  values map to `Value::String` in the planner.
- **B11 multi-statement simple queries**: the `Q` message now accepts
  `;`-separated statements and emits one response per statement with a
  single trailing `ReadyForQuery`, matching PG protocol.
- **B14 identifier case-folding**: new `Planner::normalize_ident` and
  `normalize_object_name` helpers strip surrounding quotes
  (preserving case) and lower-case unquoted identifiers. Applied at
  every DDL and reference call site — `CREATE TABLE Foo` matches
  `SELECT FROM foo` matches `SELECT FROM FOO`, while quoted
  `"Foo"` stays case-sensitive (PG-compliant).
- **B15 `gen_random_uuid()` / `uuid_generate_v4()`**: new scalar
  functions returning `Value::Uuid`.
- **B17 startup banner**: now points to `docs/compatibility/`, the
  FTS doc, and the new `heliosdb_capability_report()` probe so
  drivers / migration tools can discover supported features before
  bisecting failures.

### Added

- **`heliosdb_capability_report()`** scalar function — returns a
  human-readable summary of what this server version supports vs.
  stock Postgres.
- **`src/sql/sequences.rs`** — process-scoped, thread-safe counter
  store shared by `CREATE SEQUENCE` / `nextval` / `currval` /
  `setval`.
- **`tests/drizzle_compat_tests.rs`** — 15 regression cases, one per
  bug in the `BUGS_TIMETRACKER_DRIZZLE_COMPAT.md` report.

### Query-engine changes

- Result cache now skips SQL that contains non-deterministic
  functions (`nextval`, `setval`, `currval`, `gen_random_uuid`,
  `random(`, `now(`, `clock_timestamp`). Previously, a second call
  returned the first result verbatim.

## [3.13.0] - 2026-04-19

### Added — PostgreSQL-compatible full-text search

- **Scalar FTS functions**: `to_tsvector(text)`,
  `to_tsvector(config, text)`, `to_tsquery(text)`,
  `plainto_tsquery(text)`, `phraseto_tsquery(text)`, `ts_rank(doc,
  query)`, `ts_rank_cd(doc, query)` — all implemented in
  `src/sql/evaluator.rs`. Values round-trip as `Value::Json` (array of
  normalised tokens) so they flow through the PostgreSQL wire
  protocol unchanged and render as JSON arrays for introspection.
- **`@@` operator** (`tsvector @@ tsquery → boolean`): new
  `BinaryOperator::TsMatch` in the logical plan, wired in the planner
  from `SqlBinaryOp::AtAt` and evaluated via the shared
  `search::tokenizer` + in-memory match.
- **`TSVECTOR` / `TSQUERY` column types**: accepted in `CREATE TABLE`
  (`src/sql/planner.rs:3044`). Stored as `DataType::Json` internally.
- **`CREATE INDEX ... USING gin | gist (col)`**: accepted as DDL for
  ORM/migration compatibility (`src/sql/executor/ddl.rs:79`). The
  index is currently a no-op — `@@` still walks rows in the evaluator
  — but the syntax round-trips cleanly so Django, SQLAlchemy, and
  hand-written migrations load without errors.
- Backed by `search::Bm25Index` (landed in v3.11.0), which had been
  unreachable from SQL until now.

### Fixed

- **Stale version strings**. `pg_catalog.version()`, the
  `server_version` parameter-status message, and the `SHOW
  server_version` response all now use `env!("CARGO_PKG_VERSION")`
  instead of the hardcoded `3.7.0` / `3.10.0` / `17.0 (HeliosDB-Lite
  2.0)` strings that had drifted across releases.

### Documentation

- New `docs/compatibility/fts.md` — honest scope of our FTS support:
  what works (token match, BM25 rank, JSON-encoded tsvector),
  what doesn't (stemming, phrase queries, `setweight()`, persistent
  GIN index), and the migration hook for when it does.
- `tests/fts_tests.rs` (8 regression cases): tsvector construction,
  `@@` match/miss, rank scoring, `GIN` DDL acceptance, null
  propagation, version-string drift.

### Tracks

- Request from the EasyRAG team (`foor.network/easyrag`) — their
  adapter (`backend/app/services/vectordb/adapters/heliosdb_nano_adapter.py`)
  was client-side reranking with `rank_bm25.BM25Okapi` to work
  around the missing FTS functions. Simplification guide published
  at `easyrag/docs/heliosdb_nano_adapter_simplification.md`.

## [3.12.0] - 2026-04-17

### Fixed

- **`LIMIT $1 OFFSET $2` via psycopg extended query protocol** (root
  cause of SQLAlchemy's `NotImplementedError: _row_as_tuple_getter`).
  The planner's `expr_to_usize` rejected `Expr::Value(Placeholder(_))`,
  which made Parse-time schema derivation fail and caused `Describe` to
  send `NoData` instead of `RowDescription`. Now accepts placeholders
  (the real values are substituted at Execute time before planning).
- **Fallback `RowDescription` for `SELECT`**: if schema derivation
  still fails for an exotic query, we now synthesise a best-effort
  schema from the sqlparser projection list rather than returning
  `NoData` — matching PostgreSQL's behaviour and keeping SQLAlchemy
  row decoders happy.

### Added — Pagination

- **Top-K operator** (`sql::executor::topk::TopKOperator`): streams the
  input through a bounded max-heap of size `k = limit + offset` when
  the plan is `Limit(Sort(…))` or `Limit(Project(Sort(…)))`.
  Complexity drops from O(N log N) to O(N log k) and memory from O(N)
  to O(k). Kicks in automatically whenever the `LIMIT` has a concrete
  bound.
- **Row-constructor comparison** for keyset pagination:
  `WHERE (created_at, id) < ($1, $2) ORDER BY created_at DESC, id DESC LIMIT N`
  is now planned and evaluated lexicographically. New
  `LogicalExpr::Tuple` variant and `evaluate_tuple_compare` in the
  evaluator. Supports `=`, `<>`, `<`, `<=`, `>`, `>=`.
- **Storage-level OFFSET pushdown** (`storage::EmbeddedStorage::scan_table_with_offset_limit`):
  skips `offset` rows at the RocksDB iterator level *without*
  deserialising them (no bincode, no decrypt, no dict/CAS resolve) and
  then fetches `limit` rows fully. Markon's `LIMIT 5 OFFSET 990` on
  1000 rows now returns in ~1 ms (previously required materialising
  all 995+ rows before the `LimitOperator` skipped).
- **Primary-key range scan API**
  (`storage::EmbeddedStorage::scan_table_pk_range`): low-level building
  block for future planner-driven keyset pushdown; currently exposed
  for callers that know the PK range up front.

### Changed

- `LogicalExpr` gains a `Tuple { items }` variant — every consumer
  (`optimizer::rules`, `optimizer::cost`, `sql::type_inference`,
  `sql::evaluator`) handles it.
- Pagination integration test suite (`tests/pagination_tests.rs`, 7
  tests) lands with the feature, covering empty tables, ORDER BY,
  LEFT OUTER JOIN, keyset, row-constructor equality, and large-offset
  correctness.

### Tracks

- `FEATURE_REQUEST_pagination.md` — acceptance criteria 1–5 met;
  cross-engine benchmark (vs Postgres / Oracle / MSSQL) and a website
  marketing page tracked as follow-up (see task #122).

## [3.11.0] - 2026-04-15

### Added (RAG-native integration -- 5 features)

- **Per-request bump arena** (`runtime::RequestArena`, idea 3): wraps
  `bumpalo::Bump` so transient buffers (HNSW candidate lists, scratch
  rows, BM25 term lists) are dropped wholesale when a request finishes
  -- amortising deallocation cost to a single `free`. New crate dep:
  `bumpalo` (with `collections` feature).
- **Native graph adjacency lists** (`graph::*`, idea 1): in-memory
  `GraphStore` backed by `dashmap` with O(1) edge insert / O(1)
  per-node neighbor lookup, plus `traverse` module implementing BFS,
  Dijkstra (non-negative weights), and bidirectional BFS, all gated
  by `TraversalLimits` to bound runaway queries.
- **BM25 + hybrid search + RRF/MMR** (`search::*`, idea 2):
  Unicode-aware tokenizer, in-memory inverted-index BM25 with
  configurable `(k1, b)`, Reciprocal Rank Fusion + Maximal Marginal
  Relevance rerankers, and `hybrid_search` orchestrator that fuses
  BM25 + vector hits via RRF / MMR / weighted-linear. Deterministic
  tie-breaks on doc_id throughout. New crate dep:
  `unicode-segmentation`.
- **Compiled query plans** (`sql::compiled::CompiledPlanCache`, idea
  4): LRU-bounded cache of parser output keyed by plan name.
  `PREPARE COMPILED <name> AS <sql>` + `EXECUTE <name>` surface
  recognised by `parse_prepare_compiled` / `parse_execute` /
  `try_handle_compiled`.
- **MCP idea-5 tools + resources** (`mcp_extensions::*`, idea 5):
  six new tools (`heliosdb_bm25_index`, `heliosdb_hybrid_search`,
  `heliosdb_graph_add_edge`, `heliosdb_graph_traverse`,
  `heliosdb_graph_path`, `heliosdb_embed_and_store`) plus two
  resource resolvers (`heliosdb://schema/{table}`,
  `heliosdb://stats/{table}`). Lives in a standalone module pending
  the legacy `src/mcp/` server's reconciliation with current
  EmbeddedDatabase API -- see `BLOCKER_idea_5.md`.

### Tests

- 43 new integration tests across 9 new test files; 56 new unit tests
  inside the new modules. Existing 1730 lib tests continue to pass.

## [3.10.0] - 2026-04-14

### Added
- Implicit comma-joins: `FROM t1, t2 WHERE t1.id = t2.id` now works
  (treated as CROSS JOIN + WHERE filter). WordPress uses this pattern
  for _update_post_term_count during tag/category operations.

### Fixed
- ALTER TABLE ADD KEY/INDEX with prefix lengths now silently accepted
  (stripped by translator). WordPress dbDelta() schema checks no longer error.

## [3.9.9] - 2026-04-11

### Fixed
- **WHERE ID = '1' returns 0 rows** (root cause of wp_capabilities not written):
  `coerce_pk_value()` handled Int→Int widening but NOT String→Int coercion.
  WordPress `$wpdb->prepare("WHERE ID = %s", 1)` produces `WHERE ID = '1'`.
  The ART index lookup received `String("1")` which didn't match stored
  `Int8(1)`. Added String→Int8/Int4/Int2 parsing in coerce_pk_value().
  This was the last piece: get_userdata(1) now finds the user, wp_insert_user()
  writes capabilities, and the full install chain completes.

## [3.9.8] - 2026-04-10

### Fixed
- MySQL double-quoted string literals: WordPress $wpdb->prepare() can produce
  VALUES with double-quoted strings ("a:1:{s:13:\"admin\";b:1;}"). These were
  passed through as identifier quotes, causing silent data loss. Translator
  now detects double-quoted values in string context (after VALUES(, SET =,
  etc.) and converts them to single-quoted PG string literals with proper
  backslash escape handling. Fixes wp_capabilities not written during install.

## [3.9.7] - 2026-04-10

### Fixed
- ON CONFLICT DO UPDATE now handles UNIQUE key conflicts (not just PK).
  WordPress wp_options has option_id as PK and option_name as UNIQUE.
  The conflict is on option_name but the old code only looked up by PK
  (which was NULL/auto-generated). Now scans UNIQUE columns for the
  conflicting value, falls back to PK lookup.
  Fixes update_option(), transients, rewrite rules, cron.

## [3.9.6] - 2026-04-10

### Fixed
- **CRITICAL REGRESSION**: Semicolons inside single-quoted strings were treated
  as statement terminators, breaking all WordPress serialized PHP data
  ('a:1:{s:13:"administrator";b:1;}'). Rewrote execute_dml SQL splitting to
  use quote-aware parser instead of naive .split(';').
  128 parse errors during install → 0.

## [3.9.5] - 2026-04-10

### Added
- **Native ON CONFLICT DO UPDATE / DO NOTHING** in planner and executor.
  No more handler-level INSERT-catch-UPDATE workaround. Supports both
  PostgreSQL `ON CONFLICT` and MySQL `ON DUPLICATE KEY UPDATE` syntax
  natively through the planner with EXCLUDED.col reference resolution.
- `OnConflictAction` enum in LogicalPlan::Insert (DoNothing, DoUpdate)
- MySQL translator now produces proper `ON CONFLICT DO UPDATE SET col = EXCLUDED.col`
  instead of stripping the clause
- 10 new upsert tests covering DO NOTHING, DO UPDATE, EXCLUDED refs,
  multi-column, partial update, and no-conflict paths

## [3.9.4] - 2026-04-10

### Fixed
- ON DUPLICATE KEY UPDATE: UNIQUE KEY constraints now preserved (converted to
  UNIQUE(col) instead of stripped). UNIQUE flag propagated to column defs.
  Duplicate INSERT now correctly triggers UPDATE fallback.
- SHOW INDEX: returns UNIQUE indexes from table constraints in addition to
  PRIMARY key entries. WordPress dbDelta() can now detect existing indexes.
- Multi-table DELETE: generates two separate DELETE...IN(subquery) statements
  instead of PostgreSQL USING syntax. execute_dml splits semicolons.

## [3.9.3] - 2026-04-10

### Fixed
- **ROOT CAUSE of LAST_INSERT_ID=0 and all WordPress content creation failures:**
  Table-level `PRIMARY KEY (col)` constraint (used by WordPress in all CREATE TABLE)
  was not propagated to the column's `primary_key` flag. Only inline `col INT PRIMARY KEY`
  was handled. The column was stored as a regular nullable BIGINT — no auto-fill,
  no sequence, no insert_id. Fixed by propagating PK from table-level constraints
  to column defs in the planner's create_table_to_plan().

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
