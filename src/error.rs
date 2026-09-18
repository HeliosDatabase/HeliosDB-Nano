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
