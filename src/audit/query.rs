//! Audit log query utilities

use super::{AuditEvent, OperationType};
use crate::{Result, Tuple, Value};
use chrono::{DateTime, Utc};

/// Audit log query builder
pub struct AuditQuery {
    /// Filters to apply
    filters: Vec<AuditFilter>,
    /// Limit number of results
    limit: Option<usize>,
    /// Offset for pagination
    offset: Option<usize>,
}

impl AuditQuery {
    /// Create a new audit query
    pub fn new() -> Self {
        Self {
            filters: Vec::new(),
            limit: Some(1000), // Default limit
            offset: None,
        }
    }

    /// Filter by operation type
    pub fn with_operation(mut self, operation: OperationType) -> Self {
        self.filters.push(AuditFilter::Operation(operation));
        self
    }

    /// Filter by target (table name, etc.)
    pub fn with_target(mut self, target: String) -> Self {
        self.filters.push(AuditFilter::Target(target));
        self
    }

    /// Filter by user
    pub fn with_user(mut self, user: String) -> Self {
        self.filters.push(AuditFilter::User(user));
        self
    }

    /// Filter by session ID
    pub fn with_session(mut self, session_id: String) -> Self {
        self.filters.push(AuditFilter::Session(session_id));
        self
    }

    /// Filter by time range
    pub fn with_time_range(mut self, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        self.filters.push(AuditFilter::TimeRange { start, end });
        self
    }

    /// Filter by success status
    pub fn with_success(mut self, success: bool) -> Self {
        self.filters.push(AuditFilter::Success(success));
        self
    }

    /// Set limit
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set offset
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Build SQL WHERE clause from filters
    pub fn build_where_clause(&self) -> String {
        if self.filters.is_empty() {
            return String::new();
        }

        let conditions: Vec<String> = self.filters.iter().map(|f| f.to_sql()).collect();
        conditions.join(" AND ")
    }

    /// Build complete SQL query
    pub fn build_sql(&self) -> String {
        let mut sql = "SELECT * FROM __audit_log".to_string();

        let where_clause = self.build_where_clause();
        if !where_clause.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_clause);
        }

        sql.push_str(" ORDER BY id DESC");

        if let Some(limit) = self.limit {
            sql.push_str(&format!(" LIMIT {}", limit));
        }

        if let Some(offset) = self.offset {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        sql
    }

    /// Parse tuples into audit events
    pub fn parse_events(tuples: Vec<Tuple>) -> Result<Vec<AuditEvent>> {
        tuples.into_iter().map(Self::parse_event).collect()
    }

    /// Parse a single tuple into an audit event
    fn parse_event(tuple: Tuple) -> Result<AuditEvent> {
        use crate::Error;

        let id = match tuple.get(0) {
            Some(Value::Int8(i)) => *i as u64,
            _ => return Err(Error::audit("Invalid audit event: missing id")),
        };

        let timestamp = match tuple.get(1) {
            Some(Value::Timestamp(ts)) => *ts,
            _ => return Err(Error::audit("Invalid audit event: missing timestamp")),
        };

        let session_id = match tuple.get(2) {
            Some(Value::String(s)) => s.clone(),
            _ => return Err(Error::audit("Invalid audit event: missing session_id")),
        };

        let user = match tuple.get(3) {
            Some(Value::String(s)) => s.clone(),
            _ => return Err(Error::audit("Invalid audit event: missing user")),
        };

        let operation_str = match tuple.get(4) {
            Some(Value::String(s)) => s.clone(),
            _ => return Err(Error::audit("Invalid audit event: missing operation")),
        };
        let operation = parse_operation_type(&operation_str);

        let target = match tuple.get(5) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) => None,
            _ => return Err(Error::audit("Invalid audit event: invalid target")),
        };

        let query = match tuple.get(6) {
            Some(Value::String(s)) => s.clone(),
            _ => return Err(Error::audit("Invalid audit event: missing query")),
        };

        let affected_rows = match tuple.get(7) {
            Some(Value::Int8(i)) => *i as u64,
            _ => return Err(Error::audit("Invalid audit event: missing affected_rows")),
        };

        let success = match tuple.get(8) {
            Some(Value::Boolean(b)) => *b,
            _ => return Err(Error::audit("Invalid audit event: missing success")),
        };

        let error = match tuple.get(9) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) => None,
            _ => return Err(Error::audit("Invalid audit event: invalid error")),
        };

        let checksum = match tuple.get(10) {
            Some(Value::String(s)) => s.clone(),
            _ => return Err(Error::audit("Invalid audit event: missing checksum")),
        };

        Ok(AuditEvent {
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
            metadata: super::AuditMetadata::default(),
            checksum,
        })
    }
}

impl Default for AuditQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// Audit log filter
#[derive(Debug, Clone)]
pub enum AuditFilter {
    /// Filter by operation type
    Operation(OperationType),
    /// Filter by target
    Target(String),
    /// Filter by user
    User(String),
    /// Filter by session ID
    Session(String),
    /// Filter by time range
    TimeRange {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
    /// Filter by success status
    Success(bool),
}

impl AuditFilter {
    /// Convert filter to SQL condition
    pub fn to_sql(&self) -> String {
        match self {
            Self::Operation(op) => format!("operation = '{}'", op),
            Self::Target(target) => format!("target = '{}'", target.replace('\'', "''")),
            Self::User(user) => format!("user = '{}'", user.replace('\'', "''")),
            Self::Session(session) => format!("session_id = '{}'", session.replace('\'', "''")),
            Self::TimeRange { start, end } => {
                format!(
                    "timestamp >= '{}' AND timestamp <= '{}'",
                    start.to_rfc3339(),
                    end.to_rfc3339()
                )
            }
            Self::Success(success) => format!("success = {}", success),
        }
    }
}

/// Parse operation type from string
fn parse_operation_type(s: &str) -> OperationType {
    match s {
        "CREATE_TABLE" => OperationType::CreateTable,
        "DROP_TABLE" => OperationType::DropTable,
        "ALTER_TABLE" => OperationType::AlterTable,
        "CREATE_INDEX" => OperationType::CreateIndex,
        "DROP_INDEX" => OperationType::DropIndex,
        "INSERT" => OperationType::Insert,
        "UPDATE" => OperationType::Update,
        "DELETE" => OperationType::Delete,
        "SELECT" => OperationType::Select,
        "BEGIN" => OperationType::Begin,
        "COMMIT" => OperationType::Commit,
        "ROLLBACK" => OperationType::Rollback,
        "LOGIN" => OperationType::Login,
        "LOGOUT" => OperationType::Logout,
        "GRANT_PERMISSION" => OperationType::GrantPermission,
        "REVOKE_PERMISSION" => OperationType::RevokePermission,
        "BACKUP" => OperationType::Backup,
        "RESTORE" => OperationType::Restore,
        "VACUUM" => OperationType::Vacuum,
        other => {
            if let Some(stripped) = other.strip_prefix("OTHER_") {
                OperationType::Other(stripped.to_string())
            } else {
                OperationType::Other(other.to_string())
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_query_builder() {
        let query = AuditQuery::new()
            .with_operation(OperationType::Insert)
            .with_target("users".to_string())
            .limit(100);

        let sql = query.build_sql();
        assert!(sql.contains("operation = 'INSERT'"));
        assert!(sql.contains("target = 'users'"));
        assert!(sql.contains("LIMIT 100"));
    }

    #[test]
    fn test_time_range_filter() {
        use chrono::TimeZone;

        let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2024, 12, 31, 23, 59, 59).unwrap();

        let query = AuditQuery::new().with_time_range(start, end);

        let sql = query.build_sql();
        assert!(sql.contains("timestamp >="));
        assert!(sql.contains("timestamp <="));
    }

    #[test]
    fn test_parse_operation_type() {
        assert_eq!(
            parse_operation_type("CREATE_TABLE"),
            OperationType::CreateTable
        );
        assert_eq!(parse_operation_type("INSERT"), OperationType::Insert);
    }

    #[test]
    fn test_filter_to_sql() {
        let filter = AuditFilter::User("alice".to_string());
        assert_eq!(filter.to_sql(), "user = 'alice'");

        let filter = AuditFilter::Success(true);
        assert_eq!(filter.to_sql(), "success = true");
    }
}
