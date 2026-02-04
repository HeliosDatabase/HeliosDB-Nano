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
//! use heliosdb_lite::{EmbeddedDatabase, Error, Result};
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
/// use heliosdb_lite::Error;
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
