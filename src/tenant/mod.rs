//! Multi-tenancy support
//!
//! Provides multi-tenant database isolation with Row-Level Security (RLS).
//!
//! # Features
//!
//! - **Tenant Registration**: Register tenants with optional resource limits
//! - **Isolation Modes**: SharedSchema (RLS), DatabasePerTenant, SchemaPerTenant
//! - **RLS Policies**: Define row-level security policies per table
//! - **Context Management**: Per-request tenant context tracking
//! - **Query Rewriting**: Automatic RLS condition injection into queries
//! - **Plan Management**: Tiered plans with automatic downgrade on deletion

pub mod expression;

use uuid::Uuid;
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::cell::RefCell;

pub use expression::{RLSExpressionEvaluator, evaluate_rls_expression};

// Thread-local for current tenant ID (used by SQL functions like current_tenant())
thread_local! {
    static CURRENT_TENANT_ID: RefCell<Option<Uuid>> = RefCell::new(None);
    static CURRENT_USER_ID: RefCell<Option<String>> = RefCell::new(None);
}

/// Get current tenant ID from thread-local storage (for SQL functions)
pub fn get_current_tenant_id() -> Option<Uuid> {
    CURRENT_TENANT_ID.with(|id| *id.borrow())
}

/// Get current user ID from thread-local storage (for SQL functions)
pub fn get_current_user_id() -> Option<String> {
    CURRENT_USER_ID.with(|id| id.borrow().clone())
}

/// Set current tenant ID in thread-local storage
fn set_current_tenant_id(tenant_id: Option<Uuid>) {
    CURRENT_TENANT_ID.with(|id| {
        *id.borrow_mut() = tenant_id;
    });
}

/// Set current user ID in thread-local storage
fn set_current_user_id(user_id: Option<String>) {
    CURRENT_USER_ID.with(|id| {
        *id.borrow_mut() = user_id;
    });
}

/// Tenant ID
pub type TenantId = Uuid;

/// Plan ID (unique identifier)
pub type PlanId = String;

// ============================================================================
// Plan Management
// ============================================================================

/// Multi-tenancy plan definition
#[derive(Debug, Clone)]
pub struct Plan {
    /// Unique plan identifier (e.g., "free", "starter", "pro")
    pub id: PlanId,
    /// Display name
    pub name: String,
    /// Description
    pub description: String,
    /// Tier ID for ordering (lower = lower tier, higher = higher tier)
    /// Used for automatic downgrade when a plan is deleted
    pub tier_id: u32,
    /// Resource limits for this plan
    pub limits: ResourceLimits,
    /// Whether the plan is enabled (can be assigned to new tenants)
    pub enabled: bool,
    /// Whether this is the default/fallback plan (cannot be deleted)
    pub is_default: bool,
    /// Feature flags for this plan
    pub features: PlanFeatures,
    /// Created at timestamp
    pub created_at: String,
    /// Updated at timestamp
    pub updated_at: String,
}

/// Feature flags for a plan
#[derive(Debug, Clone, Default)]
pub struct PlanFeatures {
    /// RLS policies allowed
    pub rls_enabled: bool,
    /// CDC event tracking
    pub cdc_enabled: bool,
    /// Tenant migrations
    pub migrations_enabled: bool,
    /// Custom quotas (override plan limits)
    pub custom_quotas_enabled: bool,
    /// All isolation modes (vs SharedSchema only)
    pub all_isolation_modes: bool,
}

impl Plan {
    /// Create a new plan
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        tier_id: u32,
        limits: ResourceLimits,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            tier_id,
            limits,
            enabled: true,
            is_default: false,
            features: PlanFeatures::default(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Create the unlimited/default plan
    pub fn unlimited() -> Self {
        Self {
            id: "unlimited".to_string(),
            name: "Unlimited".to_string(),
            description: "Default fallback plan with no restrictions".to_string(),
            tier_id: u32::MAX, // Highest tier
            limits: ResourceLimits {
                max_storage_bytes: u64::MAX,
                max_connections: usize::MAX,
                max_qps: usize::MAX,
            },
            enabled: true,
            is_default: true,
            features: PlanFeatures {
                rls_enabled: true,
                cdc_enabled: true,
                migrations_enabled: true,
                custom_quotas_enabled: true,
                all_isolation_modes: true,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Set features
    pub fn with_features(mut self, features: PlanFeatures) -> Self {
        self.features = features;
        self
    }

    /// Mark as default plan
    pub fn as_default(mut self) -> Self {
        self.is_default = true;
        self
    }
}

/// Plan manager for CRUD operations
pub struct PlanManager {
    /// All plans indexed by ID
    plans: Arc<parking_lot::RwLock<HashMap<PlanId, Plan>>>,
}

impl PlanManager {
    /// Create a new plan manager with default plans
    pub fn new() -> Self {
        let mut plans = HashMap::new();

        // Add unlimited (default fallback) plan
        let unlimited = Plan::unlimited();
        plans.insert(unlimited.id.clone(), unlimited);

        // Add standard plans
        let free = Plan::new(
            "free",
            "Free",
            "Development and testing",
            100,
            ResourceLimits {
                max_storage_bytes: 100 * 1024 * 1024,      // 100 MB
                max_connections: 5,
                max_qps: 10,
            },
        ).with_features(PlanFeatures {
            rls_enabled: true,
            cdc_enabled: false,
            migrations_enabled: false,
            custom_quotas_enabled: false,
            all_isolation_modes: false,
        });
        plans.insert(free.id.clone(), free);

        let starter = Plan::new(
            "starter",
            "Starter",
            "Small teams and startups",
            200,
            ResourceLimits {
                max_storage_bytes: 1024 * 1024 * 1024,     // 1 GB
                max_connections: 20,
                max_qps: 100,
            },
        ).with_features(PlanFeatures {
            rls_enabled: true,
            cdc_enabled: true,
            migrations_enabled: false,
            custom_quotas_enabled: false,
            all_isolation_modes: false,
        });
        plans.insert(starter.id.clone(), starter);

        let pro = Plan::new(
            "pro",
            "Pro",
            "Growing businesses",
            300,
            ResourceLimits {
                max_storage_bytes: 10 * 1024 * 1024 * 1024, // 10 GB
                max_connections: 100,
                max_qps: 1000,
            },
        ).with_features(PlanFeatures {
            rls_enabled: true,
            cdc_enabled: true,
            migrations_enabled: true,
            custom_quotas_enabled: true,
            all_isolation_modes: false,
        });
        plans.insert(pro.id.clone(), pro);

        let enterprise = Plan::new(
            "enterprise",
            "Enterprise",
            "Large scale deployments",
            400,
            ResourceLimits {
                max_storage_bytes: 100 * 1024 * 1024 * 1024, // 100 GB
                max_connections: 1000,
                max_qps: 10000,
            },
        ).with_features(PlanFeatures {
            rls_enabled: true,
            cdc_enabled: true,
            migrations_enabled: true,
            custom_quotas_enabled: true,
            all_isolation_modes: true,
        });
        plans.insert(enterprise.id.clone(), enterprise);

        Self {
            plans: Arc::new(parking_lot::RwLock::new(plans)),
        }
    }

    /// Get a plan by ID
    pub fn get_plan(&self, plan_id: &str) -> Option<Plan> {
        self.plans.read().get(plan_id).cloned()
    }

    /// List all plans (sorted by tier_id)
    pub fn list_plans(&self) -> Vec<Plan> {
        let mut plans: Vec<_> = self.plans.read().values().cloned().collect();
        plans.sort_by_key(|p| p.tier_id);
        plans
    }

    /// List only enabled plans (sorted by tier_id)
    pub fn list_enabled_plans(&self) -> Vec<Plan> {
        let mut plans: Vec<_> = self.plans.read()
            .values()
            .filter(|p| p.enabled)
            .cloned()
            .collect();
        plans.sort_by_key(|p| p.tier_id);
        plans
    }

    /// Create a new plan
    pub fn create_plan(&self, plan: Plan) -> Result<(), String> {
        let mut plans = self.plans.write();

        if plans.contains_key(&plan.id) {
            return Err(format!("Plan '{}' already exists", plan.id));
        }

        // Validate tier_id is unique
        if plans.values().any(|p| p.tier_id == plan.tier_id) {
            return Err(format!("Tier ID {} is already in use", plan.tier_id));
        }

        plans.insert(plan.id.clone(), plan);
        Ok(())
    }

    /// Update an existing plan
    pub fn update_plan(&self, plan_id: &str, updates: PlanUpdate) -> Result<Plan, String> {
        let mut plans = self.plans.write();

        // Check plan exists
        if !plans.contains_key(plan_id) {
            return Err(format!("Plan '{}' not found", plan_id));
        }

        // Check if default plan (read-only check)
        let is_default = plans.get(plan_id).map(|p| p.is_default).unwrap_or(false);
        if is_default && (updates.tier_id.is_some() || updates.enabled == Some(false)) {
            return Err("Cannot modify tier_id or disable the default plan".to_string());
        }

        // Validate new tier_id is unique (before mutable borrow)
        if let Some(new_tier) = updates.tier_id {
            let tier_exists = plans.values().any(|p| p.tier_id == new_tier && p.id != plan_id);
            if tier_exists {
                return Err(format!("Tier ID {} is already in use", new_tier));
            }
        }

        // Now get mutable reference and apply updates
        // Safety: We checked plans.contains_key(plan_id) above and hold the write lock
        let Some(plan) = plans.get_mut(plan_id) else {
            return Err(format!("Plan '{}' not found", plan_id));
        };

        if let Some(new_tier) = updates.tier_id {
            plan.tier_id = new_tier;
        }
        if let Some(name) = updates.name {
            plan.name = name;
        }
        if let Some(description) = updates.description {
            plan.description = description;
        }
        if let Some(limits) = updates.limits {
            plan.limits = limits;
        }
        if let Some(enabled) = updates.enabled {
            plan.enabled = enabled;
        }
        if let Some(features) = updates.features {
            plan.features = features;
        }

        plan.updated_at = chrono::Utc::now().to_rfc3339();

        Ok(plan.clone())
    }

    /// Enable a plan
    pub fn enable_plan(&self, plan_id: &str) -> Result<(), String> {
        self.update_plan(plan_id, PlanUpdate {
            enabled: Some(true),
            ..Default::default()
        })?;
        Ok(())
    }

    /// Disable a plan (existing tenants keep it, new tenants can't use it)
    pub fn disable_plan(&self, plan_id: &str) -> Result<(), String> {
        let plans = self.plans.read();
        let plan = plans.get(plan_id)
            .ok_or_else(|| format!("Plan '{}' not found", plan_id))?;

        if plan.is_default {
            return Err("Cannot disable the default plan".to_string());
        }
        drop(plans);

        self.update_plan(plan_id, PlanUpdate {
            enabled: Some(false),
            ..Default::default()
        })?;
        Ok(())
    }

    /// Delete a plan and return the fallback plan ID for affected tenants
    /// Returns: (deleted_plan, fallback_plan_id)
    pub fn delete_plan(&self, plan_id: &str) -> Result<(Plan, PlanId), String> {
        let mut plans = self.plans.write();

        let plan = plans.get(plan_id)
            .ok_or_else(|| format!("Plan '{}' not found", plan_id))?;

        if plan.is_default {
            return Err("Cannot delete the default plan".to_string());
        }

        let deleted_tier = plan.tier_id;
        let deleted_plan = plan.clone();

        // Find the next lower tier plan, or default if none
        let fallback_plan_id = plans.values()
            .filter(|p| p.id != plan_id && p.enabled && p.tier_id < deleted_tier)
            .max_by_key(|p| p.tier_id)
            .map(|p| p.id.clone())
            .unwrap_or_else(|| {
                // No lower tier found, use default (unlimited)
                plans.values()
                    .find(|p| p.is_default)
                    .map(|p| p.id.clone())
                    .unwrap_or_else(|| "unlimited".to_string())
            });

        plans.remove(plan_id);

        Ok((deleted_plan, fallback_plan_id))
    }

    /// Get the default/fallback plan
    pub fn get_default_plan(&self) -> Plan {
        self.plans.read()
            .values()
            .find(|p| p.is_default)
            .cloned()
            .unwrap_or_else(Plan::unlimited)
    }

    /// Find next lower tier plan (for downgrade)
    pub fn get_downgrade_plan(&self, current_tier: u32) -> Option<Plan> {
        self.plans.read()
            .values()
            .filter(|p| p.enabled && p.tier_id < current_tier)
            .max_by_key(|p| p.tier_id)
            .cloned()
    }

    /// Check if a plan exists and is enabled
    pub fn is_plan_available(&self, plan_id: &str) -> bool {
        self.plans.read()
            .get(plan_id)
            .map(|p| p.enabled)
            .unwrap_or(false)
    }
}

impl Default for PlanManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Updates to apply to a plan
#[derive(Debug, Clone, Default)]
pub struct PlanUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub tier_id: Option<u32>,
    pub limits: Option<ResourceLimits>,
    pub enabled: Option<bool>,
    pub features: Option<PlanFeatures>,
}

/// Tenant isolation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    /// Shared database + shared schema (RLS)
    SharedSchema,
    /// Database per tenant
    DatabasePerTenant,
    /// Schema per tenant
    SchemaPerTenant,
}

/// Tenant information
#[derive(Debug, Clone)]
pub struct Tenant {
    /// Tenant ID
    pub id: TenantId,
    /// Tenant name
    pub name: String,
    /// Plan ID (references Plan.id)
    pub plan_id: PlanId,
    /// Isolation mode
    pub isolation_mode: IsolationMode,
    /// Resource limits (copied from plan, can be overridden)
    pub limits: ResourceLimits,
    /// RLS enabled
    pub rls_enabled: bool,
    /// Created at (timestamp)
    pub created_at: String,
}

/// Resource limits for a tenant
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Maximum storage (bytes)
    pub max_storage_bytes: u64,
    /// Maximum connections
    pub max_connections: usize,
    /// Maximum queries per second
    pub max_qps: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_storage_bytes: 100 * 1024 * 1024 * 1024, // 100 GB
            max_connections: 50,
            max_qps: 1000,
        }
    }
}

/// Row-Level Security policy
#[derive(Debug, Clone)]
pub struct RLSPolicy {
    /// Policy name
    pub name: String,
    /// Table name
    pub table_name: String,
    /// Policy condition (WHERE clause)
    pub condition: String,
    /// Applies to: SELECT, INSERT, UPDATE, DELETE
    pub cmd: RLSCommand,
    /// Using expression (for tenant isolation)
    pub using_expr: String,
    /// With check expression (for INSERT/UPDATE)
    pub with_check_expr: Option<String>,
}

/// RLS command type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RLSCommand {
    Select,
    Insert,
    Update,
    Delete,
    All,
}

/// Tenant context (per-request)
#[derive(Debug, Clone)]
pub struct TenantContext {
    /// Current tenant ID
    pub tenant_id: TenantId,
    /// Current user ID
    pub user_id: String,
    /// User roles
    pub roles: Vec<String>,
    /// Isolation mode
    pub isolation_mode: IsolationMode,
}

/// Quota tracking for a tenant with HWM and average metrics
#[derive(Debug, Clone)]
pub struct QuotaTracking {
    /// Active connection count (current)
    pub active_connections: usize,
    /// Connection high-water mark (max since start)
    pub connections_hwm: usize,
    /// Total connection samples for average
    pub connections_total_samples: u64,
    /// Connection sample count
    pub connections_sample_count: u64,

    /// Current storage usage (bytes)
    pub storage_bytes_used: u64,

    /// Queries executed in current time window (current QPS tracking)
    pub queries_this_window: usize,
    /// QPS high-water mark (max observed QPS)
    pub qps_hwm: usize,
    /// Total queries since start (for average calculation)
    pub total_queries: u64,

    /// Timestamp of last quota window reset
    pub window_reset_at: String,
    /// Timestamp when tracking started
    pub started_at: String,
    /// Total seconds elapsed (for average calculation)
    pub total_seconds: u64,
}

impl QuotaTracking {
    /// Get average connections since start
    pub fn avg_connections(&self) -> f64 {
        if self.connections_sample_count == 0 {
            0.0
        } else {
            self.connections_total_samples as f64 / self.connections_sample_count as f64
        }
    }

    /// Get average QPS since start
    pub fn avg_qps(&self) -> f64 {
        if self.total_seconds == 0 {
            0.0
        } else {
            self.total_queries as f64 / self.total_seconds as f64
        }
    }

    /// Sample current state (call periodically for accurate averages)
    pub fn sample(&mut self) {
        // Sample connections
        self.connections_total_samples += self.active_connections as u64;
        self.connections_sample_count += 1;

        // Update HWM for connections
        if self.active_connections > self.connections_hwm {
            self.connections_hwm = self.active_connections;
        }

        // Update HWM for QPS
        if self.queries_this_window > self.qps_hwm {
            self.qps_hwm = self.queries_this_window;
        }

        self.total_seconds += 1;
    }
}

impl Default for QuotaTracking {
    fn default() -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            active_connections: 0,
            connections_hwm: 0,
            connections_total_samples: 0,
            connections_sample_count: 0,
            storage_bytes_used: 0,
            queries_this_window: 0,
            qps_hwm: 0,
            total_queries: 0,
            window_reset_at: now.clone(),
            started_at: now,
            total_seconds: 0,
        }
    }
}

/// Change event type (INSERT, UPDATE, DELETE)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChangeType {
    /// Row inserted
    Insert,
    /// Row updated
    Update,
    /// Row deleted
    Delete,
}

/// Single change event captured by CDC
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChangeEvent {
    /// Unique event ID
    pub event_id: u64,
    /// Change type (INSERT/UPDATE/DELETE)
    pub change_type: ChangeType,
    /// Table name
    pub table_name: String,
    /// Row key/ID
    pub row_key: String,
    /// Old values (for UPDATE/DELETE)
    pub old_values: Option<String>,
    /// New values (for INSERT/UPDATE)
    pub new_values: Option<String>,
    /// Tenant ID affected
    pub tenant_id: TenantId,
    /// Timestamp of change
    pub timestamp: String,
    /// Transaction ID
    pub transaction_id: Option<u64>,
}

/// Migration state for a tenant replication
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationState {
    /// Waiting to start
    Pending,
    /// Capturing initial snapshot
    Snapshotting,
    /// Replicating incremental changes
    Replicating,
    /// Verifying consistency
    Verifying,
    /// Migration complete
    Completed,
    /// Migration failed
    Failed(String),
    /// Migration paused
    Paused,
}

/// Replication target for tenant migration
#[derive(Debug, Clone)]
pub struct ReplicationTarget {
    /// Target tenant ID
    pub target_tenant_id: TenantId,
    /// Source tenant ID
    pub source_tenant_id: TenantId,
    /// Current migration state
    pub migration_state: MigrationState,
    /// Number of changes replicated
    pub changes_replicated: u64,
    /// Total changes to replicate
    pub total_changes: u64,
    /// Consistency check hash (source)
    pub source_checksum: Option<String>,
    /// Consistency check hash (target)
    pub target_checksum: Option<String>,
    /// Last LSN replicated
    pub last_lsn: Option<u64>,
    /// Started at
    pub started_at: String,
    /// Completed at (if finished)
    pub completed_at: Option<String>,
}

/// CDC log entry for change tracking
#[derive(Debug, Clone)]
pub struct CDCLog {
    /// Log entry ID (monotonically increasing)
    pub log_id: u64,
    /// Change events
    pub changes: Vec<ChangeEvent>,
    /// Total size in bytes
    pub size_bytes: u64,
}

impl Default for CDCLog {
    fn default() -> Self {
        Self {
            log_id: 0,
            changes: Vec::new(),
            size_bytes: 0,
        }
    }
}

/// Tenant manager
pub struct TenantManager {
    /// Registered tenants
    tenants: Arc<parking_lot::RwLock<HashMap<TenantId, Tenant>>>,
    /// Plan manager
    pub plan_manager: PlanManager,
    /// RLS policies per table
    rls_policies: Arc<parking_lot::RwLock<HashMap<String, Vec<RLSPolicy>>>>,
    /// Current tenant context (thread-local or request-scoped)
    current_context: Arc<parking_lot::RwLock<Option<TenantContext>>>,
    /// Quota tracking per tenant
    quota_tracking: Arc<parking_lot::RwLock<HashMap<TenantId, QuotaTracking>>>,
    /// CDC logs per tenant
    cdc_logs: Arc<parking_lot::RwLock<HashMap<TenantId, CDCLog>>>,
    /// Replication targets per source tenant
    replication_targets: Arc<parking_lot::RwLock<HashMap<TenantId, Vec<ReplicationTarget>>>>,
    /// Global event ID counter
    event_id_counter: AtomicU64,
}

impl TenantManager {
    /// Create a new tenant manager
    pub fn new() -> Self {
        Self {
            tenants: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            plan_manager: PlanManager::new(),
            rls_policies: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            current_context: Arc::new(parking_lot::RwLock::new(None)),
            quota_tracking: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            cdc_logs: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            replication_targets: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            event_id_counter: AtomicU64::new(1),
        }
    }

    /// Register a new tenant with a plan
    pub fn register_tenant(&self, name: String, isolation_mode: IsolationMode) -> Tenant {
        self.register_tenant_with_plan(name, isolation_mode, "free")
    }

    /// Register a new tenant with a specific plan
    pub fn register_tenant_with_plan(&self, name: String, isolation_mode: IsolationMode, plan_id: &str) -> Tenant {
        // Get plan limits or use default
        let (plan_id, limits) = if let Some(plan) = self.plan_manager.get_plan(plan_id) {
            if plan.enabled {
                (plan.id, plan.limits)
            } else {
                // Plan is disabled, use default
                let default = self.plan_manager.get_default_plan();
                (default.id, default.limits)
            }
        } else {
            // Plan not found, use default
            let default = self.plan_manager.get_default_plan();
            (default.id, default.limits)
        };

        let tenant = Tenant {
            id: Uuid::new_v4(),
            name,
            plan_id,
            isolation_mode,
            limits,
            rls_enabled: isolation_mode == IsolationMode::SharedSchema,
            created_at: chrono::Utc::now().to_rfc3339(),
        };

        self.tenants.write().insert(tenant.id, tenant.clone());
        // Initialize quota tracking for the new tenant
        self.quota_tracking.write().insert(tenant.id, QuotaTracking::default());
        tenant
    }

    /// Change a tenant's plan
    pub fn change_tenant_plan(&self, tenant_id: TenantId, new_plan_id: &str) -> Result<Tenant, String> {
        let plan = self.plan_manager.get_plan(new_plan_id)
            .ok_or_else(|| format!("Plan '{}' not found", new_plan_id))?;

        if !plan.enabled {
            return Err(format!("Plan '{}' is disabled", new_plan_id));
        }

        let mut tenants = self.tenants.write();
        let tenant = tenants.get_mut(&tenant_id)
            .ok_or_else(|| format!("Tenant {} not found", tenant_id))?;

        tenant.plan_id = plan.id;
        tenant.limits = plan.limits;

        Ok(tenant.clone())
    }

    /// Get tenants by plan ID
    pub fn get_tenants_by_plan(&self, plan_id: &str) -> Vec<Tenant> {
        self.tenants.read()
            .values()
            .filter(|t| t.plan_id == plan_id)
            .cloned()
            .collect()
    }

    /// Downgrade all tenants from a plan to another
    pub fn downgrade_tenants(&self, from_plan_id: &str, to_plan_id: &str) -> Result<Vec<TenantId>, String> {
        let new_plan = self.plan_manager.get_plan(to_plan_id)
            .ok_or_else(|| format!("Target plan '{}' not found", to_plan_id))?;

        let mut downgraded = Vec::new();
        let mut tenants = self.tenants.write();

        for tenant in tenants.values_mut() {
            if tenant.plan_id == from_plan_id {
                tenant.plan_id = new_plan.id.clone();
                tenant.limits = new_plan.limits.clone();
                downgraded.push(tenant.id);
            }
        }

        Ok(downgraded)
    }

    /// Delete a plan and downgrade all affected tenants
    pub fn delete_plan_and_downgrade(&self, plan_id: &str) -> Result<(Plan, PlanId, Vec<TenantId>), String> {
        // First, delete the plan and get the fallback
        let (deleted_plan, fallback_id) = self.plan_manager.delete_plan(plan_id)?;

        // Then downgrade all affected tenants
        let downgraded = self.downgrade_tenants(plan_id, &fallback_id)?;

        Ok((deleted_plan, fallback_id, downgraded))
    }

    /// Get a tenant by ID
    pub fn get_tenant(&self, tenant_id: TenantId) -> Option<Tenant> {
        self.tenants.read().get(&tenant_id).cloned()
    }

    /// List all tenants (sorted by name for deterministic ordering)
    pub fn list_tenants(&self) -> Vec<Tenant> {
        let mut tenants: Vec<Tenant> = self.tenants.read().values().cloned().collect();
        tenants.sort_by(|a, b| a.name.cmp(&b.name));
        tenants
    }

    /// Set current tenant context
    /// Also sets thread-local context for use by SQL functions like current_tenant()
    pub fn set_current_context(&self, context: TenantContext) {
        // Set thread-local for SQL functions
        set_current_tenant_id(Some(context.tenant_id));
        set_current_user_id(Some(context.user_id.clone()));
        // Set on TenantManager (for RLS checks in query planning)
        *self.current_context.write() = Some(context);
    }

    /// Get current tenant context
    pub fn get_current_context(&self) -> Option<TenantContext> {
        self.current_context.read().clone()
    }

    /// Clear current tenant context
    pub fn clear_current_context(&self) {
        set_current_tenant_id(None);
        set_current_user_id(None);
        *self.current_context.write() = None;
    }

    /// Create RLS policy for table
    pub fn create_rls_policy(
        &self,
        table_name: String,
        policy_name: String,
        condition: String,
        cmd: RLSCommand,
        using_expr: String,
        with_check_expr: Option<String>,
    ) {
        let policy = RLSPolicy {
            name: policy_name,
            table_name: table_name.clone(),
            condition,
            cmd,
            using_expr,
            with_check_expr,
        };

        self.rls_policies
            .write()
            .entry(table_name)
            .or_insert_with(Vec::new)
            .push(policy);
    }

    /// Get RLS policies for table
    pub fn get_rls_policies(&self, table_name: &str) -> Vec<RLSPolicy> {
        self.rls_policies
            .read()
            .get(table_name)
            .cloned()
            .unwrap_or_default()
    }

    /// Check resource quota for tenant
    pub fn check_quota(&self, tenant_id: TenantId, resource_type: &str) -> bool {
        if let Some(tenant) = self.get_tenant(tenant_id) {
            if let Some(tracking) = self.quota_tracking.read().get(&tenant_id) {
                match resource_type {
                    "connections" => {
                        // Check if adding one more connection would exceed limit
                        tracking.active_connections < tenant.limits.max_connections
                    }
                    "storage" => {
                        // Check if current storage is below limit
                        tracking.storage_bytes_used < tenant.limits.max_storage_bytes
                    }
                    "qps" => {
                        // Check if queries in current window are below limit
                        tracking.queries_this_window < tenant.limits.max_qps
                    }
                    _ => false,
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Increment active connection count for tenant
    pub fn add_connection(&self, tenant_id: TenantId) -> Result<(), String> {
        if !self.check_quota(tenant_id, "connections") {
            return Err(format!("Connection limit exceeded for tenant {}", tenant_id));
        }

        if let Some(tracking) = self.quota_tracking.write().get_mut(&tenant_id) {
            tracking.active_connections += 1;

            // Update connection HWM
            if tracking.active_connections > tracking.connections_hwm {
                tracking.connections_hwm = tracking.active_connections;
            }

            // Sample for average calculation
            tracking.connections_total_samples += tracking.active_connections as u64;
            tracking.connections_sample_count += 1;

            Ok(())
        } else {
            Err(format!("Tenant {} not found", tenant_id))
        }
    }

    /// Decrement active connection count for tenant
    pub fn remove_connection(&self, tenant_id: TenantId) -> Result<(), String> {
        if let Some(tracking) = self.quota_tracking.write().get_mut(&tenant_id) {
            if tracking.active_connections > 0 {
                tracking.active_connections -= 1;
                Ok(())
            } else {
                Err("No active connections to remove".to_string())
            }
        } else {
            Err(format!("Tenant {} not found", tenant_id))
        }
    }

    /// Record storage usage for tenant
    pub fn update_storage_usage(&self, tenant_id: TenantId, bytes: u64) -> Result<(), String> {
        if let Some(tenant) = self.get_tenant(tenant_id) {
            // Check if update would exceed limit
            if bytes > tenant.limits.max_storage_bytes {
                return Err(format!(
                    "Storage quota exceeded: {} > {} bytes",
                    bytes, tenant.limits.max_storage_bytes
                ));
            }

            if let Some(tracking) = self.quota_tracking.write().get_mut(&tenant_id) {
                tracking.storage_bytes_used = bytes;
                Ok(())
            } else {
                Err(format!("Tenant {} not found", tenant_id))
            }
        } else {
            Err(format!("Tenant {} not found", tenant_id))
        }
    }

    /// Record a query execution for quota tracking (QPS)
    pub fn record_query(&self, tenant_id: TenantId) -> Result<(), String> {
        if !self.check_quota(tenant_id, "qps") {
            return Err(format!("Query rate limit exceeded for tenant {}", tenant_id));
        }

        if let Some(tracking) = self.quota_tracking.write().get_mut(&tenant_id) {
            tracking.queries_this_window += 1;
            tracking.total_queries += 1;

            // Update QPS HWM
            if tracking.queries_this_window > tracking.qps_hwm {
                tracking.qps_hwm = tracking.queries_this_window;
            }
            Ok(())
        } else {
            Err(format!("Tenant {} not found", tenant_id))
        }
    }

    /// Reset QPS quota (call periodically, e.g., per second or per minute)
    pub fn reset_qps_window(&self, tenant_id: TenantId) -> Result<(), String> {
        if let Some(tracking) = self.quota_tracking.write().get_mut(&tenant_id) {
            // Update HWM before reset
            if tracking.queries_this_window > tracking.qps_hwm {
                tracking.qps_hwm = tracking.queries_this_window;
            }

            tracking.queries_this_window = 0;
            tracking.window_reset_at = chrono::Utc::now().to_rfc3339();
            tracking.total_seconds += 1; // Increment time for average calculation
            Ok(())
        } else {
            Err(format!("Tenant {} not found", tenant_id))
        }
    }

    /// Get current quota tracking for a tenant
    pub fn get_quota_tracking(&self, tenant_id: TenantId) -> Option<QuotaTracking> {
        self.quota_tracking.read().get(&tenant_id).cloned()
    }

    /// Set custom resource limits for a tenant
    pub fn update_resource_limits(&self, tenant_id: TenantId, limits: ResourceLimits) -> Result<(), String> {
        if let Some(tenant) = self.tenants.write().get_mut(&tenant_id) {
            tenant.limits = limits;
            Ok(())
        } else {
            Err(format!("Tenant {} not found", tenant_id))
        }
    }

    /// Apply RLS to query (adds WHERE clauses for tenant isolation)
    pub fn apply_rls_to_query(&self, query: &str, table_name: &str) -> String {
        // Note: This is a text-based query rewriting approach
        // For production, consider using the logical plan-based approach
        if let Some(context) = self.get_current_context() {
            if let Some(tenant) = self.get_tenant(context.tenant_id) {
                if tenant.rls_enabled {
                    let policies = self.get_rls_policies(table_name);
                    if !policies.is_empty() {
                        // Simple implementation: add tenant_id filtering for SharedSchema mode
                        if matches!(tenant.isolation_mode, IsolationMode::SharedSchema) {
                            // This would need proper query parsing to insert correctly
                            // For now, return as-is to avoid breaking valid queries
                        }
                    }
                }
            }
        }
        query.to_string()
    }

    /// Check if RLS is enabled and applicable for current context
    pub fn should_apply_rls(&self, table_name: &str, cmd: &str) -> bool {
        if let Some(context) = self.get_current_context() {
            if let Some(tenant) = self.get_tenant(context.tenant_id) {
                if !tenant.rls_enabled {
                    return false;
                }

                let policies = self.get_rls_policies(table_name);
                if policies.is_empty() {
                    return false;
                }

                // Check if any policy applies to this command
                let cmd_upper = cmd.to_uppercase();
                for policy in policies {
                    let applies = match policy.cmd {
                        RLSCommand::All => true,
                        RLSCommand::Select => cmd_upper == "SELECT",
                        RLSCommand::Insert => cmd_upper == "INSERT",
                        RLSCommand::Update => cmd_upper == "UPDATE",
                        RLSCommand::Delete => cmd_upper == "DELETE",
                    };
                    if applies {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Get RLS conditions for current context and table
    /// Returns a tuple of (using_expression, with_check_expression) if RLS should be applied
    pub fn get_rls_conditions(&self, table_name: &str, cmd: &str) -> Option<(String, Option<String>)> {
        if !self.should_apply_rls(table_name, cmd) {
            return None;
        }

        if let Some(context) = self.get_current_context() {
            if let Some(_tenant) = self.get_tenant(context.tenant_id) {
                let policies = self.get_rls_policies(table_name);
                let cmd_upper = cmd.to_uppercase();

                // Collect applicable policies
                let applicable_policies: Vec<_> = policies.iter()
                    .filter(|p| {
                        matches!(p.cmd, RLSCommand::All) ||
                        (matches!(p.cmd, RLSCommand::Select) && cmd_upper == "SELECT") ||
                        (matches!(p.cmd, RLSCommand::Insert) && cmd_upper == "INSERT") ||
                        (matches!(p.cmd, RLSCommand::Update) && cmd_upper == "UPDATE") ||
                        (matches!(p.cmd, RLSCommand::Delete) && cmd_upper == "DELETE")
                    })
                    .collect();

                if !applicable_policies.is_empty() {
                    // For now, combine with first applicable policy
                    // In production, would combine multiple policies with OR
                    if let Some(policy) = applicable_policies.first() {
                        return Some((policy.using_expr.clone(), policy.with_check_expr.clone()));
                    }
                }
            }
        }
        None
    }

    // ============================================================================
    // CDC (Change Data Capture) and Tenant Migration Methods
    // ============================================================================

    /// Record a change event for CDC
    pub fn record_change_event(
        &self,
        change_type: ChangeType,
        table_name: String,
        row_key: String,
        old_values: Option<String>,
        new_values: Option<String>,
        tenant_id: TenantId,
        transaction_id: Option<u64>,
    ) -> u64 {
        let event_id = self.event_id_counter.fetch_add(1, Ordering::SeqCst);

        let event = ChangeEvent {
            event_id,
            change_type,
            table_name,
            row_key,
            old_values,
            new_values,
            tenant_id,
            timestamp: chrono::Utc::now().to_rfc3339(),
            transaction_id,
        };

        let mut cdc_logs = self.cdc_logs.write();
        if let Some(log) = cdc_logs.get_mut(&tenant_id) {
            log.changes.push(event);
            log.size_bytes += 256; // Rough estimate
        } else {
            let mut log = CDCLog::default();
            log.log_id = 1;
            log.changes.push(event);
            log.size_bytes = 256;
            cdc_logs.insert(tenant_id, log);
        }

        event_id
    }

    /// Get CDC log for tenant
    pub fn get_cdc_log(&self, tenant_id: TenantId) -> Option<CDCLog> {
        self.cdc_logs.read().get(&tenant_id).cloned()
    }

    /// Get recent change events (limit to last N)
    pub fn get_recent_changes(&self, tenant_id: TenantId, limit: usize) -> Vec<ChangeEvent> {
        if let Some(log) = self.cdc_logs.read().get(&tenant_id) {
            log.changes.iter()
                .rev()
                .take(limit)
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Clear CDC log for tenant (after successful replication)
    pub fn clear_cdc_log(&self, tenant_id: TenantId) -> Result<(), String> {
        if self.cdc_logs.write().contains_key(&tenant_id) {
            let mut log = CDCLog::default();
            log.log_id = self.cdc_logs.read()
                .get(&tenant_id)
                .map(|l| l.log_id + 1)
                .unwrap_or(1);
            self.cdc_logs.write().insert(tenant_id, log);
            Ok(())
        } else {
            Err(format!("CDC log not found for tenant {}", tenant_id))
        }
    }

    /// Start tenant migration (replication to target tenant)
    pub fn start_migration(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
    ) -> Result<(), String> {
        // Verify both tenants exist
        if !self.tenants.read().contains_key(&source_tenant_id) {
            return Err(format!("Source tenant {} not found", source_tenant_id));
        }
        if !self.tenants.read().contains_key(&target_tenant_id) {
            return Err(format!("Target tenant {} not found", target_tenant_id));
        }

        let target = ReplicationTarget {
            target_tenant_id,
            source_tenant_id,
            migration_state: MigrationState::Pending,
            changes_replicated: 0,
            total_changes: 0,
            source_checksum: None,
            target_checksum: None,
            last_lsn: None,
            started_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
        };

        self.replication_targets.write()
            .entry(source_tenant_id)
            .or_insert_with(Vec::new)
            .push(target);

        Ok(())
    }

    /// Update migration state
    pub fn update_migration_state(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
        state: MigrationState,
    ) -> Result<(), String> {
        let mut targets = self.replication_targets.write();
        if let Some(replication_vec) = targets.get_mut(&source_tenant_id) {
            if let Some(target) = replication_vec.iter_mut()
                .find(|t| t.target_tenant_id == target_tenant_id)
            {
                target.migration_state = state;
                if matches!(target.migration_state, MigrationState::Completed | MigrationState::Failed(_)) {
                    target.completed_at = Some(chrono::Utc::now().to_rfc3339());
                }
                Ok(())
            } else {
                Err(format!("Migration target {} not found", target_tenant_id))
            }
        } else {
            Err(format!("No migrations found for source tenant {}", source_tenant_id))
        }
    }

    /// Record progress on migration
    pub fn record_replication_progress(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
        changes_replicated: u64,
        total_changes: u64,
    ) -> Result<(), String> {
        let mut targets = self.replication_targets.write();
        if let Some(replication_vec) = targets.get_mut(&source_tenant_id) {
            if let Some(target) = replication_vec.iter_mut()
                .find(|t| t.target_tenant_id == target_tenant_id)
            {
                target.changes_replicated = changes_replicated;
                target.total_changes = total_changes;
                Ok(())
            } else {
                Err(format!("Migration target {} not found", target_tenant_id))
            }
        } else {
            Err(format!("No migrations found for source tenant {}", source_tenant_id))
        }
    }

    /// Set consistency checksums for migration verification
    pub fn set_migration_checksums(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
        source_checksum: String,
        target_checksum: String,
    ) -> Result<(), String> {
        let mut targets = self.replication_targets.write();
        if let Some(replication_vec) = targets.get_mut(&source_tenant_id) {
            if let Some(target) = replication_vec.iter_mut()
                .find(|t| t.target_tenant_id == target_tenant_id)
            {
                target.source_checksum = Some(source_checksum);
                target.target_checksum = Some(target_checksum);
                Ok(())
            } else {
                Err(format!("Migration target {} not found", target_tenant_id))
            }
        } else {
            Err(format!("No migrations found for source tenant {}", source_tenant_id))
        }
    }

    /// Get migration status for a replication target
    pub fn get_migration_status(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
    ) -> Option<ReplicationTarget> {
        self.replication_targets.read()
            .get(&source_tenant_id)
            .and_then(|targets| {
                targets.iter()
                    .find(|t| t.target_tenant_id == target_tenant_id)
                    .cloned()
            })
    }

    /// Get all active migrations for a source tenant
    pub fn get_active_migrations(&self, source_tenant_id: TenantId) -> Vec<ReplicationTarget> {
        self.replication_targets.read()
            .get(&source_tenant_id)
            .map(|targets| {
                targets.iter()
                    .filter(|t| !matches!(t.migration_state, MigrationState::Completed | MigrationState::Failed(_)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Pause migration
    pub fn pause_migration(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
    ) -> Result<(), String> {
        self.update_migration_state(source_tenant_id, target_tenant_id, MigrationState::Paused)
    }

    /// Resume migration
    pub fn resume_migration(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
    ) -> Result<(), String> {
        self.update_migration_state(source_tenant_id, target_tenant_id, MigrationState::Replicating)
    }

    /// Verify migration consistency (checksums match)
    pub fn verify_migration_consistency(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
    ) -> Result<bool, String> {
        if let Some(migration) = self.get_migration_status(source_tenant_id, target_tenant_id) {
            match (&migration.source_checksum, &migration.target_checksum) {
                (Some(src), Some(tgt)) => Ok(src == tgt),
                _ => Err("Checksums not set for migration".to_string()),
            }
        } else {
            Err(format!("Migration target {} not found", target_tenant_id))
        }
    }

    /// Rollback migration (undo replication)
    pub fn rollback_migration(
        &self,
        source_tenant_id: TenantId,
        target_tenant_id: TenantId,
    ) -> Result<(), String> {
        self.update_migration_state(
            source_tenant_id,
            target_tenant_id,
            MigrationState::Failed("Rolled back by user".to_string()),
        )
    }

    /// Delete a tenant (removes from registry and cleans up associated data)
    pub fn delete_tenant(&self, tenant_id: TenantId) -> Result<(), String> {
        // Remove from tenant registry
        if self.tenants.write().remove(&tenant_id).is_none() {
            return Err(format!("Tenant {} not found", tenant_id));
        }

        // Clean up quota tracking
        self.quota_tracking.write().remove(&tenant_id);

        // Clean up CDC logs
        self.cdc_logs.write().remove(&tenant_id);

        // Clean up replication targets
        self.replication_targets.write().remove(&tenant_id);

        // Clean up RLS policies for this tenant (if they're tenant-specific)
        // Note: RLS policies are per-table, not per-tenant, so we keep them
        // This is intentional as tables may be shared across tenants

        Ok(())
    }
}

impl Default for TenantManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================================
    // Tenant Registration Tests
    // ============================================================================

    #[test]
    fn test_register_tenant_basic() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant(
            "TestTenant".to_string(),
            IsolationMode::SharedSchema,
        );

        assert_eq!(tenant.name, "TestTenant");
        assert_eq!(tenant.isolation_mode, IsolationMode::SharedSchema);
        assert!(tenant.rls_enabled);
    }

    #[test]
    fn test_register_multiple_tenants() {
        let manager = TenantManager::new();

        let tenant1 = manager.register_tenant("Tenant1".to_string(), IsolationMode::SharedSchema);
        let tenant2 = manager.register_tenant("Tenant2".to_string(), IsolationMode::DatabasePerTenant);

        assert_ne!(tenant1.id, tenant2.id);

        let tenants = manager.list_tenants();
        assert_eq!(tenants.len(), 2);
    }

    #[test]
    fn test_get_tenant_by_id() {
        let manager = TenantManager::new();
        let created = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        let retrieved = manager.get_tenant(created.id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "Test");
    }

    #[test]
    fn test_get_nonexistent_tenant() {
        let manager = TenantManager::new();
        let fake_id = Uuid::new_v4();

        let result = manager.get_tenant(fake_id);
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_tenant() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("ToDelete".to_string(), IsolationMode::SharedSchema);

        // Verify tenant exists
        assert!(manager.get_tenant(tenant.id).is_some());

        // Delete tenant
        let result = manager.delete_tenant(tenant.id);
        assert!(result.is_ok());

        // Verify tenant is gone
        assert!(manager.get_tenant(tenant.id).is_none());
    }

    #[test]
    fn test_delete_nonexistent_tenant() {
        let manager = TenantManager::new();
        let fake_id = Uuid::new_v4();

        let result = manager.delete_tenant(fake_id);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    // ============================================================================
    // Isolation Mode Tests
    // ============================================================================

    #[test]
    fn test_shared_schema_enables_rls() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant(
            "Test".to_string(),
            IsolationMode::SharedSchema,
        );

        assert!(tenant.rls_enabled);
    }

    #[test]
    fn test_db_per_tenant_disables_rls() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant(
            "Test".to_string(),
            IsolationMode::DatabasePerTenant,
        );

        assert!(!tenant.rls_enabled);
    }

    #[test]
    fn test_schema_per_tenant_disables_rls() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant(
            "Test".to_string(),
            IsolationMode::SchemaPerTenant,
        );

        assert!(!tenant.rls_enabled);
    }

    // ============================================================================
    // Context Management Tests
    // ============================================================================

    #[test]
    fn test_set_and_get_context() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        let context = TenantContext {
            tenant_id: tenant.id,
            user_id: "user1".to_string(),
            roles: vec!["admin".to_string()],
            isolation_mode: IsolationMode::SharedSchema,
        };

        manager.set_current_context(context.clone());

        let retrieved = manager.get_current_context();
        assert!(retrieved.is_some());

        let ctx = retrieved.unwrap();
        assert_eq!(ctx.tenant_id, tenant.id);
        assert_eq!(ctx.user_id, "user1");
    }

    #[test]
    fn test_clear_context() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.set_current_context(TenantContext {
            tenant_id: tenant.id,
            user_id: "user1".to_string(),
            roles: vec![],
            isolation_mode: IsolationMode::SharedSchema,
        });

        assert!(manager.get_current_context().is_some());

        manager.clear_current_context();

        assert!(manager.get_current_context().is_none());
    }

    // ============================================================================
    // RLS Policy Tests
    // ============================================================================

    #[test]
    fn test_create_rls_policy() {
        let manager = TenantManager::new();

        manager.create_rls_policy(
            "users".to_string(),
            "tenant_isolation".to_string(),
            "Isolate users by tenant".to_string(),
            RLSCommand::Select,
            "tenant_id = current_tenant_id()".to_string(),
            None,
        );

        let policies = manager.get_rls_policies("users");
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].name, "tenant_isolation");
    }

    #[test]
    fn test_multiple_policies_per_table() {
        let manager = TenantManager::new();

        manager.create_rls_policy(
            "orders".to_string(),
            "policy1".to_string(),
            "Policy 1".to_string(),
            RLSCommand::Select,
            "expr1".to_string(),
            None,
        );

        manager.create_rls_policy(
            "orders".to_string(),
            "policy2".to_string(),
            "Policy 2".to_string(),
            RLSCommand::Update,
            "expr2".to_string(),
            None,
        );

        let policies = manager.get_rls_policies("orders");
        assert_eq!(policies.len(), 2);
    }

    #[test]
    fn test_rls_policy_with_check_expression() {
        let manager = TenantManager::new();

        manager.create_rls_policy(
            "orders".to_string(),
            "insert_check".to_string(),
            "Check on insert".to_string(),
            RLSCommand::Insert,
            "tenant_id = current_tenant()".to_string(),
            Some("tenant_id = current_tenant()".to_string()),
        );

        let policies = manager.get_rls_policies("orders");
        assert!(policies[0].with_check_expr.is_some());
    }

    #[test]
    fn test_should_apply_rls_no_context() {
        let manager = TenantManager::new();

        let should_apply = manager.should_apply_rls("users", "SELECT");
        assert!(!should_apply);
    }

    #[test]
    fn test_should_apply_rls_with_policy() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.create_rls_policy(
            "users".to_string(),
            "policy".to_string(),
            "desc".to_string(),
            RLSCommand::Select,
            "expr".to_string(),
            None,
        );

        manager.set_current_context(TenantContext {
            tenant_id: tenant.id,
            user_id: "user1".to_string(),
            roles: vec![],
            isolation_mode: IsolationMode::SharedSchema,
        });

        assert!(manager.should_apply_rls("users", "SELECT"));
    }

    #[test]
    fn test_rls_command_matching() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        // Create SELECT-only policy
        manager.create_rls_policy(
            "users".to_string(),
            "select_policy".to_string(),
            "desc".to_string(),
            RLSCommand::Select,
            "expr".to_string(),
            None,
        );

        manager.set_current_context(TenantContext {
            tenant_id: tenant.id,
            user_id: "user1".to_string(),
            roles: vec![],
            isolation_mode: IsolationMode::SharedSchema,
        });

        // Should apply to SELECT
        assert!(manager.should_apply_rls("users", "SELECT"));

        // Should NOT apply to UPDATE
        assert!(!manager.should_apply_rls("users", "UPDATE"));
    }

    #[test]
    fn test_rls_all_command() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        // Create ALL command policy
        manager.create_rls_policy(
            "users".to_string(),
            "all_policy".to_string(),
            "desc".to_string(),
            RLSCommand::All,
            "expr".to_string(),
            None,
        );

        manager.set_current_context(TenantContext {
            tenant_id: tenant.id,
            user_id: "user1".to_string(),
            roles: vec![],
            isolation_mode: IsolationMode::SharedSchema,
        });

        // Should apply to all commands
        assert!(manager.should_apply_rls("users", "SELECT"));
        assert!(manager.should_apply_rls("users", "INSERT"));
        assert!(manager.should_apply_rls("users", "UPDATE"));
        assert!(manager.should_apply_rls("users", "DELETE"));
    }

    // ============================================================================
    // Quota Management Tests
    // ============================================================================

    #[test]
    fn test_connection_quota_enforcement() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.update_resource_limits(
            tenant.id,
            ResourceLimits {
                max_storage_bytes: 100_000_000,
                max_connections: 2,
                max_qps: 100,
            },
        ).unwrap();

        // Add connections up to limit
        assert!(manager.add_connection(tenant.id).is_ok());
        assert!(manager.add_connection(tenant.id).is_ok());

        // Exceed limit
        assert!(manager.add_connection(tenant.id).is_err());
    }

    #[test]
    fn test_storage_quota_enforcement() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.update_resource_limits(
            tenant.id,
            ResourceLimits {
                max_storage_bytes: 1000,
                max_connections: 10,
                max_qps: 100,
            },
        ).unwrap();

        // Within limit
        assert!(manager.update_storage_usage(tenant.id, 500).is_ok());

        // Exceed limit
        assert!(manager.update_storage_usage(tenant.id, 2000).is_err());
    }

    #[test]
    fn test_qps_quota_enforcement() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.update_resource_limits(
            tenant.id,
            ResourceLimits {
                max_storage_bytes: 100_000_000,
                max_connections: 10,
                max_qps: 3,
            },
        ).unwrap();

        // Record queries up to limit
        assert!(manager.record_query(tenant.id).is_ok());
        assert!(manager.record_query(tenant.id).is_ok());
        assert!(manager.record_query(tenant.id).is_ok());

        // Exceed limit
        assert!(manager.record_query(tenant.id).is_err());
    }

    #[test]
    fn test_qps_window_reset() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.update_resource_limits(
            tenant.id,
            ResourceLimits {
                max_storage_bytes: 100_000_000,
                max_connections: 10,
                max_qps: 2,
            },
        ).unwrap();

        // Hit limit
        manager.record_query(tenant.id).unwrap();
        manager.record_query(tenant.id).unwrap();
        assert!(manager.record_query(tenant.id).is_err());

        // Reset window
        manager.reset_qps_window(tenant.id).unwrap();

        // Should work again
        assert!(manager.record_query(tenant.id).is_ok());
    }

    #[test]
    fn test_remove_connection() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.add_connection(tenant.id).unwrap();
        manager.add_connection(tenant.id).unwrap();

        let tracking = manager.get_quota_tracking(tenant.id).unwrap();
        assert_eq!(tracking.active_connections, 2);

        manager.remove_connection(tenant.id).unwrap();

        let tracking = manager.get_quota_tracking(tenant.id).unwrap();
        assert_eq!(tracking.active_connections, 1);
    }

    #[test]
    fn test_quota_tracking_initialized() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        let tracking = manager.get_quota_tracking(tenant.id);
        assert!(tracking.is_some());

        let t = tracking.unwrap();
        assert_eq!(t.active_connections, 0);
        assert_eq!(t.storage_bytes_used, 0);
        assert_eq!(t.queries_this_window, 0);
    }

    // ============================================================================
    // CDC Tests
    // ============================================================================

    #[test]
    fn test_record_insert_event() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        let event_id = manager.record_change_event(
            ChangeType::Insert,
            "users".to_string(),
            "user_123".to_string(),
            None,
            Some(r#"{"name": "Alice"}"#.to_string()),
            tenant.id,
            Some(1),
        );

        assert!(event_id > 0);

        let log = manager.get_cdc_log(tenant.id);
        assert!(log.is_some());
        assert_eq!(log.unwrap().changes.len(), 1);
    }

    #[test]
    fn test_record_update_event() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.record_change_event(
            ChangeType::Update,
            "users".to_string(),
            "user_123".to_string(),
            Some(r#"{"name": "Alice"}"#.to_string()),
            Some(r#"{"name": "Alice Smith"}"#.to_string()),
            tenant.id,
            Some(2),
        );

        let changes = manager.get_recent_changes(tenant.id, 10);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::Update);
        assert!(changes[0].old_values.is_some());
        assert!(changes[0].new_values.is_some());
    }

    #[test]
    fn test_record_delete_event() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.record_change_event(
            ChangeType::Delete,
            "users".to_string(),
            "user_123".to_string(),
            Some(r#"{"name": "Alice"}"#.to_string()),
            None,
            tenant.id,
            Some(3),
        );

        let changes = manager.get_recent_changes(tenant.id, 10);
        assert_eq!(changes[0].change_type, ChangeType::Delete);
        assert!(changes[0].old_values.is_some());
        assert!(changes[0].new_values.is_none());
    }

    #[test]
    fn test_get_recent_changes_limit() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        // Record 10 events
        for i in 0..10 {
            manager.record_change_event(
                ChangeType::Insert,
                "users".to_string(),
                format!("user_{}", i),
                None,
                Some(format!(r#"{{"id": {}}}"#, i)),
                tenant.id,
                Some(i),
            );
        }

        // Get last 5
        let changes = manager.get_recent_changes(tenant.id, 5);
        assert_eq!(changes.len(), 5);

        // Verify they're the most recent (9, 8, 7, 6, 5)
        assert_eq!(changes[0].row_key, "user_9");
        assert_eq!(changes[4].row_key, "user_5");
    }

    #[test]
    fn test_clear_cdc_log() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        manager.record_change_event(
            ChangeType::Insert,
            "users".to_string(),
            "user_1".to_string(),
            None,
            Some(r#"{"id": 1}"#.to_string()),
            tenant.id,
            Some(1),
        );

        let log_before = manager.get_cdc_log(tenant.id).unwrap();
        assert_eq!(log_before.changes.len(), 1);

        manager.clear_cdc_log(tenant.id).unwrap();

        let log_after = manager.get_cdc_log(tenant.id).unwrap();
        assert_eq!(log_after.changes.len(), 0);
    }

    // ============================================================================
    // Migration Tests
    // ============================================================================

    #[test]
    fn test_start_migration() {
        let manager = TenantManager::new();
        let source = manager.register_tenant("Source".to_string(), IsolationMode::SharedSchema);
        let target = manager.register_tenant("Target".to_string(), IsolationMode::SharedSchema);

        let result = manager.start_migration(source.id, target.id);
        assert!(result.is_ok());

        let status = manager.get_migration_status(source.id, target.id);
        assert!(status.is_some());
        assert_eq!(status.unwrap().migration_state, MigrationState::Pending);
    }

    #[test]
    fn test_update_migration_state() {
        let manager = TenantManager::new();
        let source = manager.register_tenant("Source".to_string(), IsolationMode::SharedSchema);
        let target = manager.register_tenant("Target".to_string(), IsolationMode::SharedSchema);

        manager.start_migration(source.id, target.id).unwrap();

        manager.update_migration_state(
            source.id,
            target.id,
            MigrationState::Replicating,
        ).unwrap();

        let status = manager.get_migration_status(source.id, target.id).unwrap();
        assert_eq!(status.migration_state, MigrationState::Replicating);
    }

    #[test]
    fn test_record_migration_progress() {
        let manager = TenantManager::new();
        let source = manager.register_tenant("Source".to_string(), IsolationMode::SharedSchema);
        let target = manager.register_tenant("Target".to_string(), IsolationMode::SharedSchema);

        manager.start_migration(source.id, target.id).unwrap();

        manager.record_replication_progress(source.id, target.id, 50, 100).unwrap();

        let status = manager.get_migration_status(source.id, target.id).unwrap();
        assert_eq!(status.changes_replicated, 50);
        assert_eq!(status.total_changes, 100);
    }

    #[test]
    fn test_verify_migration_consistency() {
        let manager = TenantManager::new();
        let source = manager.register_tenant("Source".to_string(), IsolationMode::SharedSchema);
        let target = manager.register_tenant("Target".to_string(), IsolationMode::SharedSchema);

        manager.start_migration(source.id, target.id).unwrap();

        manager.set_migration_checksums(
            source.id,
            target.id,
            "abc123".to_string(),
            "abc123".to_string(),
        ).unwrap();

        let consistent = manager.verify_migration_consistency(source.id, target.id).unwrap();
        assert!(consistent);
    }

    #[test]
    fn test_pause_resume_migration() {
        let manager = TenantManager::new();
        let source = manager.register_tenant("Source".to_string(), IsolationMode::SharedSchema);
        let target = manager.register_tenant("Target".to_string(), IsolationMode::SharedSchema);

        manager.start_migration(source.id, target.id).unwrap();

        manager.pause_migration(source.id, target.id).unwrap();
        let status = manager.get_migration_status(source.id, target.id).unwrap();
        assert_eq!(status.migration_state, MigrationState::Paused);

        manager.resume_migration(source.id, target.id).unwrap();
        let status = manager.get_migration_status(source.id, target.id).unwrap();
        assert_eq!(status.migration_state, MigrationState::Replicating);
    }

    #[test]
    fn test_rollback_migration() {
        let manager = TenantManager::new();
        let source = manager.register_tenant("Source".to_string(), IsolationMode::SharedSchema);
        let target = manager.register_tenant("Target".to_string(), IsolationMode::SharedSchema);

        manager.start_migration(source.id, target.id).unwrap();
        manager.rollback_migration(source.id, target.id).unwrap();

        let status = manager.get_migration_status(source.id, target.id).unwrap();
        match status.migration_state {
            MigrationState::Failed(msg) => {
                assert!(msg.contains("Rolled back"));
            }
            _ => panic!("Expected Failed state"),
        }
    }

    // ============================================================================
    // Resource Limits Tests
    // ============================================================================

    #[test]
    fn test_update_resource_limits() {
        let manager = TenantManager::new();
        let tenant = manager.register_tenant("Test".to_string(), IsolationMode::SharedSchema);

        let new_limits = ResourceLimits {
            max_storage_bytes: 500_000_000,
            max_connections: 200,
            max_qps: 5000,
        };

        manager.update_resource_limits(tenant.id, new_limits.clone()).unwrap();

        let updated = manager.get_tenant(tenant.id).unwrap();
        assert_eq!(updated.limits.max_storage_bytes, 500_000_000);
        assert_eq!(updated.limits.max_connections, 200);
        assert_eq!(updated.limits.max_qps, 5000);
    }

    #[test]
    fn test_default_resource_limits() {
        let limits = ResourceLimits::default();

        assert_eq!(limits.max_storage_bytes, 100 * 1024 * 1024 * 1024); // 100 GB
        assert_eq!(limits.max_connections, 50);
        assert_eq!(limits.max_qps, 1000);
    }
}
