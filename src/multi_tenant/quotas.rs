//! Usage Analytics and Quota Management
//!
//! Track resource usage per tenant and enforce quotas.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{TenantError, TenantQuotas};

/// Usage metrics for a tenant
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TenantUsage {
    /// Storage used in bytes
    pub storage_bytes: u64,
    /// Number of tables
    pub table_count: u32,
    /// Total row count across all tables
    pub total_rows: u64,
    /// Number of vector stores
    pub vector_store_count: u32,
    /// Total vectors stored
    pub vector_count: u64,
    /// Number of branches
    pub branch_count: u32,
    /// Queries in current period
    pub queries_in_period: u64,
    /// Active connections
    pub active_connections: u32,
    /// API requests in current period
    pub api_requests_in_period: u64,
    /// Bytes transferred out
    pub egress_bytes: u64,
    /// Bytes transferred in
    pub ingress_bytes: u64,
    /// Compute time in milliseconds
    pub compute_ms: u64,
    /// Last updated timestamp
    pub last_updated: u64,
}

/// Rate limiter for QPM limits
pub struct RateLimiter {
    windows: Arc<RwLock<HashMap<String, RateLimitWindow>>>,
    window_size: Duration,
}

struct RateLimitWindow {
    count: u64,
    window_start: Instant,
}

impl RateLimiter {
    pub fn new(window_size: Duration) -> Self {
        Self {
            windows: Arc::new(RwLock::new(HashMap::new())),
            window_size,
        }
    }

    /// Check if request is allowed
    pub fn check(&self, tenant_id: &str, limit: u64) -> bool {
        let mut windows = self.windows.write();
        let now = Instant::now();

        let window = windows.entry(tenant_id.to_string()).or_insert(RateLimitWindow {
            count: 0,
            window_start: now,
        });

        // Reset window if expired
        if now.duration_since(window.window_start) >= self.window_size {
            window.count = 0;
            window.window_start = now;
        }

        // Check limit
        if window.count >= limit {
            return false;
        }

        window.count += 1;
        true
    }

    /// Get current count for tenant
    pub fn current_count(&self, tenant_id: &str) -> u64 {
        self.windows.read()
            .get(tenant_id)
            .map(|w| w.count)
            .unwrap_or(0)
    }

    /// Get remaining quota
    pub fn remaining(&self, tenant_id: &str, limit: u64) -> u64 {
        limit.saturating_sub(self.current_count(tenant_id))
    }

    /// Time until window resets
    pub fn reset_in(&self, tenant_id: &str) -> Duration {
        let windows = self.windows.read();
        if let Some(window) = windows.get(tenant_id) {
            let elapsed = Instant::now().duration_since(window.window_start);
            if elapsed < self.window_size {
                return self.window_size.saturating_sub(elapsed);
            }
        }
        Duration::from_secs(0)
    }
}

/// Quota enforcement service
pub struct QuotaService {
    usage: Arc<RwLock<HashMap<String, TenantUsage>>>,
    rate_limiter: RateLimiter,
}

impl QuotaService {
    pub fn new() -> Self {
        Self {
            usage: Arc::new(RwLock::new(HashMap::new())),
            rate_limiter: RateLimiter::new(Duration::from_secs(60)), // 1 minute window
        }
    }

    /// Get current usage for tenant
    pub fn get_usage(&self, tenant_id: &str) -> TenantUsage {
        self.usage.read()
            .get(tenant_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Update usage metrics
    pub fn update_usage(&self, tenant_id: &str, update: UsageUpdate) {
        let mut usage_map = self.usage.write();
        let usage = usage_map.entry(tenant_id.to_string()).or_default();

        match update {
            UsageUpdate::Storage(bytes) => usage.storage_bytes = bytes,
            UsageUpdate::AddStorage(bytes) => usage.storage_bytes += bytes,
            UsageUpdate::Tables(count) => usage.table_count = count,
            UsageUpdate::AddTable => usage.table_count += 1,
            UsageUpdate::RemoveTable => usage.table_count = usage.table_count.saturating_sub(1),
            UsageUpdate::Rows(count) => usage.total_rows = count,
            UsageUpdate::AddRows(count) => usage.total_rows += count,
            UsageUpdate::RemoveRows(count) => usage.total_rows = usage.total_rows.saturating_sub(count),
            UsageUpdate::VectorStores(count) => usage.vector_store_count = count,
            UsageUpdate::Vectors(count) => usage.vector_count = count,
            UsageUpdate::AddVectors(count) => usage.vector_count += count,
            UsageUpdate::Branches(count) => usage.branch_count = count,
            UsageUpdate::Query => usage.queries_in_period += 1,
            UsageUpdate::Connection(delta) => {
                if delta > 0 {
                    usage.active_connections += delta as u32;
                } else {
                    usage.active_connections = usage.active_connections.saturating_sub((-delta) as u32);
                }
            }
            UsageUpdate::ApiRequest => usage.api_requests_in_period += 1,
            UsageUpdate::Egress(bytes) => usage.egress_bytes += bytes,
            UsageUpdate::Ingress(bytes) => usage.ingress_bytes += bytes,
            UsageUpdate::Compute(ms) => usage.compute_ms += ms,
        }

        usage.last_updated = current_timestamp();
    }

    /// Check quota before operation
    pub fn check_quota(&self, tenant_id: &str, quotas: &TenantQuotas, resource: QuotaResource) -> Result<(), TenantError> {
        let usage = self.get_usage(tenant_id);

        let (current, limit, resource_name) = match resource {
            QuotaResource::Storage(additional) => (
                usage.storage_bytes + additional,
                quotas.max_storage_bytes,
                "storage"
            ),
            QuotaResource::Tables => (
                usage.table_count as u64 + 1,
                quotas.max_tables as u64,
                "tables"
            ),
            QuotaResource::RowsPerTable(table_rows) => (
                table_rows,
                quotas.max_rows_per_table,
                "rows_per_table"
            ),
            QuotaResource::VectorStores => (
                usage.vector_store_count as u64 + 1,
                quotas.max_vector_stores as u64,
                "vector_stores"
            ),
            QuotaResource::Vectors(additional) => (
                usage.vector_count + additional,
                quotas.max_vectors,
                "vectors"
            ),
            QuotaResource::Branches => (
                usage.branch_count as u64 + 1,
                quotas.max_branches as u64,
                "branches"
            ),
            QuotaResource::Connections => (
                usage.active_connections as u64 + 1,
                quotas.max_connections as u64,
                "connections"
            ),
            QuotaResource::Query => {
                // Use rate limiter for QPM
                if !self.rate_limiter.check(tenant_id, quotas.qpm_limit as u64) {
                    return Err(TenantError::QuotaExceeded {
                        resource: "queries_per_minute".to_string(),
                        limit: quotas.qpm_limit as u64,
                        current: self.rate_limiter.current_count(tenant_id),
                    });
                }
                return Ok(());
            }
        };

        if current > limit {
            return Err(TenantError::QuotaExceeded {
                resource: resource_name.to_string(),
                limit,
                current,
            });
        }

        Ok(())
    }

    /// Get quota status for tenant
    pub fn get_quota_status(&self, tenant_id: &str, quotas: &TenantQuotas) -> QuotaStatus {
        let usage = self.get_usage(tenant_id);

        QuotaStatus {
            storage: ResourceStatus {
                used: usage.storage_bytes,
                limit: quotas.max_storage_bytes,
                percentage: percentage(usage.storage_bytes, quotas.max_storage_bytes),
            },
            tables: ResourceStatus {
                used: usage.table_count as u64,
                limit: quotas.max_tables as u64,
                percentage: percentage(usage.table_count as u64, quotas.max_tables as u64),
            },
            vectors: ResourceStatus {
                used: usage.vector_count,
                limit: quotas.max_vectors,
                percentage: percentage(usage.vector_count, quotas.max_vectors),
            },
            branches: ResourceStatus {
                used: usage.branch_count as u64,
                limit: quotas.max_branches as u64,
                percentage: percentage(usage.branch_count as u64, quotas.max_branches as u64),
            },
            qpm: RateStatus {
                current: self.rate_limiter.current_count(tenant_id),
                limit: quotas.qpm_limit as u64,
                remaining: self.rate_limiter.remaining(tenant_id, quotas.qpm_limit as u64),
                reset_in_seconds: self.rate_limiter.reset_in(tenant_id).as_secs(),
            },
            connections: ResourceStatus {
                used: usage.active_connections as u64,
                limit: quotas.max_connections as u64,
                percentage: percentage(usage.active_connections as u64, quotas.max_connections as u64),
            },
        }
    }

    /// Reset period counters
    pub fn reset_period_counters(&self, tenant_id: &str) {
        if let Some(usage) = self.usage.write().get_mut(tenant_id) {
            usage.queries_in_period = 0;
            usage.api_requests_in_period = 0;
        }
    }
}

impl Default for QuotaService {
    fn default() -> Self {
        Self::new()
    }
}

/// Usage update types
pub enum UsageUpdate {
    Storage(u64),
    AddStorage(u64),
    Tables(u32),
    AddTable,
    RemoveTable,
    Rows(u64),
    AddRows(u64),
    RemoveRows(u64),
    VectorStores(u32),
    Vectors(u64),
    AddVectors(u64),
    Branches(u32),
    Query,
    Connection(i32),
    ApiRequest,
    Egress(u64),
    Ingress(u64),
    Compute(u64),
}

/// Resource types for quota checking
pub enum QuotaResource {
    Storage(u64),
    Tables,
    RowsPerTable(u64),
    VectorStores,
    Vectors(u64),
    Branches,
    Connections,
    Query,
}

/// Quota status response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaStatus {
    pub storage: ResourceStatus,
    pub tables: ResourceStatus,
    pub vectors: ResourceStatus,
    pub branches: ResourceStatus,
    pub qpm: RateStatus,
    pub connections: ResourceStatus,
}

/// Resource usage status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceStatus {
    pub used: u64,
    pub limit: u64,
    pub percentage: f64,
}

/// Rate limit status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateStatus {
    pub current: u64,
    pub limit: u64,
    pub remaining: u64,
    pub reset_in_seconds: u64,
}

/// Usage analytics aggregation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageAnalytics {
    pub period_start: u64,
    pub period_end: u64,
    pub total_queries: u64,
    pub total_api_requests: u64,
    pub total_egress_bytes: u64,
    pub total_ingress_bytes: u64,
    pub total_compute_ms: u64,
    pub peak_storage_bytes: u64,
    pub peak_connections: u32,
    pub average_query_time_ms: f64,
    pub percentile_95_query_time_ms: f64,
}

// Helper functions

fn percentage(used: u64, limit: u64) -> f64 {
    if limit == 0 || limit == u64::MAX {
        0.0
    } else {
        (used as f64 / limit as f64) * 100.0
    }
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
    use crate::multi_tenant::TenantQuotas;

    #[test]
    fn test_rate_limiter() {
        let limiter = RateLimiter::new(Duration::from_secs(60));

        // Should allow up to limit
        for _ in 0..10 {
            assert!(limiter.check("test", 10));
        }

        // Should deny over limit
        assert!(!limiter.check("test", 10));
    }

    #[test]
    fn test_quota_check() {
        let service = QuotaService::new();
        let quotas = TenantQuotas {
            max_tables: 5,
            ..Default::default()
        };

        // Initially should pass
        assert!(service.check_quota("test", &quotas, QuotaResource::Tables).is_ok());

        // Update usage
        service.update_usage("test", UsageUpdate::Tables(5));

        // Now should fail
        assert!(service.check_quota("test", &quotas, QuotaResource::Tables).is_err());
    }
}
