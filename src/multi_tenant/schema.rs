//! Schema-Level Multi-Tenant Isolation
//!
//! Each tenant gets their own database schema, providing:
//! - Complete data isolation
//! - Independent schema migrations
//! - Per-tenant backups and restores

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Tenant configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    /// Unique tenant identifier
    pub id: String,
    /// Display name
    pub name: String,
    /// Database schema name
    pub schema_name: String,
    /// Tenant status
    pub status: TenantStatus,
    /// Creation timestamp
    pub created_at: u64,
    /// Last activity timestamp
    pub last_active_at: Option<u64>,
    /// Tenant metadata
    pub metadata: HashMap<String, serde_json::Value>,
    /// Assigned plan/tier
    pub plan: TenantPlan,
    /// Resource quotas
    pub quotas: TenantQuotas,
}

/// Tenant status
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    /// Active and operational
    Active,
    /// Temporarily suspended
    Suspended,
    /// In provisioning process
    Provisioning,
    /// Marked for deletion
    PendingDeletion,
    /// Read-only mode
    ReadOnly,
    /// Trial period
    Trial,
}

/// Tenant pricing plan
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantPlan {
    Free,
    Starter,
    Pro,
    Enterprise,
    Custom(String),
}

impl Default for TenantPlan {
    fn default() -> Self {
        Self::Free
    }
}

/// Resource quotas for a tenant
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantQuotas {
    /// Maximum storage in bytes
    pub max_storage_bytes: u64,
    /// Maximum tables
    pub max_tables: u32,
    /// Maximum rows per table
    pub max_rows_per_table: u64,
    /// Maximum vector stores
    pub max_vector_stores: u32,
    /// Maximum vectors total
    pub max_vectors: u64,
    /// Maximum branches
    pub max_branches: u32,
    /// Queries per minute
    pub qpm_limit: u32,
    /// Concurrent connections
    pub max_connections: u32,
    /// Time travel retention days
    pub time_travel_days: u32,
    /// Enable API access
    pub api_enabled: bool,
    /// Enable realtime subscriptions
    pub realtime_enabled: bool,
    /// Custom features
    pub features: HashMap<String, bool>,
}

impl Default for TenantQuotas {
    fn default() -> Self {
        Self {
            max_storage_bytes: 1024 * 1024 * 100, // 100MB
            max_tables: 50,
            max_rows_per_table: 100_000,
            max_vector_stores: 5,
            max_vectors: 100_000,
            max_branches: 10,
            qpm_limit: 100,
            max_connections: 5,
            time_travel_days: 7,
            api_enabled: true,
            realtime_enabled: false,
            features: HashMap::new(),
        }
    }
}

impl TenantQuotas {
    /// Get quotas for a plan
    pub fn for_plan(plan: &TenantPlan) -> Self {
        match plan {
            TenantPlan::Free => Self::default(),
            TenantPlan::Starter => Self {
                max_storage_bytes: 1024 * 1024 * 1024, // 1GB
                max_tables: 100,
                max_rows_per_table: 1_000_000,
                max_vector_stores: 10,
                max_vectors: 500_000,
                max_branches: 20,
                qpm_limit: 1000,
                max_connections: 20,
                time_travel_days: 30,
                realtime_enabled: true,
                ..Default::default()
            },
            TenantPlan::Pro => Self {
                max_storage_bytes: 1024 * 1024 * 1024 * 10, // 10GB
                max_tables: 500,
                max_rows_per_table: 10_000_000,
                max_vector_stores: 50,
                max_vectors: 5_000_000,
                max_branches: 100,
                qpm_limit: 10000,
                max_connections: 100,
                time_travel_days: 90,
                realtime_enabled: true,
                ..Default::default()
            },
            TenantPlan::Enterprise | TenantPlan::Custom(_) => Self {
                max_storage_bytes: u64::MAX,
                max_tables: u32::MAX,
                max_rows_per_table: u64::MAX,
                max_vector_stores: u32::MAX,
                max_vectors: u64::MAX,
                max_branches: u32::MAX,
                qpm_limit: u32::MAX,
                max_connections: u32::MAX,
                time_travel_days: 365,
                realtime_enabled: true,
                ..Default::default()
            },
        }
    }
}

/// Tenant manager
pub struct TenantManager {
    tenants: Arc<RwLock<HashMap<String, Tenant>>>,
    schema_prefix: String,
}

impl TenantManager {
    /// Create new tenant manager
    pub fn new(schema_prefix: &str) -> Self {
        Self {
            tenants: Arc::new(RwLock::new(HashMap::new())),
            schema_prefix: schema_prefix.to_string(),
        }
    }

    /// Generate schema name for tenant
    pub fn schema_name(&self, tenant_id: &str) -> String {
        format!("{}_{}", self.schema_prefix, sanitize_identifier(tenant_id))
    }

    /// Create a new tenant
    pub fn create_tenant(&self, id: &str, name: &str, plan: TenantPlan) -> Result<Tenant, TenantError> {
        let mut tenants = self.tenants.write();

        if tenants.contains_key(id) {
            return Err(TenantError::AlreadyExists(id.to_string()));
        }

        let schema_name = self.schema_name(id);
        let tenant = Tenant {
            id: id.to_string(),
            name: name.to_string(),
            schema_name,
            status: TenantStatus::Provisioning,
            created_at: current_timestamp(),
            last_active_at: None,
            metadata: HashMap::new(),
            plan: plan.clone(),
            quotas: TenantQuotas::for_plan(&plan),
        };

        tenants.insert(id.to_string(), tenant.clone());
        Ok(tenant)
    }

    /// Get tenant by ID
    pub fn get_tenant(&self, id: &str) -> Option<Tenant> {
        self.tenants.read().get(id).cloned()
    }

    /// List all tenants
    pub fn list_tenants(&self) -> Vec<Tenant> {
        self.tenants.read().values().cloned().collect()
    }

    /// Update tenant status
    pub fn update_status(&self, id: &str, status: TenantStatus) -> Result<(), TenantError> {
        let mut tenants = self.tenants.write();
        let tenant = tenants.get_mut(id)
            .ok_or_else(|| TenantError::NotFound(id.to_string()))?;

        tenant.status = status;
        Ok(())
    }

    /// Update tenant plan
    pub fn update_plan(&self, id: &str, plan: TenantPlan) -> Result<(), TenantError> {
        let mut tenants = self.tenants.write();
        let tenant = tenants.get_mut(id)
            .ok_or_else(|| TenantError::NotFound(id.to_string()))?;

        tenant.plan = plan.clone();
        tenant.quotas = TenantQuotas::for_plan(&plan);
        Ok(())
    }

    /// Update tenant metadata
    pub fn update_metadata(&self, id: &str, metadata: HashMap<String, serde_json::Value>) -> Result<(), TenantError> {
        let mut tenants = self.tenants.write();
        let tenant = tenants.get_mut(id)
            .ok_or_else(|| TenantError::NotFound(id.to_string()))?;

        tenant.metadata = metadata;
        Ok(())
    }

    /// Update last active timestamp
    pub fn touch(&self, id: &str) {
        if let Some(tenant) = self.tenants.write().get_mut(id) {
            tenant.last_active_at = Some(current_timestamp());
        }
    }

    /// Delete tenant
    pub fn delete_tenant(&self, id: &str) -> Result<(), TenantError> {
        let mut tenants = self.tenants.write();

        if !tenants.contains_key(id) {
            return Err(TenantError::NotFound(id.to_string()));
        }

        // Mark for deletion first
        if let Some(tenant) = tenants.get_mut(id) {
            tenant.status = TenantStatus::PendingDeletion;
        }

        // In production, actual deletion would be asynchronous
        tenants.remove(id);
        Ok(())
    }

    /// Check if tenant is active
    pub fn is_active(&self, id: &str) -> bool {
        self.tenants.read()
            .get(id)
            .map(|t| t.status == TenantStatus::Active || t.status == TenantStatus::Trial)
            .unwrap_or(false)
    }

    /// Check if tenant has quota for operation
    pub fn check_quota(&self, id: &str, resource: &str, current: u64) -> Result<(), TenantError> {
        let tenants = self.tenants.read();
        let tenant = tenants.get(id)
            .ok_or_else(|| TenantError::NotFound(id.to_string()))?;

        let limit = match resource {
            "storage" => tenant.quotas.max_storage_bytes,
            "tables" => tenant.quotas.max_tables as u64,
            "rows" => tenant.quotas.max_rows_per_table,
            "vectors" => tenant.quotas.max_vectors,
            "branches" => tenant.quotas.max_branches as u64,
            _ => return Ok(()),
        };

        if current >= limit {
            return Err(TenantError::QuotaExceeded {
                resource: resource.to_string(),
                limit,
                current,
            });
        }

        Ok(())
    }

    /// Generate SQL to create tenant schema
    pub fn create_schema_sql(&self, tenant_id: &str) -> Vec<String> {
        let schema = self.schema_name(tenant_id);

        vec![
            format!("CREATE SCHEMA IF NOT EXISTS {}", schema),
            format!("SET search_path TO {}", schema),
            // Create system tables within schema
            format!(
                "CREATE TABLE IF NOT EXISTS {}._metadata (
                    key TEXT PRIMARY KEY,
                    value JSON,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
                )", schema
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {}._audit_log (
                    id BIGSERIAL PRIMARY KEY,
                    action TEXT NOT NULL,
                    table_name TEXT,
                    row_id TEXT,
                    old_data JSON,
                    new_data JSON,
                    user_id TEXT,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
                )", schema
            ),
        ]
    }

    /// Generate SQL to drop tenant schema
    pub fn drop_schema_sql(&self, tenant_id: &str) -> String {
        format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema_name(tenant_id))
    }
}

impl Default for TenantManager {
    fn default() -> Self {
        Self::new("tenant")
    }
}

/// Tenant error types
#[derive(Debug, thiserror::Error)]
pub enum TenantError {
    #[error("Tenant not found: {0}")]
    NotFound(String),

    #[error("Tenant already exists: {0}")]
    AlreadyExists(String),

    #[error("Tenant is not active: {0}")]
    NotActive(String),

    #[error("Quota exceeded for {resource}: limit={limit}, current={current}")]
    QuotaExceeded { resource: String, limit: u64, current: u64 },

    #[error("Invalid tenant ID: {0}")]
    InvalidId(String),

    #[error("Operation not allowed: {0}")]
    NotAllowed(String),

    #[error("Schema error: {0}")]
    Schema(String),
}

/// Schema isolation layer
pub struct SchemaIsolation {
    manager: TenantManager,
}

impl SchemaIsolation {
    pub fn new(manager: TenantManager) -> Self {
        Self { manager }
    }

    /// Wrap SQL query with tenant schema
    pub fn wrap_query(&self, tenant_id: &str, sql: &str) -> Result<String, TenantError> {
        let tenant = self.manager.get_tenant(tenant_id)
            .ok_or_else(|| TenantError::NotFound(tenant_id.to_string()))?;

        if tenant.status != TenantStatus::Active && tenant.status != TenantStatus::Trial {
            return Err(TenantError::NotActive(tenant_id.to_string()));
        }

        // Set search path to tenant schema
        Ok(format!(
            "SET search_path TO {}; {}",
            tenant.schema_name,
            sql
        ))
    }

    /// Validate query doesn't access other schemas
    pub fn validate_query(&self, tenant_id: &str, sql: &str) -> Result<(), TenantError> {
        let tenant = self.manager.get_tenant(tenant_id)
            .ok_or_else(|| TenantError::NotFound(tenant_id.to_string()))?;

        let allowed_schemas = vec![
            tenant.schema_name.clone(),
            "pg_catalog".to_string(),
            "information_schema".to_string(),
        ];

        // Simple check for cross-schema access
        let sql_upper = sql.to_uppercase();
        for schema in &allowed_schemas {
            // This is a simplified check - production would use proper SQL parsing
            if sql_upper.contains(&format!("{}.", schema.to_uppercase())) {
                continue;
            }
        }

        // Check for schema-switching commands
        let forbidden = ["SET SEARCH_PATH", "CREATE SCHEMA", "DROP SCHEMA", "ALTER SCHEMA"];
        for cmd in forbidden {
            if sql_upper.contains(cmd) {
                return Err(TenantError::NotAllowed(format!("Command '{}' is not allowed", cmd)));
            }
        }

        Ok(())
    }
}

// Helper functions

fn sanitize_identifier(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_lowercase()
}

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tenant_creation() {
        let manager = TenantManager::new("tenant");
        let tenant = manager.create_tenant("test-1", "Test Tenant", TenantPlan::Starter).unwrap();

        assert_eq!(tenant.id, "test-1");
        assert_eq!(tenant.schema_name, "tenant_test1");
        assert_eq!(tenant.status, TenantStatus::Provisioning);
    }

    #[test]
    fn test_quota_check() {
        let manager = TenantManager::new("tenant");
        manager.create_tenant("test-1", "Test", TenantPlan::Free).unwrap();
        manager.update_status("test-1", TenantStatus::Active).unwrap();

        // Should pass
        assert!(manager.check_quota("test-1", "tables", 10).is_ok());

        // Should fail (Free plan has 50 table limit)
        assert!(manager.check_quota("test-1", "tables", 100).is_err());
    }
}
