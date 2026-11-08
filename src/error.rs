//! Error types for HeliosDB Lite
//!
//! This module defines all error types returned by HeliosDB Lite operations.
//! All errors implement `std::error::Error` and can be displayed as human-readable
//! messages.
//!
//! # Error Categories
//!
//! - **Storage errors**: RocksDB operations, disk I/O, corruption
//! - **SQL errors**: Parsing, planning, and execution failures
//! - **Transaction errors**: Conflicts, deadlocks, isolation violations
//! - **Protocol errors**: Wire protocol issues, authentication failures
//! - **Configuration errors**: Invalid settings, missing keys
//!
//! # Error Handling Example
//!
//! ```rust,no_run
//! use heliosdb_nano::{EmbeddedDatabase, Error, Result};
//!
//! fn run_query(db: &EmbeddedDatabase, sql: &str) -> Result<()> {
//!     match db.execute(sql) {
//!         Ok(rows) => println!("Affected {} rows", rows),
//!         Err(Error::SqlParse(msg)) => eprintln!("Syntax error: {}", msg),
//!         Err(Error::QueryTimeout(msg)) => eprintln!("Query timed out: {}", msg),
//!         Err(e) => eprintln!("Database error: {}", e),
//!     }
//!     Ok(())
//! }
//! ```

/// Result type for HeliosDB operations
///
/// Alias for `std::result::Result<T, Error>` for convenience.
pub type Result<T> = std::result::Result<T, Error>;

/// HDB-008: the SQLSTATE 25P02 message text, verbatim from PostgreSQL.
///
/// Carried by [`Error::in_failed_transaction`] and recognised by
/// [`Error::is_failed_transaction`]; the PostgreSQL wire maps it back to
/// `25P02` in `sqlstate_for_error`. A shared const rather than a literal so
/// the engine, the wire handlers and the tests cannot drift.
pub const IN_FAILED_TRANSACTION_MESSAGE: &str =
    "current transaction is aborted, commands ignored until end of transaction block";

/// HDB-008: the message a `COMMIT` of an already-aborted transaction returns
/// on the ENGINE API. The transaction has been rolled back; nothing was
/// committed.
///
/// PostgreSQL answers such a COMMIT on the wire with the `ROLLBACK` command
/// tag and no error, and the PostgreSQL handler still does exactly that. An
/// embedded caller has no command tag to inspect, so a silent `Ok` there would
/// be indistinguishable from a real commit — which is the whole of the
/// reported bug.
pub const COMMIT_OF_FAILED_TRANSACTION_MESSAGE: &str =
    "current transaction is aborted, COMMIT rolled it back and nothing was committed; \
     issue the transaction again";

/// The marker carried by the planner's "this statement kind has no plan"
/// refusal (`Planner::statement_to_plan`'s final arm).
///
/// Two things anchor on it, which is why it is a shared const rather than a
/// literal at the raise site:
///
/// * the PostgreSQL wire maps it to `0A000 feature_not_supported` in
///   `sqlstate_for_query_execution_message`, and
/// * the MySQL wire maps it to ER_NOT_SUPPORTED_YET / SQLSTATE `0A000` in
///   `map_error_code`,
///
/// instead of the generic `XX000 internal_error` / `1105 HY000` both used to
/// report. XX000 is what PgBouncer, pgpool and HA proxies read as "this
/// backend is broken", and they may answer it by dropping the backend — for a
/// statement the server merely has not implemented yet.
///
/// The message that carries it names only the statement KIND (and, for the
/// kinds that carry exactly one, the object the user wrote); the sqlparser AST
/// it used to interpolate with `{:?}` stays at DEBUG level.
pub const UNSUPPORTED_STATEMENT_KIND_MARKER: &str = "is not supported by HeliosDB Nano";

/// sprinter f32ba64c00a7: the marker carried by the refusal raised when a
/// `NULL` reaches a PRIMARY KEY column whose DECLARED type the row-id
/// allocator cannot produce — a `TEXT` / `UUID` / `NUMERIC` / … key.
///
/// The auto-fill that serves `SERIAL` / `IDENTITY` used to gate on
/// `col.primary_key` ALONE and then write `Value::Int8(row_id)` whatever the
/// column was declared as, so `CREATE TABLE t (id TEXT, v INT, PRIMARY KEY
/// (id))` + `INSERT INTO t (v) VALUES (1)` stored an INTEGER in a TEXT key.
/// PostgreSQL has no such path: an omitted / NULL primary key with no default
/// and no identity is `23502 not_null_violation`, and a default is always of
/// the column's OWN type.
///
/// A shared const rather than a literal at the raise sites (there are seven —
/// three storage funnels and four executor arms) because two classifiers
/// anchor on it:
///
/// * the PostgreSQL wire maps it to `23502 not_null_violation` in
///   `sqlstate_for_error`, and
/// * the MySQL wire maps it to ER_BAD_NULL_ERROR / SQLSTATE `23000` in
///   `map_error_code`.
///
/// Without those arms the message falls through to `23000`
/// integrity_constraint_violation on the PostgreSQL wire and — because it
/// names a "constraint" — to `1452 ER_NO_REFERENCED_ROW_2` on the MySQL wire,
/// which tells a driver a FOREIGN KEY failed.
///
/// The text is PostgreSQL's own wording, so the whole message reads
/// `null value in column "id" of relation "t" violates not-null constraint`.
pub const NOT_NULL_VIOLATION_MARKER: &str = "violates not-null constraint";

/// Spec P1.4: the prefix of the one error every write entry point of an
/// `EmbeddedDatabase` serving a physical standby returns
/// ([`Error::physical_standby_write_refused`]).
///
/// The refusal is raised by the SQL layer BEFORE the statement or call
/// reaches the storage engine, whose closed `WriteGate` is only the backstop:
/// a refusal the gate had to make is a write the SQL layer let through. A
/// shared const, like [`IN_FAILED_TRANSACTION_MESSAGE`], because the PostgreSQL
/// wire maps it to `25006 read_only_sql_transaction` in `sqlstate_for_error`
/// and [`Error::is_physical_standby_write_refusal`] recognises it.
pub const PHYSICAL_STANDBY_WRITE_REFUSED: &str = "writes are not permitted on a physical standby";

/// Spec P1.3: the prefix of the refusal a physical standby answers while it is
/// not ready (`crate::standby_posture`): `standby is not ready: <state>
/// (cursor X, primary Y)`, raised by `heliosdb_require_ready()` and by the
/// PostgreSQL wire's read guard. One shared const because the wire maps it to
/// `55000 object_not_in_prerequisite_state` (`sqlstate_for_error`), which is
/// what a proxy's health check keys on, and no other message may claim it.
pub const STANDBY_NOT_READY: &str = "standby is not ready";

/// Spec P5.1 §3.2: the prefix of the refusal every write receives on a
/// primary that a promoted standby fenced — from the storage write gate
/// (`WriteGate::fence`) and from the PostgreSQL wire's statement guard, which
/// refuses it before the engine is reached. The full text names when, by
/// which epoch, and the command that rebuilds the node. One shared const
/// because the wire maps it to `25006 read_only_sql_transaction`, like a write
/// on a standby, and [`Error::is_primary_fenced_write_refusal`] recognises it.
pub const PRIMARY_FENCED_WRITE_REFUSED: &str = "write refused: this primary was fenced";

/// Spec P5.1 §3.2: the prefix of `heliosdb_require_ready()`'s answer on a
/// fenced primary. Mapped to `55000 object_not_in_prerequisite_state`, the
/// code a proxy's health check takes a node out of its set on, like a standby
/// that is not ready.
pub const PRIMARY_FENCED_NOT_READY: &str = "primary is fenced";

/// Spec P5.2 §2: the prefix of the refusal `pg_promote` gets anywhere but
/// as the PostgreSQL wire session's own statement (`SELECT pg_promote(…)`
/// alone, literal arguments) — inside another statement, or through the
/// embedded API: the promotion reopens the store the caller is running
/// against, which only the server session can let go of. Mapped to `55000
/// object_not_in_prerequisite_state`.
pub const PG_PROMOTE_SERVER_ONLY: &str = "pg_promote is answered by the server session";

/// Spec P2.1: the prefix of the error a client receives for a commit its
/// standbys did not acknowledge within `[replication] sync_timeout_ms` under
/// `sync_timeout_action = "fail"` (`replication::ha_state::SyncUnconfirmed`).
/// The commit is durable on the primary — like a cancelled synchronous commit
/// in PostgreSQL — so a client that retries it blindly writes it twice. One
/// shared const because the commit path emits it and the docs and tests key on it.
pub const SYNC_REPLICATION_UNCONFIRMED: &str = "synchronous replication not confirmed";

/// sprinter 6bc4a4040eb7 (phase B1): the marker of the refusal a branch
/// BINDING gets when the name does not resolve to an ACTIVE branch — `USE
/// BRANCH x`, `SET heliosdb.branch = 'x'`, the PostgreSQL startup forms, a REST
/// branch route, an MCP call that names a branch. A dropped or merged branch is
/// not active, so it is refused here too ([`Error::branch_not_active`]).
///
/// One shared const because the PostgreSQL wire maps it to `3D000
/// invalid_catalog_name` (`sqlstate_for_query_execution_message`) — the code
/// PostgreSQL gives a connection to a database that does not exist — and the
/// REST layer maps it to 404.
pub const BRANCH_NOT_ACTIVE_MARKER: &str = "is not an active branch";

/// sprinter 6bc4a4040eb7 (phase B1): the marker of the refusal a session gets
/// when the branch it is BOUND to has since been dropped or merged by anyone
/// ([`Error::branch_binding_gone`]). The session fails CLOSED: nothing it runs
/// reaches main or any other branch until it changes its own binding. Mapped
/// to `3D000` like [`BRANCH_NOT_ACTIVE_MARKER`].
pub const BRANCH_BINDING_GONE_MARKER: &str = "USE BRANCH main or another branch";

/// sprinter 6bc4a4040eb7 (phase B1): the marker of the refusal `DROP BRANCH x`
/// gets on the session that is itself bound to `x` ([`Error::branch_in_use`])
/// — PostgreSQL's `cannot drop the currently open database`, SQLSTATE `55006
/// object_in_use`. Another session's binding does not block the drop; that
/// session fails closed instead.
pub const BRANCH_IN_USE_MARKER: &str = "is the active branch of this session";

/// sprinter 6bc4a4040eb7 (B1 fix pass): the marker of a DROP / MERGE BRANCH
/// that waited [`crate::storage::BRANCH_DRAIN_TIMEOUT`] for the statements
/// running on the branch and gave up ([`Error::branch_busy`]). SQLSTATE 55006,
/// like dropping the session's own branch.
pub const BRANCH_BUSY_MARKER: &str = "is in use by running statements";

/// sprinter 6bc4a4040eb7 (B1 second fix pass): the marker of the refusal a
/// statement gets when it starts on a branch a DROP or MERGE is closing right
/// now ([`Error::branch_closing`]). Refused immediately rather than queued
/// behind the close: when the close succeeds the branch is gone anyway.
/// SQLSTATE 55006, like [`BRANCH_BUSY_MARKER`].
pub const BRANCH_CLOSING_MARKER: &str = "is being dropped or merged";

/// sprinter 6bc4a4040eb7 (phase B2): the marker of the refusal a write path
/// that can only write MAIN's keyspace gets when the running statement is on a
/// branch ([`Error::branch_write_unsupported`]) — the lock-free ingestion
/// engine, direct bulk load, the main-only fast DML funnels. Every SQL write
/// path on a branch goes through the branch view; one that cannot is refused
/// BEFORE it writes, never let through to main (fail closed). SQLSTATE 0A000.
///
/// (Phase B1's interim refusal of every data statement inside a transaction on
/// a branch — `BRANCH_TRANSACTION_UNSUPPORTED_MARKER` — is gone: B2 makes
/// transactions on a branch work.)
pub const BRANCH_WRITE_UNSUPPORTED_MARKER: &str = "is not supported on a branch";

/// sprinter 6bc4a4040eb7 (phase B2 fix pass): the marker of the refusal a
/// statement gets that cannot yet be scoped to a branch and would otherwise
/// write main ([`Error::branch_statement_unsupported`]) —
/// `REFRESH MATERIALIZED VIEW` / `CREATE MATERIALIZED VIEW`, which store a
/// view's rows through main's catalog (a materialized view is not
/// branch-scoped until phase B3). Refused before anything is written.
/// SQLSTATE 0A000.
pub const BRANCH_STATEMENT_UNSUPPORTED_MARKER: &str = "on a branch is not supported yet";

/// sprinter 6bc4a4040eb7 (phase B4): the marker of a `DROP BRANCH`, or a `MERGE …
/// WITH (delete_branch_after = true)`, of a branch with a child branch
/// ([`Error::branch_has_child`]). SQLSTATE 2BP01.
pub const BRANCH_HAS_CHILD_MARKER: &str = "it has child branches";

/// sprinter 6bc4a4040eb7 (phase B4): the marker of a `CREATE TABLE`, `RENAME TO` or
/// `CREATE SCHEMA` whose name holds a `:` ([`Error::table_name_holds_colon`],
/// [`Error::schema_name_holds_colon`]). SQLSTATE 42602.
pub const TABLE_NAME_HOLDS_COLON_MARKER: &str = "name cannot hold ':'";

/// sprinter 6bc4a4040eb7 (phase B4): the marker of a `CREATE BRANCH … AS OF` a time
/// whose rows cannot be read under the table's definition now
/// ([`Error::branch_anchor_before_shape_change`]). SQLSTATE 0A000.
pub const BRANCH_ANCHOR_UNREADABLE_MARKER: &str = "its rows as they were then cannot be read";

/// sprinter 6bc4a4040eb7 (B1 fix pass): the marker of `USE BRANCH` /
/// `SET heliosdb.branch` / `RESET heliosdb.branch` inside an open transaction
/// ([`Error::branch_switch_in_transaction`]) — PostgreSQL cannot change the
/// database of a running transaction either. SQLSTATE 25001
/// active_sql_transaction.
pub const BRANCH_SWITCH_IN_TRANSACTION_MARKER: &str = "cannot change the branch inside a transaction block";

/// sprinter 6bc4a4040eb7 (phase B3): the marker of a SCHEMA statement refused
/// on a branch ([`Error::branch_schema_unsupported`]). A branch scopes `CREATE
/// TABLE`, `DROP TABLE` and `ALTER TABLE … ADD COLUMN` to itself (its catalog
/// overlay, `storage::branch_catalog`); every other schema statement would
/// change main's catalog and is refused before it changes anything. The
/// message also carries [`BRANCH_WRITE_UNSUPPORTED_MARKER`], so every wire
/// maps it to SQLSTATE 0A000 through the arm that already exists.
pub const BRANCH_SCHEMA_UNSUPPORTED_MARKER: &str = "rehearse it in a copy of the database";

/// sprinter 6bc4a4040eb7 (phase B3): the marker of a `MERGE BRANCH` refused
/// because a table whose definition the branch changed was ALSO changed on
/// the merge target after the branch changed it (or the change belongs to an
/// unmerged ancestor branch) — [`Error::branch_merge_schema_refused`].
/// Refused before anything is merged. SQLSTATE 55000
/// object_not_in_prerequisite_state.
pub const BRANCH_MERGE_SCHEMA_REFUSED_MARKER: &str = "refused before anything was merged";

/// sprinter 6bc4a4040eb7 (B3 fix pass): the start of the message of a `MERGE
/// BRANCH` refused because a row it would put on its target holds a PRIMARY KEY
/// / UNIQUE key another target row holds ([`Error::branch_merge_duplicate_key`]).
/// Refused before anything is merged. SQLSTATE 23505. Anchored at the START of
/// the message: the message names the key's values, which are free text.
pub const BRANCH_MERGE_DUPLICATE_KEY_MARKER: &str = "Duplicate key on MERGE BRANCH";

/// sprinter 6bc4a4040eb7 (B3 fix pass): the marker of everything an UNFINISHED
/// merge refuses, and of the merge that stopped ([`Error::branch_merge_pending`],
/// [`Error::branch_merge_interrupted`]): a `MERGE BRANCH` whose durable intent
/// exists is finished by running it again (or by the next open). SQLSTATE 55006.
pub const BRANCH_MERGE_PENDING_MARKER: &str = "run it again to finish it";

/// sprinter 6bc4a4040eb7 (B3 second fix pass): the marker of a merge refused
/// because something else has not finished yet: another merge holds one of its
/// tables, or a schema statement on main is still running
/// ([`Error::branch_merge_table_busy`], [`Error::branch_merge_ddl_busy`]). The
/// merge changed nothing. SQLSTATE 55006.
pub const BRANCH_MERGE_BUSY_MARKER: &str = "has not finished: retry";

/// sprinter 6bc4a4040eb7 (B3 second fix pass): the marker of a merge refused
/// because an open transaction on the target has claimed a key a merged row
/// holds ([`Error::branch_merge_key_claimed`]). Retryable. SQLSTATE 55006.
pub const BRANCH_MERGE_KEY_CLAIMED_MARKER: &str = "another transaction holds the key";

/// sprinter 6bc4a4040eb7 (B3 third fix pass): the marker of a branch `ADD COLUMN`
/// refused because the rows the level itself changed would not fit in the one
/// atomic batch that writes them with the definition
/// ([`Error::branch_materialize_too_large`]). SQLSTATE 54000.
pub const BRANCH_MATERIALIZE_TOO_LARGE_MARKER: &str = "too large for one atomic write";

/// An owning row group succeeded, but its derived vector publication failed.
/// This is a system/storage error, never a pre-write refusal or retryable
/// serialization error, even when its original derived cause carries one.
pub const APPLIED_PUBLICATION_FAILURE_MARKER: &str = "row changes were already applied";

/// Spec R5.2, decision 2(h): the marker of a prepared statement whose result
/// shape moved after it was Described, refused at Execute with nothing run
/// ([`Error::cached_plan_result_changed`]), and of a result row whose field count
/// differs from its row description, the last-resort check in every wire writer
/// (`storage::row_shape::refuse_mismatched_row`). PostgreSQL's words. SQLSTATE
/// 0A000.
pub const ROW_DESCRIPTION_MISMATCH_MARKER: &str = "cached plan must not change result type";

/// Decision 2(h): what says a [`ROW_DESCRIPTION_MISMATCH_MARKER`] refusal came before
/// the statement ran, so a client may prepare it again and retry it: the PG wire
/// then names PostgreSQL's routine (`R = RevalidateCachedQuery`).
pub const RESULT_TYPE_REFUSED_BEFORE_RUN_MARKER: &str = "the statement did not run";

/// Spec R5.2: the marker of a schema statement refused because the transaction
/// it runs in has staged rows of the table it would change
/// ([`Error::ddl_over_staged_rows`]). SQLSTATE 0A000.
pub const DDL_OVER_STAGED_ROWS_MARKER: &str = "after writing it in the same transaction is not supported";

/// Spec R5.3 (review F17): the marker of a `CREATE` / `ALTER SEQUENCE` parameter out
/// of range ([`Error::sequence_parameter_invalid`]). SQLSTATE 22023, PostgreSQL's
/// `invalid_parameter_value`.
pub const SEQUENCE_PARAMETER_INVALID_MARKER: &str = "invalid sequence parameter";

/// Spec R5.3: the marker of `EXPLAIN ANALYZE` of a statement it cannot run as
/// part of the statement's transaction ([`Error::explain_analyze_unsupported`]).
/// SQLSTATE 0A000.
pub const EXPLAIN_ANALYZE_UNSUPPORTED_MARKER: &str = "is not a statement EXPLAIN ANALYZE can run";

/// Spec R5.2: the marker of a volatile `ADD COLUMN` whose rewrite passes
/// `[storage] max_atomic_rewrite_bytes` ([`Error::atomic_rewrite_too_large`]).
/// SQLSTATE 54000.
pub const ATOMIC_REWRITE_TOO_LARGE_MARKER: &str = "exceeds [storage] max_atomic_rewrite_bytes";

/// sprinter 6bc4a4040eb7 (B3 second fix pass): the marker of a statement that
/// waited too long for a schema statement on its branch to finish
/// ([`Error::branch_statement_waited`]). SQLSTATE 55006.
pub const BRANCH_SCHEMA_RUNNING_MARKER: &str = "is running a schema statement";

/// sprinter 6bc4a4040eb7 (B3 fix pass): the marker of a schema statement on a
/// branch refused because another open transaction has staged writes of the
/// table there ([`Error::branch_table_busy`]). SQLSTATE 55006.
pub const BRANCH_TABLE_BUSY_MARKER: &str = "has uncommitted writes in another transaction";

/// Spec R5.1: the marker of a statement that could not get a table's writer lock
/// within `[session] lock_timeout_ms` ([`Error::table_lock_timeout`]), or that could
/// not upgrade to it ([`Error::table_lock_upgrade_refused`]). Nothing it would have
/// changed in the table has changed; retry. SQLSTATE 55P03.
pub const TABLE_LOCK_TIMEOUT_MARKER: &str = "could not obtain lock on table";

/// Spec R5.4 (P12): the marker of a statement that could not get a row's write lock
/// within `[session] lock_timeout_ms` ([`Error::row_lock_timeout`]): another
/// transaction is changing the row. PostgreSQL's words. SQLSTATE 55P03.
pub const ROW_LOCK_TIMEOUT_MARKER: &str = "could not obtain lock on row in relation";

/// Database error type
///
/// All errors from HeliosDB Lite operations are represented by this enum.
/// Each variant includes a human-readable message describing the error.
///
/// # Creating Errors
///
/// Use the constructor methods for creating errors:
///
/// ```rust
/// use heliosdb_nano::Error;
///
/// let err = Error::storage("Table not found: users");
/// let err = Error::transaction("Deadlock detected");
/// let err = Error::query_timeout("Query exceeded 30s limit");
/// ```
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// I/O error
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Storage error
    #[error("Storage error: {0}")]
    Storage(String),

    /// SQL parsing error
    #[error("SQL parse error: {0}")]
    SqlParse(String),

    /// Query execution error
    #[error("Query execution error: {0}")]
    QueryExecution(String),

    /// Query timeout error
    #[error("Query timeout: {0}")]
    QueryTimeout(String),

    /// Query cancelled error
    #[error("Query cancelled: {0}")]
    QueryCancelled(String),

    /// Transaction error
    #[error("Transaction error: {0}")]
    Transaction(String),

    /// Write-write conflict: a pessimistic row-lock wait timed out because
    /// another transaction holds the write lock on the same row. A retriable
    /// serialization failure — maps to PostgreSQL SQLSTATE 40001 on the wire.
    /// The leading `serialization failure` text also drives the MySQL
    /// error-code sniffing (`protocol/mysql/handler.rs`).
    #[error("serialization failure: write-write conflict on {table} row {row} (held by transaction {holder_txn}, waiter {waiter_txn} timed out after {waited_ms}ms); retry the transaction")]
    WriteConflict {
        /// Table owning the contended row.
        table: String,
        /// Row identity within the table (primary-key / row-id text).
        row: String,
        /// Transaction currently holding the write lock (0 if released between
        /// the timeout and the report — a benign race).
        holder_txn: u64,
        /// The waiting transaction that gave up.
        waiter_txn: u64,
        /// Milliseconds the waiter spun before giving up (the lock-acquire timeout).
        waited_ms: u64,
    },

    /// Type conversion error
    #[error("Type conversion error: {0}")]
    TypeConversion(String),

    /// Invalid configuration
    #[error("Invalid configuration: {0}")]
    Config(String),

    /// Encryption error
    #[error("Encryption error: {0}")]
    Encryption(String),

    /// Protocol error
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// Vector index error
    #[error("Vector index error: {0}")]
    VectorIndex(String),

    /// Multi-tenancy error
    #[error("Multi-tenancy error: {0}")]
    MultiTenant(String),

    /// Audit error
    #[error("Audit error: {0}")]
    Audit(String),

    /// Compression error
    #[error("Compression error: {0}")]
    Compression(String),

    /// Branch merge error
    #[error("Branch merge error: {0}")]
    BranchMerge(String),

    /// Merge conflict error
    #[error("Merge conflict: {0}")]
    MergeConflict(String),

    /// Constraint violation error (FK, CHECK, UNIQUE)
    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),

    /// Lock poisoning error (mutex/rwlock poisoned)
    #[error("Lock poisoning error: {0}")]
    LockPoisoned(String),

    /// Generic error
    #[error("{0}")]
    Generic(String),
}

impl Error {
    /// Create a storage error
    pub fn storage(msg: impl Into<String>) -> Self {
        Error::Storage(msg.into())
    }

    /// Create a SQL parse error
    pub fn sql_parse(msg: impl Into<String>) -> Self {
        Error::SqlParse(msg.into())
    }

    /// Create a query execution error
    pub fn query_execution(msg: impl Into<String>) -> Self {
        Error::QueryExecution(msg.into())
    }

    /// Create a query timeout error
    pub fn query_timeout(msg: impl Into<String>) -> Self {
        Error::QueryTimeout(msg.into())
    }

    /// Create a query cancelled error
    pub fn query_cancelled(msg: impl Into<String>) -> Self {
        Error::QueryCancelled(msg.into())
    }

    /// Create a transaction error
    pub fn transaction(msg: impl Into<String>) -> Self {
        Error::Transaction(msg.into())
    }

    /// HDB-008: a statement was issued inside a transaction that an earlier
    /// statement had already aborted. PostgreSQL SQLSTATE 25P02.
    ///
    /// Deliberately a `Transaction` variant rather than a new enum arm: a new
    /// variant would ripple through every error mapper in the tree (REST, MCP,
    /// the Python binding, both wire protocols) and those mappers already do
    /// the right thing for `Transaction`.
    pub fn in_failed_transaction() -> Self {
        Error::Transaction(IN_FAILED_TRANSACTION_MESSAGE.to_string())
    }

    /// HDB-008: `COMMIT` of an aborted transaction — the engine rolled it back
    /// and committed nothing. See [`COMMIT_OF_FAILED_TRANSACTION_MESSAGE`].
    pub fn commit_of_failed_transaction() -> Self {
        Error::Transaction(COMMIT_OF_FAILED_TRANSACTION_MESSAGE.to_string())
    }

    /// HDB-008: true for both aborted-transaction errors above — the refusal
    /// of a statement inside a failed block AND the refusal of its `COMMIT`.
    ///
    /// Callers that want to distinguish them compare the message against the
    /// two consts; this is the "did my transaction die?" question, which is
    /// the one an application actually branches on.
    pub fn is_failed_transaction(&self) -> bool {
        match self {
            Error::Transaction(message) => message.starts_with("current transaction is aborted"),
            _ => false,
        }
    }

    /// Spec P1.4: `operation` (a statement's leading keyword, or the name of
    /// the API called) was refused on a physical standby before it reached the
    /// storage engine. PostgreSQL SQLSTATE 25006. A `Transaction` variant for
    /// the reason [`Error::in_failed_transaction`] gives.
    pub fn physical_standby_write_refused(operation: &str) -> Self {
        Error::Transaction(format!(
            "{PHYSICAL_STANDBY_WRITE_REFUSED}: {operation} was refused before reaching the storage engine; \
             run it on the primary"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B1): `name` does not resolve to an ACTIVE
    /// branch (it never existed, or it was dropped or merged), so it cannot be
    /// bound. SQLSTATE 3D000 on the PostgreSQL wire. A `QueryExecution`
    /// variant so every existing mapper (REST, MCP, both wires) keeps working.
    pub fn branch_not_active(name: &str) -> Self {
        Error::QueryExecution(format!(
            "branch \"{name}\" {BRANCH_NOT_ACTIVE_MARKER}; SHOW BRANCHES lists the branches you can use"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B1): the branch this session is bound to
    /// was dropped (`merged == false`) or merged into another branch since the
    /// binding was made. SQLSTATE 3D000.
    pub fn branch_binding_gone(name: &str, merged: bool) -> Self {
        let how = if merged { "merged" } else { "dropped" };
        Error::QueryExecution(format!("branch \"{name}\" was {how}; {BRANCH_BINDING_GONE_MARKER}"))
    }

    /// sprinter 6bc4a4040eb7 (phase B1): `DROP BRANCH name` on the session
    /// bound to `name`. SQLSTATE 55006.
    pub fn branch_in_use(name: &str) -> Self {
        Error::QueryExecution(format!(
            "cannot drop branch \"{name}\": it {BRANCH_IN_USE_MARKER}; USE BRANCH main first"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): a schema statement on branch `name`
    /// could not get the branch to itself in time, or another one holds it.
    /// SQLSTATE 55006.
    pub fn branch_schema_busy(name: &str) -> Self {
        Error::QueryExecution(format!(
            "a schema statement cannot run alone on branch \"{name}\": it {BRANCH_BUSY_MARKER}; retry when they finish"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): a statement on branch `name`
    /// gave up waiting for a schema statement running on it. SQLSTATE 55006.
    pub fn branch_statement_waited(name: &str) -> Self {
        Error::QueryExecution(format!(
            "branch \"{name}\" {BRANCH_SCHEMA_RUNNING_MARKER} and this statement gave up waiting for it: retry"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B1 fix pass): DROP / MERGE BRANCH `name` timed
    /// out waiting for the statements running on it. SQLSTATE 55006.
    pub fn branch_busy(name: &str) -> Self {
        Error::QueryExecution(format!(
            "cannot drop or merge branch \"{name}\": it {BRANCH_BUSY_MARKER}; retry when they finish"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B1 second fix pass): a statement started on a
    /// branch that a DROP or MERGE is closing. SQLSTATE 55006.
    pub fn branch_closing(name: &str) -> Self {
        Error::QueryExecution(format!("branch \"{name}\" {BRANCH_CLOSING_MARKER}; retry"))
    }

    /// sprinter 6bc4a4040eb7 (phase B2): `path` can only write main's
    /// keyspace and the statement runs on branch `name`. SQLSTATE 0A000.
    pub fn branch_write_unsupported(path: &str, name: &str) -> Self {
        Error::QueryExecution(format!(
            "{path} {BRANCH_WRITE_UNSUPPORTED_MARKER} (branch \"{name}\"): use INSERT / UPDATE / DELETE / \
             COPY on the branch, or run it on main"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B2 fix pass): `statement` cannot be scoped
    /// to branch `name` yet. SQLSTATE 0A000.
    pub fn branch_statement_unsupported(statement: &str, name: &str) -> Self {
        Error::QueryExecution(format!(
            "{statement} {BRANCH_STATEMENT_UNSUPPORTED_MARKER} (branch \"{name}\"): run it on main"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B3): the schema statement `statement` is
    /// not scoped to branches, and the running statement is on branch `name`.
    /// SQLSTATE 0A000 (the message carries [`BRANCH_WRITE_UNSUPPORTED_MARKER`]).
    pub fn branch_schema_unsupported(statement: &str, name: &str) -> Self {
        Error::QueryExecution(format!(
            "{statement} {BRANCH_WRITE_UNSUPPORTED_MARKER} (on branch \"{name}\"): run it on the parent, or \
             {BRANCH_SCHEMA_UNSUPPORTED_MARKER} (`heliosdb-nano dump` / `restore`)"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B3): `MERGE BRANCH source INTO target`
    /// refused, `why` naming the tables. SQLSTATE 55000.
    pub fn branch_merge_schema_refused(source: &str, target: &str, why: &str) -> Self {
        Error::QueryExecution(format!(
            "MERGE BRANCH {source} INTO {target}: {why} — {BRANCH_MERGE_SCHEMA_REFUSED_MARKER}"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): `MERGE BRANCH source INTO target`
    /// would put a row holding `key` of `constraint` (`kind` is `PRIMARY KEY` or
    /// `UNIQUE`) on `table` of the target, where another row holds that key.
    /// Nothing was merged. SQLSTATE 23505.
    pub fn branch_merge_duplicate_key(
        source: &str,
        target: &str,
        table: &str,
        kind: &str,
        constraint: &str,
        key: &str,
    ) -> Self {
        Error::ConstraintViolation(format!(
            "{BRANCH_MERGE_DUPLICATE_KEY_MARKER} {source} INTO {target}: table {table}, {kind} constraint \
             \"{constraint}\", key {key} would be held twice — {BRANCH_MERGE_SCHEMA_REFUSED_MARKER}"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): `what` (a table's DDL, or a statement
    /// on the merge's source) is refused while `MERGE BRANCH source INTO target`
    /// is unfinished. SQLSTATE 55006.
    pub fn branch_merge_pending(what: &str, source: &str, target: &str) -> Self {
        Error::QueryExecution(format!(
            "{what} is part of an unfinished MERGE BRANCH {source} INTO {target}: {BRANCH_MERGE_PENDING_MARKER}, or \
             MERGE BRANCH {source} ABORT to abandon it"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): `what` (a whole-database
    /// operation) is refused while `MERGE BRANCH source INTO target` is
    /// unfinished. SQLSTATE 55006.
    pub fn branch_merge_pending_refuses(what: &str, source: &str, target: &str) -> Self {
        Error::QueryExecution(format!(
            "{what} is refused while MERGE BRANCH {source} INTO {target} is unfinished: \
             {BRANCH_MERGE_PENDING_MARKER}, or MERGE BRANCH {source} ABORT to abandon it"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): this merge needs `table`, and
    /// `MERGE BRANCH source INTO target` is holding it. This merge changed
    /// nothing. SQLSTATE 55006.
    pub fn branch_merge_table_busy(table: &str, source: &str, target: &str) -> Self {
        Error::QueryExecution(format!(
            "table {table} is held by MERGE BRANCH {source} INTO {target}, which {BRANCH_MERGE_BUSY_MARKER} this \
             merge when it has (run that one again to finish it, or MERGE BRANCH {source} ABORT to abandon it)"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): `MERGE BRANCH source INTO
    /// target` waited for schema statements already running on main, and gave
    /// up. It changed nothing. SQLSTATE 55006.
    pub fn branch_merge_ddl_busy(source: &str, target: &str) -> Self {
        Error::QueryExecution(format!(
            "a schema statement running on main {BRANCH_MERGE_BUSY_MARKER} MERGE BRANCH {source} INTO {target} when \
             it has"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 second fix pass): an open transaction on the
    /// target has claimed a key a merged row holds: letting the merge through
    /// would duplicate the key when that transaction commits. Retryable.
    /// SQLSTATE 55006.
    pub fn branch_merge_key_claimed(source: &str, target: &str, table: &str, constraint: &str, key: &str) -> Self {
        Error::QueryExecution(format!(
            "MERGE BRANCH {source} INTO {target}: table {table}, constraint \"{constraint}\", key {key}: \
             {BRANCH_MERGE_KEY_CLAIMED_MARKER} on {target}; retry after it commits or rolls back"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 third fix pass): `table` is held by a merge that
    /// has not written its intent yet, so there is nothing to run again and
    /// nothing to abort. SQLSTATE 55006.
    pub fn branch_merge_in_progress(table: &str, source: &str, target: &str) -> Self {
        Error::QueryExecution(format!(
            "table {table} is being merged by MERGE BRANCH {source} INTO {target}, which {BRANCH_MERGE_BUSY_MARKER} \
             when it has"
        ))
    }

    /// Spec R5.2: `ADD COLUMN` with no usable value for a NOT NULL column, on a
    /// table with rows. Nothing was written. SQLSTATE 23502 (the message carries
    /// [`NOT_NULL_VIOLATION_MARKER`]).
    pub fn add_column_null_on_existing_rows(table: &str, column: &str) -> Self {
        Error::ConstraintViolation(format!(
            "column \"{column}\" of relation \"{table}\" contains null values: adding it with no usable default \
             {NOT_NULL_VIOLATION_MARKER} on the existing rows"
        ))
    }

    /// Spec R5.2: a volatile `ADD COLUMN`, which gives every row its own value and
    /// so rewrites every row in one write group, measured above the configured
    /// cap. Nothing was written. SQLSTATE 54000.
    pub fn atomic_rewrite_too_large(table: &str, bytes: u64, limit: u64) -> Self {
        Error::QueryExecution(format!(
            "ALTER TABLE {table} ADD COLUMN with a volatile default rewrites every row in one atomic write, and \
             at {bytes} bytes it {ATOMIC_REWRITE_TOO_LARGE_MARKER} ({limit}): nothing was written. Add the column \
             without a default, then UPDATE it in batches, or raise the setting"
        ))
    }

    /// Decision 2(h): a statement Described as `described` whose plan now returns
    /// `now`. Nothing has run: a client that prepares it again and executes it
    /// applies it once. SQLSTATE 0A000.
    pub fn cached_plan_result_changed(described: &str, now: &str) -> Self {
        Error::QueryExecution(format!(
            "{ROW_DESCRIPTION_MISMATCH_MARKER}: it was described as ({described}) and would now return ({now}); \
             {RESULT_TYPE_REFUSED_BEFORE_RUN_MARKER}. Prepare it again (or Describe it again) to get the new result type"
        ))
    }

    /// Spec R5.2: `statement` on `table`, with rows of it staged by the
    /// transaction it runs in. Those rows would commit under the old layout, or
    /// go unseen by what the statement builds. Nothing was written. SQLSTATE
    /// 0A000. On the PostgreSQL wire and the embedded API the error aborts the
    /// transaction, so a `COMMIT` after it discards those rows: the message says
    /// so, and what to do instead.
    pub fn ddl_over_staged_rows(statement: &str, table: &str) -> Self {
        Error::QueryExecution(format!(
            "{statement} of table \"{table}\" {DDL_OVER_STAGED_ROWS_MARKER}. Where this error aborts the \
             transaction (the PostgreSQL wire, the embedded API), its writes are discarded, a COMMIT included: \
             commit the rows in a transaction of their own, then run the statement. A restore in one \
             transaction (psql -1) has to be split there"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 third fix pass): a branch `ADD COLUMN`, or a
    /// merge that carries one, whose level changed more of `table` than one
    /// atomic batch can hold. Nothing was written. SQLSTATE 54000.
    pub fn branch_materialize_too_large(table: &str, branch: &str, bytes: usize, limit: usize) -> Self {
        Error::QueryExecution(format!(
            "the rows branch \"{branch}\" changed in table {table} need {bytes} bytes, \
             {BRANCH_MATERIALIZE_TOO_LARGE_MARKER} (the limit is {limit}): nothing was written; merge those rows \
             first, or add the column on the parent and recreate the branch"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 fourth fix pass): a table's merged rows land in one
    /// write group, and these would not fit. Nothing was changed. SQLSTATE 54000.
    pub fn branch_merge_table_too_large(source: &str, target: &str, table: &str, bytes: usize, limit: usize) -> Self {
        Error::QueryExecution(format!(
            "MERGE BRANCH {source} INTO {target}: the rows it changed in table {table} need {bytes} bytes, \
             {BRANCH_MATERIALIZE_TOO_LARGE_MARKER} (the limit is {limit}): nothing was changed; copy those rows to \
             {target} in smaller transactions, or change fewer of them per branch"
        ))
    }

    /// Spec R5.3: `EXPLAIN ANALYZE` of `statement`, which runs outside the
    /// statement's transaction (TRUNCATE deletes at once: inside `BEGIN` a
    /// `ROLLBACK` restored nothing). PostgreSQL has no `EXPLAIN` of these either.
    /// Nothing ran. SQLSTATE 0A000.
    pub fn explain_analyze_unsupported(statement: &str) -> Self {
        Error::QueryExecution(format!(
            "{statement} {EXPLAIN_ANALYZE_UNSUPPORTED_MARKER}: it would run apart from the statement's transaction, \
             which a ROLLBACK could not undo; nothing ran. Run {statement} itself"
        ))
    }

    /// Spec R5 decision 6: a write group of `bytes` above the ceiling `limit`
    /// (`storage::write_gate::MAX_WRITE_GROUP_BYTES`), which no standby could ever
    /// receive and no archive hold. Refused before RocksDB saw it: nothing was
    /// written. SQLSTATE 54000 (the message carries
    /// [`BRANCH_MATERIALIZE_TOO_LARGE_MARKER`]).
    pub fn write_group_too_large(bytes: usize, limit: usize) -> Self {
        Error::QueryExecution(format!(
            "this statement's changes need {bytes} bytes in one write group, {BRANCH_MATERIALIZE_TOO_LARGE_MARKER} \
             (the limit is {limit}, what a standby can receive): nothing was written; split it into smaller \
             statements or transactions"
        ))
    }

    /// Scope the original publication cause to the derived group. In particular,
    /// its cap refusal's "nothing was written" never describes the owning rows.
    pub(crate) fn applied_publication_failure(cause: Self) -> Self {
        if cause.is_applied_publication_failure() {
            return cause;
        }
        Error::Storage(format!(
            "{APPLIED_PUBLICATION_FAILURE_MARKER}; derived vector publication failed. Reopen the database before \
             continuing and do not retry these row changes. Cause for the derived vector group only (its refusal \
             does not undo the row group): {cause}"
        ))
    }

    /// Whether this error reports a derived publication failure after the row
    /// changes applied. Retrying those row changes can apply them a second time.
    pub fn is_applied_publication_failure(&self) -> bool {
        match self {
            Self::Storage(message)
            | Self::Transaction(message)
            | Self::QueryExecution(message)
            | Self::Generic(message) => message.contains(APPLIED_PUBLICATION_FAILURE_MARKER),
            _ => false,
        }
    }

    /// Spec R5.3 (review F17): a `CREATE` / `ALTER SEQUENCE` parameter out of range
    /// (MINVALUE above MAXVALUE, START or RESTART outside them, INCREMENT 0), refused
    /// before anything is written. SQLSTATE 22023; it used to reach a client as XX000.
    pub fn sequence_parameter_invalid(detail: impl std::fmt::Display) -> Self {
        Error::QueryExecution(format!("{detail} ({SEQUENCE_PARAMETER_INVALID_MARKER})"))
    }

    /// Spec R5.1: this statement waited `waited` for `table`'s writer lock and gave
    /// up. `holder` says what it is behind ("an open transaction holds it", "ALTER
    /// TABLE … ADD COLUMN holds it"). SQLSTATE 55P03.
    pub fn table_lock_timeout(table: &str, holder: &str, waited: std::time::Duration) -> Self {
        Error::QueryExecution(format!(
            "{TABLE_LOCK_TIMEOUT_MARKER} {table}: {holder} (waited {} ms; SET lock_timeout or [session] \
             lock_timeout_ms bounds this): retry when it has finished",
            waited.as_millis()
        ))
    }

    /// Spec R5.4 (P12): this statement waited `waited` for the write lock of row
    /// `row_id` of `table`, held by another transaction that is changing it, and gave
    /// up. Nothing it would have changed has changed. SQLSTATE 55P03.
    pub fn row_lock_timeout(table: &str, row_id: u64, waited: std::time::Duration) -> Self {
        Error::QueryExecution(format!(
            "{ROW_LOCK_TIMEOUT_MARKER} \"{table}\": another transaction is changing row {row_id} (waited {} ms; \
             SET lock_timeout or [session] lock_timeout_ms bounds this): retry when it has finished",
            waited.as_millis()
        ))
    }

    /// Spec R5.4 (P12): a REPEATABLE READ or SERIALIZABLE transaction found that the
    /// row it is about to change was changed by a transaction that committed after
    /// its snapshot. PostgreSQL's words. SQLSTATE 40001.
    pub fn concurrent_update(table: &str) -> Self {
        Error::Transaction(format!(
            "could not serialize access due to concurrent update of a row of table {table} (serialization \
             failure); retry the transaction"
        ))
    }

    /// Spec R5.4 (P12): row `row_id` of `table` is held by the embedded handle's own
    /// transaction (`BEGIN` on the handle), which only the thread that opened it can
    /// end, and this statement runs on that thread: a wait could never end. Refused at
    /// once. SQLSTATE 40001.
    pub fn row_held_by_this_threads_transaction(table: &str, row_id: u64) -> Self {
        Error::Transaction(format!(
            "could not serialize access to row {row_id} of table {table}: the transaction this thread opened on \
             the embedded handle holds it, and only this thread can end it (serialization failure); COMMIT or \
             ROLLBACK that transaction first"
        ))
    }

    /// Spec R5.1: this owner holds `table` shared and wants it exclusively, in a case
    /// where waiting is certain to deadlock, and `holder` says why: another owner is
    /// already waiting to upgrade it, or the merge landing beside this transaction would
    /// be waiting for the transaction it runs in. Refused at once. SQLSTATE 55P03.
    pub fn table_lock_upgrade_refused(table: &str, holder: &str) -> Self {
        Error::QueryExecution(format!(
            "{TABLE_LOCK_TIMEOUT_MARKER} {table}: {holder}, and this transaction already holds the table shared: \
             COMMIT or ROLLBACK, then retry"
        ))
    }

    /// Spec R5.1: this statement would have to wait for `table`, and `waiters` are
    /// waiting already, each pinning a thread the server needs for the statements that
    /// would free them. Refused at once. SQLSTATE 55P03.
    pub fn table_lock_too_many_waiters(table: &str, waiters: usize) -> Self {
        Error::QueryExecution(format!(
            "{TABLE_LOCK_TIMEOUT_MARKER} {table}: {waiters} statements are already waiting for table locks, the most \
             this server lets wait: retry when they have finished"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): `MERGE BRANCH source INTO target`
    /// stopped after its intent was durable. It is resumable, never undone: the
    /// source stays merged, and running the statement again finishes it (so does
    /// the next open). SQLSTATE 55006.
    pub fn branch_merge_interrupted(source: &str, target: &str, cause: &Error) -> Self {
        Error::QueryExecution(format!(
            "MERGE BRANCH {source} INTO {target} stopped after it began changing {target} ({cause}): \
             {BRANCH_MERGE_PENDING_MARKER}, or MERGE BRANCH {source} ABORT to abandon it"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B3 fix pass): a schema statement on `table` of
    /// branch `branch` while another open transaction has staged writes of the
    /// table there. SQLSTATE 55006.
    pub fn branch_table_busy(table: &str, branch: &str) -> Self {
        Error::QueryExecution(format!(
            "table \"{table}\" {BRANCH_TABLE_BUSY_MARKER} on branch \"{branch}\": retry after it commits or rolls back"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B4): `CREATE BRANCH name FROM parent AS OF …` of a branch:
    /// a level keeps no history to materialize an anchor from. SQLSTATE 0A000 (the message
    /// carries [`BRANCH_WRITE_UNSUPPORTED_MARKER`]).
    pub fn branch_as_of_needs_main(name: &str, parent: &str) -> Self {
        Error::QueryExecution(format!(
            "CREATE BRANCH {name} AS OF a past time {BRANCH_WRITE_UNSUPPORTED_MARKER}: branch \"{parent}\" keeps no \
             history, only main does; fork it AS OF NOW, or fork main AS OF that time"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B4): a new branch waited out its 30 s budget for writes or
    /// schema statements already running on its parent. SQLSTATE 55006.
    pub fn branch_create_busy(name: &str, parent: &str) -> Self {
        Error::QueryExecution(format!(
            "cannot create branch \"{name}\": branch \"{parent}\" {BRANCH_BUSY_MARKER}; retry when they finish"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B4): a schema statement on main waited for a
    /// `CREATE BRANCH` of main, which waits for the statements before it, and the time
    /// ran out. Nothing changed. SQLSTATE 55006.
    pub fn branch_schema_waited_for_fork(table: &str) -> Self {
        Error::QueryExecution(format!(
            "a schema statement on table \"{table}\" cannot start while a branch of main is being created: \
             main {BRANCH_BUSY_MARKER}; retry when they finish"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B4): `CREATE BRANCH name … AS OF` a time before
    /// `what_happened`, a change of a table of main that a row version written before
    /// it cannot be read under. SQLSTATE 0A000.
    pub fn branch_anchor_before_shape_change(name: &str, what_happened: &str) -> Self {
        Error::QueryExecution(format!(
            "cannot create branch {name} AS OF that time: {what_happened} since, and \
             {BRANCH_ANCHOR_UNREADABLE_MARKER}. Branch from a later time, after its rows last changed, or AS OF NOW"
        ))
    }

    /// Spec R5.5: a read of `table` at a snapshot older than `what_happened`, a change
    /// its rows as they were before cannot be read across (`storage::table_layout`):
    /// `AS OF`, or a transaction whose snapshot is older. The marker is the branch
    /// anchor's, refused for the same reason. SQLSTATE 0A000.
    pub fn history_before_layout_change(table: &str, what_happened: &str) -> Self {
        Error::QueryExecution(format!(
            "cannot read table \"{table}\" as of that time: {what_happened} since, and \
             {BRANCH_ANCHOR_UNREADABLE_MARKER} under its definition now. Read it as of a later time (in a \
             transaction: begin a new one)"
        ))
    }

    /// Spec R5.5: an `ALTER TABLE` whose one write group (the new row images and their
    /// versions, with the copies main's live branches take of them) measured above
    /// `[storage] max_atomic_rewrite_bytes`. Nothing was written. SQLSTATE 54000.
    pub fn atomic_alter_too_large(table: &str, what: &str, bytes: u64, limit: u64) -> Self {
        Error::QueryExecution(format!(
            "ALTER TABLE {table} {what} rewrites every row in one atomic write, and at {bytes} bytes it \
             {ATOMIC_REWRITE_TOO_LARGE_MARKER} ({limit}): nothing was written. Copy the table instead \
             (CREATE TABLE … AS SELECT the columns it keeps, then RENAME), or raise the setting"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B4): `name` has child branch `child`, merged or not,
    /// so it cannot be dropped. SQLSTATE 2BP01.
    pub fn branch_has_child(name: &str, child: &str, count: usize, child_is_merged: bool) -> Self {
        let advice = if child_is_merged {
            format!("branch \"{child}\" is merged and only its record is left: DROP BRANCH {child} first")
        } else {
            format!("branch \"{child}\" reads through it: MERGE or DROP BRANCH {child} first")
        };
        Error::QueryExecution(format!(
            "Cannot drop branch '{name}': {BRANCH_HAS_CHILD_MARKER} ({count}): {advice}"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B4): the name of a table holds a `:`, which the
    /// keys of its rows use to end the name, so its rows would mix with another
    /// table's. SQLSTATE 42602.
    pub fn table_name_holds_colon(name: &str) -> Self {
        let name = name.trim_matches('"');
        Error::QueryExecution(format!(
            "invalid table name \"{name}\": a table {TABLE_NAME_HOLDS_COLON_MARKER}"
        ))
    }

    /// sprinter 6bc4a4040eb7 (phase B4): the name of a schema holds a `:`, so no table
    /// could be created in it (`schema.table` is its tables' name). SQLSTATE 42602.
    pub fn schema_name_holds_colon(name: &str) -> Self {
        let name = name.trim_matches('"');
        Error::QueryExecution(format!(
            "invalid schema name \"{name}\": a schema {TABLE_NAME_HOLDS_COLON_MARKER}"
        ))
    }

    /// sprinter 6bc4a4040eb7 (B1 fix pass): a branch switch inside an open
    /// transaction. SQLSTATE 25001.
    pub fn branch_switch_in_transaction() -> Self {
        Error::QueryExecution(format!(
            "{BRANCH_SWITCH_IN_TRANSACTION_MARKER}; COMMIT or ROLLBACK first"
        ))
    }

    /// True for [`Error::branch_not_active`] and [`Error::branch_binding_gone`]
    /// — "this branch cannot be used", what REST answers with 404.
    pub fn is_branch_unavailable(&self) -> bool {
        match self {
            Error::QueryExecution(message) => {
                message.contains(BRANCH_NOT_ACTIVE_MARKER) || message.contains(BRANCH_BINDING_GONE_MARKER)
            }
            _ => false,
        }
    }

    /// True for [`Error::physical_standby_write_refused`].
    pub fn is_physical_standby_write_refusal(&self) -> bool {
        match self {
            Error::Transaction(message) => message.starts_with(PHYSICAL_STANDBY_WRITE_REFUSED),
            _ => false,
        }
    }

    /// Spec P5.1: a write refused on a fenced primary. `message` is the
    /// fence's full text and starts with [`PRIMARY_FENCED_WRITE_REFUSED`]; a
    /// message that does not is prefixed with it, so the wire always maps the
    /// error to SQLSTATE 25006. A `Transaction` variant for the reason
    /// [`Error::physical_standby_write_refused`] gives.
    pub fn primary_fenced_write_refused(message: impl Into<String>) -> Self {
        let message = message.into();
        if message.starts_with(PRIMARY_FENCED_WRITE_REFUSED) {
            Error::Transaction(message)
        } else {
            Error::Transaction(format!("{PRIMARY_FENCED_WRITE_REFUSED}: {message}"))
        }
    }

    /// True for [`Error::primary_fenced_write_refused`], and for an error a
    /// storage site wrapped around one.
    pub fn is_primary_fenced_write_refusal(&self) -> bool {
        match self {
            Error::Transaction(message) => message.starts_with(PRIMARY_FENCED_WRITE_REFUSED),
            Error::Storage(message) => message.contains(PRIMARY_FENCED_WRITE_REFUSED),
            _ => false,
        }
    }

    /// Create a write-write conflict error (serialization failure, SQLSTATE
    /// 40001). Carries the contended row's identity and the holding
    /// transaction so a client — or an autocommit statement-retry layer — can
    /// act on it instead of parsing a message string.
    pub fn write_conflict(
        table: impl Into<String>,
        row: impl Into<String>,
        holder_txn: u64,
        waiter_txn: u64,
        waited_ms: u64,
    ) -> Self {
        Error::WriteConflict {
            table: table.into(),
            row: row.into(),
            holder_txn,
            waiter_txn,
            waited_ms,
        }
    }

    /// Create a type conversion error
    pub fn type_conversion(msg: impl Into<String>) -> Self {
        Error::TypeConversion(msg.into())
    }

    /// Create a config error
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }

    /// Create an encryption error
    pub fn encryption(msg: impl Into<String>) -> Self {
        Error::Encryption(msg.into())
    }

    /// Create a protocol error
    pub fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    /// Create a vector index error
    pub fn vector_index(msg: impl Into<String>) -> Self {
        Error::VectorIndex(msg.into())
    }

    /// Create a multi-tenant error
    pub fn multi_tenant(msg: impl Into<String>) -> Self {
        Error::MultiTenant(msg.into())
    }

    /// Create an audit error
    pub fn audit(msg: impl Into<String>) -> Self {
        Error::Audit(msg.into())
    }

    /// Create a compression error
    pub fn compression(msg: impl Into<String>) -> Self {
        Error::Compression(msg.into())
    }

    /// Create a branch merge error
    pub fn branch_merge(msg: impl Into<String>) -> Self {
        Error::BranchMerge(msg.into())
    }

    /// Create a merge conflict error
    pub fn merge_conflict(msg: impl Into<String>) -> Self {
        Error::MergeConflict(msg.into())
    }

    /// Create a constraint violation error (FK, CHECK, UNIQUE)
    pub fn constraint_violation(msg: impl Into<String>) -> Self {
        Error::ConstraintViolation(msg.into())
    }

    /// Create a network error
    pub fn network(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    /// Create an authentication error
    pub fn authentication(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    /// Create an I/O error from a message
    pub fn io(msg: impl Into<String>) -> Self {
        Error::Io(std::io::Error::new(std::io::ErrorKind::Other, msg.into()))
    }

    /// Create an internal error
    pub fn internal(msg: impl Into<String>) -> Self {
        Error::Generic(format!("Internal error: {}", msg.into()))
    }

    /// Create an execution error
    pub fn execution(msg: impl Into<String>) -> Self {
        Error::QueryExecution(msg.into())
    }

    /// Create a lock poisoning error
    pub fn lock_poisoned(msg: impl Into<String>) -> Self {
        Error::LockPoisoned(msg.into())
    }

    /// Create a resource limit error
    pub fn resource_limit(msg: impl Into<String>) -> Self {
        Error::Generic(format!("Resource limit exceeded: {}", msg.into()))
    }

    /// Create a deadlock error
    pub fn deadlock(msg: impl Into<String>) -> Self {
        Error::Transaction(format!("Deadlock: {}", msg.into()))
    }

    /// Create a high availability error
    pub fn ha(msg: impl Into<String>) -> Self {
        Error::Generic(format!("HA error: {}", msg.into()))
    }

    /// Create a switchover error
    pub fn switchover(msg: impl Into<String>) -> Self {
        Error::Generic(format!("Switchover error: {}", msg.into()))
    }

    /// Create a replication error
    pub fn replication(msg: impl Into<String>) -> Self {
        Error::Generic(format!("Replication error: {}", msg.into()))
    }
}

/// Helper trait for converting PoisonError to our Error type
pub trait LockResultExt<T> {
    /// Convert a poisoned lock result into our Result type
    fn map_lock_err(self, context: &str) -> Result<T>;
}

impl<T, E> LockResultExt<T> for std::result::Result<T, E>
where
    E: std::fmt::Display,
{
    fn map_lock_err(self, context: &str) -> Result<T> {
        self.map_err(|e| Error::lock_poisoned(format!("{}: {}", context, e)))
    }
}

// Implement conversions for common error types
impl From<rocksdb::Error> for Error {
    fn from(err: rocksdb::Error) -> Self {
        Error::Storage(err.to_string())
    }
}

impl From<sqlparser::parser::ParserError> for Error {
    fn from(err: sqlparser::parser::ParserError) -> Self {
        Error::SqlParse(err.to_string())
    }
}

#[cfg(test)]
mod applied_publication_tests {
    use super::*;
    #[test]
    fn applied_publication_context_keeps_derived_cause_and_is_idempotent() {
        let original = Error::write_group_too_large(1024, 64);
        assert!(!original.is_applied_publication_failure());
        let cause = original.to_string();
        let applied = Error::applied_publication_failure(original);
        assert!(applied.is_applied_publication_failure());
        let message = applied.to_string();
        assert!(message.contains(APPLIED_PUBLICATION_FAILURE_MARKER));
        assert!(message.contains("do not retry these row changes"));
        assert!(message.contains("Cause for the derived vector group only"));
        assert!(message.contains(&cause));
        assert!(message.contains(BRANCH_MATERIALIZE_TOO_LARGE_MARKER));
        assert_eq!(Error::applied_publication_failure(applied).to_string(), message);
    }
}
