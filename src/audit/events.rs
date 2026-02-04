//! Audit event types and structures

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Audit event representing a database operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unique event ID
    pub id: u64,
    /// Timestamp when the operation occurred
    pub timestamp: DateTime<Utc>,
    /// Session ID that performed the operation
    pub session_id: String,
    /// User who performed the operation
    pub user: String,
    /// Type of operation
    pub operation: OperationType,
    /// Target object (table name, etc.)
    pub target: Option<String>,
    /// SQL query or command
    pub query: String,
    /// Number of rows affected
    pub affected_rows: u64,
    /// Whether the operation succeeded
    pub success: bool,
    /// Error message if the operation failed
    pub error: Option<String>,
    /// Additional metadata
    pub metadata: AuditMetadata,
    /// Cryptographic checksum for tamper detection
    pub checksum: String,
}

impl AuditEvent {
    /// Create a new audit event
    pub fn new(
        id: u64,
        session_id: String,
        user: String,
        operation: OperationType,
        target: Option<String>,
        query: String,
        affected_rows: u64,
        success: bool,
        error: Option<String>,
        metadata: AuditMetadata,
    ) -> Self {
        let timestamp = Utc::now();
        let mut event = Self {
            id,
            timestamp,
            session_id,
            user,
            operation,
            target,
            query,
            affected_rows,
            success,
            error,
            metadata,
            checksum: String::new(),
        };

        // Calculate checksum
        event.checksum = event.calculate_checksum();
        event
    }

    /// Calculate cryptographic checksum for this event
    pub fn calculate_checksum(&self) -> String {
        use sha2::{Sha256, Digest};

        let mut hasher = Sha256::new();
        hasher.update(self.id.to_le_bytes());
        hasher.update(self.timestamp.to_rfc3339().as_bytes());
        hasher.update(self.session_id.as_bytes());
        hasher.update(self.user.as_bytes());
        hasher.update(self.operation.to_string().as_bytes());
        if let Some(target) = &self.target {
            hasher.update(target.as_bytes());
        }
        hasher.update(self.query.as_bytes());
        hasher.update(self.affected_rows.to_le_bytes());
        hasher.update(&[if self.success { 1u8 } else { 0u8 }]);
        if let Some(error) = &self.error {
            hasher.update(error.as_bytes());
        }

        let result = hasher.finalize();
        format!("{:x}", result)
    }

    /// Verify the checksum of this event
    pub fn verify_checksum(&self) -> bool {
        let calculated = self.calculate_checksum();
        calculated == self.checksum
    }
}

/// Type of database operation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationType {
    // DDL Operations
    CreateTable,
    DropTable,
    AlterTable,
    CreateIndex,
    DropIndex,

    // DML Operations
    Insert,
    Update,
    Delete,
    Select,

    // Transaction Operations
    Begin,
    Commit,
    Rollback,

    // Auth Operations
    Login,
    Logout,
    GrantPermission,
    RevokePermission,

    // System Operations
    Backup,
    Restore,
    Vacuum,

    // Other
    Other(String),
}

impl OperationType {
    /// Check if this is a DDL operation
    pub fn is_ddl(&self) -> bool {
        matches!(
            self,
            Self::CreateTable
                | Self::DropTable
                | Self::AlterTable
                | Self::CreateIndex
                | Self::DropIndex
        )
    }

    /// Check if this is a DML operation
    pub fn is_dml(&self) -> bool {
        matches!(
            self,
            Self::Insert | Self::Update | Self::Delete | Self::Select
        )
    }

    /// Check if this is a transaction operation
    pub fn is_transaction(&self) -> bool {
        matches!(self, Self::Begin | Self::Commit | Self::Rollback)
    }

    /// Check if this is an auth operation
    pub fn is_auth(&self) -> bool {
        matches!(
            self,
            Self::Login | Self::Logout | Self::GrantPermission | Self::RevokePermission
        )
    }

    /// Parse from SQL statement type
    pub fn from_sql_statement(sql: &str) -> Self {
        let sql_upper = sql.trim().to_uppercase();
        if sql_upper.starts_with("CREATE TABLE") {
            Self::CreateTable
        } else if sql_upper.starts_with("DROP TABLE") {
            Self::DropTable
        } else if sql_upper.starts_with("ALTER TABLE") {
            Self::AlterTable
        } else if sql_upper.starts_with("CREATE INDEX") {
            Self::CreateIndex
        } else if sql_upper.starts_with("DROP INDEX") {
            Self::DropIndex
        } else if sql_upper.starts_with("INSERT") {
            Self::Insert
        } else if sql_upper.starts_with("UPDATE") {
            Self::Update
        } else if sql_upper.starts_with("DELETE") {
            Self::Delete
        } else if sql_upper.starts_with("SELECT") {
            Self::Select
        } else if sql_upper.starts_with("BEGIN") {
            Self::Begin
        } else if sql_upper.starts_with("COMMIT") {
            Self::Commit
        } else if sql_upper.starts_with("ROLLBACK") {
            Self::Rollback
        } else {
            Self::Other(sql_upper.split_whitespace().next().unwrap_or("UNKNOWN").to_string())
        }
    }
}

impl std::fmt::Display for OperationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateTable => write!(f, "CREATE_TABLE"),
            Self::DropTable => write!(f, "DROP_TABLE"),
            Self::AlterTable => write!(f, "ALTER_TABLE"),
            Self::CreateIndex => write!(f, "CREATE_INDEX"),
            Self::DropIndex => write!(f, "DROP_INDEX"),
            Self::Insert => write!(f, "INSERT"),
            Self::Update => write!(f, "UPDATE"),
            Self::Delete => write!(f, "DELETE"),
            Self::Select => write!(f, "SELECT"),
            Self::Begin => write!(f, "BEGIN"),
            Self::Commit => write!(f, "COMMIT"),
            Self::Rollback => write!(f, "ROLLBACK"),
            Self::Login => write!(f, "LOGIN"),
            Self::Logout => write!(f, "LOGOUT"),
            Self::GrantPermission => write!(f, "GRANT_PERMISSION"),
            Self::RevokePermission => write!(f, "REVOKE_PERMISSION"),
            Self::Backup => write!(f, "BACKUP"),
            Self::Restore => write!(f, "RESTORE"),
            Self::Vacuum => write!(f, "VACUUM"),
            Self::Other(s) => write!(f, "OTHER_{}", s),
        }
    }
}

/// Additional metadata for audit events
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditMetadata {
    /// Client IP address
    pub client_ip: Option<String>,
    /// Application name
    pub application_name: Option<String>,
    /// Database name
    pub database_name: Option<String>,
    /// Query execution time (milliseconds)
    pub execution_time_ms: Option<u64>,
    /// Additional custom fields
    pub custom_fields: std::collections::HashMap<String, String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_event_checksum() {
        let metadata = AuditMetadata::default();
        let event = AuditEvent::new(
            1,
            "session-123".to_string(),
            "alice".to_string(),
            OperationType::Insert,
            Some("users".to_string()),
            "INSERT INTO users VALUES (1, 'Alice')".to_string(),
            1,
            true,
            None,
            metadata,
        );

        assert!(!event.checksum.is_empty());
        assert!(event.verify_checksum());
    }

    #[test]
    fn test_operation_type_from_sql() {
        assert_eq!(
            OperationType::from_sql_statement("CREATE TABLE users (id INT)"),
            OperationType::CreateTable
        );
        assert_eq!(
            OperationType::from_sql_statement("INSERT INTO users VALUES (1)"),
            OperationType::Insert
        );
        assert_eq!(
            OperationType::from_sql_statement("SELECT * FROM users"),
            OperationType::Select
        );
    }

    #[test]
    fn test_operation_type_classification() {
        assert!(OperationType::CreateTable.is_ddl());
        assert!(OperationType::Insert.is_dml());
        assert!(OperationType::Begin.is_transaction());
        assert!(OperationType::Login.is_auth());
    }
}
