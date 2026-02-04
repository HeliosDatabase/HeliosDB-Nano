//! Audit logging configuration

use serde::{Deserialize, Serialize};

/// Audit logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Enable audit logging
    pub enabled: bool,

    /// Log DDL operations (CREATE, DROP, ALTER)
    pub log_ddl: bool,

    /// Log DML operations (INSERT, UPDATE, DELETE)
    pub log_dml: bool,

    /// Log SELECT queries (can be very verbose)
    pub log_select: bool,

    /// Log transaction operations (BEGIN, COMMIT, ROLLBACK)
    pub log_transactions: bool,

    /// Log authentication operations
    pub log_auth: bool,

    /// Retention period in days (0 = infinite)
    pub retention_days: u32,

    /// Async buffer size (number of events before flushing)
    pub async_buffer_size: usize,

    /// Enable cryptographic checksums
    pub enable_checksums: bool,

    /// Truncate long queries to this length (0 = no truncation)
    pub max_query_length: usize,

    /// Additional metadata to capture
    pub capture_metadata: MetadataCapture,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_ddl: true,
            log_dml: true,
            log_select: false,  // Too verbose by default
            log_transactions: false,
            log_auth: true,
            retention_days: 90,
            async_buffer_size: 100,
            enable_checksums: true,
            max_query_length: 10000,  // 10KB
            capture_metadata: MetadataCapture::default(),
        }
    }
}

impl AuditConfig {
    /// Create a minimal audit configuration (DDL only)
    pub fn minimal() -> Self {
        Self {
            enabled: true,
            log_ddl: true,
            log_dml: false,
            log_select: false,
            log_transactions: false,
            log_auth: false,
            retention_days: 30,
            async_buffer_size: 50,
            enable_checksums: false,
            max_query_length: 5000,
            capture_metadata: MetadataCapture::minimal(),
        }
    }

    /// Create a verbose audit configuration (everything)
    pub fn verbose() -> Self {
        Self {
            enabled: true,
            log_ddl: true,
            log_dml: true,
            log_select: true,
            log_transactions: true,
            log_auth: true,
            retention_days: 365,
            async_buffer_size: 500,
            enable_checksums: true,
            max_query_length: 50000,
            capture_metadata: MetadataCapture::verbose(),
        }
    }

    /// Create a compliance-focused configuration (SOC2, HIPAA, GDPR)
    pub fn compliance() -> Self {
        Self {
            enabled: true,
            log_ddl: true,
            log_dml: true,
            log_select: false,  // SELECT typically doesn't need to be audited for compliance
            log_transactions: true,
            log_auth: true,
            retention_days: 2555,  // 7 years for some compliance standards
            async_buffer_size: 100,
            enable_checksums: true,  // Tamper detection is critical
            max_query_length: 10000,
            capture_metadata: MetadataCapture::compliance(),
        }
    }

    /// Check if a given operation type should be logged
    pub fn should_log(&self, operation: &super::OperationType) -> bool {
        if !self.enabled {
            return false;
        }

        use super::OperationType;
        match operation {
            op if op.is_ddl() => self.log_ddl,
            OperationType::Select => self.log_select,
            op if op.is_dml() => self.log_dml,
            op if op.is_transaction() => self.log_transactions,
            op if op.is_auth() => self.log_auth,
            _ => true,  // Log other operations by default
        }
    }

    /// Truncate query if needed
    pub fn truncate_query(&self, query: &str) -> String {
        if self.max_query_length == 0 || query.len() <= self.max_query_length {
            query.to_string()
        } else {
            format!(
                "{}... [truncated {} chars]",
                &query[..self.max_query_length],
                query.len() - self.max_query_length
            )
        }
    }
}

/// Configuration for metadata capture
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataCapture {
    /// Capture client IP address
    pub capture_client_ip: bool,

    /// Capture application name
    pub capture_application_name: bool,

    /// Capture database name
    pub capture_database_name: bool,

    /// Capture query execution time
    pub capture_execution_time: bool,

    /// Capture custom fields
    pub capture_custom_fields: bool,
}

impl Default for MetadataCapture {
    fn default() -> Self {
        Self {
            capture_client_ip: true,
            capture_application_name: true,
            capture_database_name: true,
            capture_execution_time: true,
            capture_custom_fields: false,
        }
    }
}

impl MetadataCapture {
    /// Minimal metadata capture
    pub fn minimal() -> Self {
        Self {
            capture_client_ip: false,
            capture_application_name: false,
            capture_database_name: false,
            capture_execution_time: false,
            capture_custom_fields: false,
        }
    }

    /// Verbose metadata capture
    pub fn verbose() -> Self {
        Self {
            capture_client_ip: true,
            capture_application_name: true,
            capture_database_name: true,
            capture_execution_time: true,
            capture_custom_fields: true,
        }
    }

    /// Compliance-focused metadata capture
    pub fn compliance() -> Self {
        Self {
            capture_client_ip: true,
            capture_application_name: true,
            capture_database_name: true,
            capture_execution_time: true,
            capture_custom_fields: false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AuditConfig::default();
        assert!(config.enabled);
        assert!(config.log_ddl);
        assert!(config.log_dml);
        assert!(!config.log_select);
    }

    #[test]
    fn test_minimal_config() {
        let config = AuditConfig::minimal();
        assert!(config.log_ddl);
        assert!(!config.log_dml);
        assert!(!config.log_select);
    }

    #[test]
    fn test_verbose_config() {
        let config = AuditConfig::verbose();
        assert!(config.log_ddl);
        assert!(config.log_dml);
        assert!(config.log_select);
    }

    #[test]
    fn test_should_log() {
        let config = AuditConfig::default();
        use super::super::OperationType;

        assert!(config.should_log(&OperationType::CreateTable));
        assert!(config.should_log(&OperationType::Insert));
        assert!(!config.should_log(&OperationType::Select));
    }

    #[test]
    fn test_truncate_query() {
        let config = AuditConfig {
            max_query_length: 10,
            ..Default::default()
        };

        let short = "SELECT *";
        assert_eq!(config.truncate_query(short), short);

        let long = "SELECT * FROM very_long_table_name";
        let truncated = config.truncate_query(long);
        assert!(truncated.contains("truncated"));
        assert!(truncated.len() < long.len() + 50);
    }
}
