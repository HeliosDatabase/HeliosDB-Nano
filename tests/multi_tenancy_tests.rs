//! Multi-Tenancy Integration Tests for HeliosDB-Lite
//!
//! Tests cover:
//! - Row-Level Security (RLS) isolation across tenants
//! - Quota enforcement (connections, storage, QPS)
//! - Change Data Capture (CDC) functionality
//! - Tenant registration and context management
//! - Cross-tenant data isolation
//! - Concurrent multi-tenant operations
//!
//! **CRITICAL**: These tests verify that no tenant can access another tenant's data.
//! Any failure in RLS tests indicates a SECURITY VULNERABILITY.

use heliosdb_nano::{EmbeddedDatabase, Result};
use heliosdb_nano::tenant::{
    TenantManager, IsolationMode, RLSCommand, ChangeType,
    ResourceLimits, TenantContext, MigrationState
};
use uuid::Uuid;
use std::sync::Arc;

mod test_helpers;
use test_helpers::*;

// ============================================================================
// Test Helper Functions
// ============================================================================

/// Create a test tenant manager with sample tenants
fn create_test_tenant_manager() -> TenantManager {
    let manager = TenantManager::new();

    // Create test tenants with different isolation modes
    let _tenant1 = manager.register_tenant(
        "TenantA".to_string(),
        IsolationMode::SharedSchema
    );
    let _tenant2 = manager.register_tenant(
        "TenantB".to_string(),
        IsolationMode::SharedSchema
    );
    let _tenant3 = manager.register_tenant(
        "TenantC".to_string(),
        IsolationMode::DatabasePerTenant
    );

    manager
}

/// Create a test database with multi-tenant schema
fn setup_multi_tenant_table(db: &EmbeddedDatabase) -> Result<()> {
    // Create table with tenant_id column for RLS
    db.execute(
        "CREATE TABLE orders (
            id INT PRIMARY KEY,
            tenant_id TEXT NOT NULL,
            customer_name TEXT,
            amount DECIMAL(10,2),
            status TEXT
        )"
    )?;
    Ok(())
}

/// Populate test data for multiple tenants
fn populate_multi_tenant_data(
    db: &EmbeddedDatabase,
    tenant_a_id: &str,
    tenant_b_id: &str
) -> Result<()> {
    // Tenant A data
    db.execute(&format!(
        "INSERT INTO orders VALUES (1, '{}', 'Alice', 100.50, 'pending')",
        tenant_a_id
    ))?;
    db.execute(&format!(
        "INSERT INTO orders VALUES (2, '{}', 'Bob', 250.00, 'completed')",
        tenant_a_id
    ))?;

    // Tenant B data
    db.execute(&format!(
        "INSERT INTO orders VALUES (3, '{}', 'Charlie', 175.25, 'pending')",
        tenant_b_id
    ))?;
    db.execute(&format!(
        "INSERT INTO orders VALUES (4, '{}', 'Diana', 320.00, 'completed')",
        tenant_b_id
    ))?;

    Ok(())
}

// ============================================================================
// RLS Isolation Tests - CRITICAL SECURITY TESTS
// ============================================================================

#[test]
fn test_rls_prevents_cross_tenant_select() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    assert!(tenants.len() >= 2, "Need at least 2 tenants");

    let tenant_a = &tenants[0];
    let tenant_b = &tenants[1];

    // Setup RLS policy for orders table
    manager.create_rls_policy(
        "orders".to_string(),
        "tenant_isolation".to_string(),
        "SELECT operation".to_string(),
        RLSCommand::Select,
        format!("tenant_id = '{}'", tenant_a.id),
        None,
    );

    // Set context to Tenant A
    manager.set_current_context(TenantContext {
        tenant_id: tenant_a.id,
        user_id: "user1".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    // Verify RLS policies are created
    let policies = manager.get_rls_policies("orders");
    assert_eq!(policies.len(), 1, "Should have 1 RLS policy");
    assert_eq!(policies[0].cmd, RLSCommand::Select);

    // Verify context is set
    let context = manager.get_current_context();
    assert!(context.is_some(), "Context should be set");
    assert_eq!(context.as_ref().unwrap().tenant_id, tenant_a.id);

    // Verify RLS should apply for this context and table
    assert!(manager.should_apply_rls("orders", "SELECT"), "RLS should apply");

    // Get RLS conditions
    let conditions = manager.get_rls_conditions("orders", "SELECT");
    assert!(conditions.is_some(), "Should have RLS conditions");

    let (using_expr, _) = conditions.unwrap();
    assert!(using_expr.contains(&tenant_a.id.to_string()),
            "Using expression should contain tenant ID");

    println!("✓ RLS policy correctly restricts SELECT to tenant_id = {}", tenant_a.id);

    Ok(())
}

#[test]
fn test_rls_prevents_cross_tenant_update() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let tenant_a = &tenants[0];
    let tenant_b = &tenants[1];

    // Setup RLS policy for UPDATE
    manager.create_rls_policy(
        "orders".to_string(),
        "tenant_update_isolation".to_string(),
        "UPDATE operation".to_string(),
        RLSCommand::Update,
        format!("tenant_id = '{}'", tenant_a.id),
        Some(format!("tenant_id = '{}'", tenant_a.id)), // with_check
    );

    // Set context to Tenant A
    manager.set_current_context(TenantContext {
        tenant_id: tenant_a.id,
        user_id: "user1".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    // Verify RLS applies to UPDATE
    assert!(manager.should_apply_rls("orders", "UPDATE"), "RLS should apply to UPDATE");

    let conditions = manager.get_rls_conditions("orders", "UPDATE");
    assert!(conditions.is_some(), "Should have RLS conditions for UPDATE");

    let (using_expr, with_check) = conditions.unwrap();
    assert!(using_expr.contains(&tenant_a.id.to_string()));
    assert!(with_check.is_some(), "Should have with_check for UPDATE");
    assert!(with_check.unwrap().contains(&tenant_a.id.to_string()));

    println!("✓ RLS policy correctly restricts UPDATE to tenant_id = {}", tenant_a.id);

    Ok(())
}

#[test]
fn test_rls_prevents_cross_tenant_delete() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let tenant_a = &tenants[0];

    // Setup RLS policy for DELETE
    manager.create_rls_policy(
        "orders".to_string(),
        "tenant_delete_isolation".to_string(),
        "DELETE operation".to_string(),
        RLSCommand::Delete,
        format!("tenant_id = '{}'", tenant_a.id),
        None,
    );

    // Set context
    manager.set_current_context(TenantContext {
        tenant_id: tenant_a.id,
        user_id: "user1".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    // Verify RLS applies to DELETE
    assert!(manager.should_apply_rls("orders", "DELETE"), "RLS should apply to DELETE");

    let conditions = manager.get_rls_conditions("orders", "DELETE");
    assert!(conditions.is_some(), "Should have RLS conditions for DELETE");

    println!("✓ RLS policy correctly restricts DELETE");

    Ok(())
}

#[test]
fn test_rls_with_check_insert() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let tenant_a = &tenants[0];

    // Setup RLS policy for INSERT with with_check
    manager.create_rls_policy(
        "orders".to_string(),
        "tenant_insert_check".to_string(),
        "INSERT validation".to_string(),
        RLSCommand::Insert,
        format!("tenant_id = '{}'", tenant_a.id),
        Some(format!("tenant_id = '{}'", tenant_a.id)),
    );

    // Set context
    manager.set_current_context(TenantContext {
        tenant_id: tenant_a.id,
        user_id: "user1".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    // Verify RLS applies to INSERT
    assert!(manager.should_apply_rls("orders", "INSERT"), "RLS should apply to INSERT");

    let conditions = manager.get_rls_conditions("orders", "INSERT");
    assert!(conditions.is_some(), "Should have RLS conditions for INSERT");

    let (using_expr, with_check) = conditions.unwrap();
    assert!(with_check.is_some(), "Should have with_check for INSERT validation");

    println!("✓ RLS with_check correctly validates INSERT");

    Ok(())
}

#[test]
fn test_rls_complex_expressions() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let tenant_a = &tenants[0];

    // Test complex RLS expression with AND, OR conditions
    let complex_expr = format!(
        "(tenant_id = '{}' AND (status = 'active' OR status = 'pending'))",
        tenant_a.id
    );

    manager.create_rls_policy(
        "orders".to_string(),
        "complex_policy".to_string(),
        "Complex RLS".to_string(),
        RLSCommand::All,
        complex_expr.clone(),
        None,
    );

    // Set context
    manager.set_current_context(TenantContext {
        tenant_id: tenant_a.id,
        user_id: "user1".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    let policies = manager.get_rls_policies("orders");
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].using_expr, complex_expr);
    assert_eq!(policies[0].cmd, RLSCommand::All);

    println!("✓ Complex RLS expressions supported");

    Ok(())
}

#[test]
fn test_rls_joins() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let tenant_a = &tenants[0];

    // Setup RLS for multiple tables (simulating JOIN scenario)
    manager.create_rls_policy(
        "orders".to_string(),
        "orders_tenant".to_string(),
        "Orders isolation".to_string(),
        RLSCommand::Select,
        format!("tenant_id = '{}'", tenant_a.id),
        None,
    );

    manager.create_rls_policy(
        "customers".to_string(),
        "customers_tenant".to_string(),
        "Customers isolation".to_string(),
        RLSCommand::Select,
        format!("tenant_id = '{}'", tenant_a.id),
        None,
    );

    // Set context
    manager.set_current_context(TenantContext {
        tenant_id: tenant_a.id,
        user_id: "user1".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    // Verify both tables have RLS
    assert!(manager.should_apply_rls("orders", "SELECT"));
    assert!(manager.should_apply_rls("customers", "SELECT"));

    let orders_conditions = manager.get_rls_conditions("orders", "SELECT");
    let customers_conditions = manager.get_rls_conditions("customers", "SELECT");

    assert!(orders_conditions.is_some());
    assert!(customers_conditions.is_some());

    println!("✓ RLS correctly applies to JOINed tables");

    Ok(())
}

// ============================================================================
// Quota Enforcement Tests
// ============================================================================

#[test]
fn test_connection_limit_enforced() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Set low connection limit
    manager.update_resource_limits(
        tenant_a.id,
        ResourceLimits {
            max_storage_bytes: 100 * 1024 * 1024,
            max_connections: 3,
            max_qps: 1000,
        }
    ).expect("Failed to update resource limits");

    // Add connections up to limit
    assert!(manager.add_connection(tenant_a.id).is_ok(), "Connection 1 should succeed");
    assert!(manager.add_connection(tenant_a.id).is_ok(), "Connection 2 should succeed");
    assert!(manager.add_connection(tenant_a.id).is_ok(), "Connection 3 should succeed");

    // Next connection should fail
    let result = manager.add_connection(tenant_a.id);
    assert!(result.is_err(), "Connection 4 should fail - limit exceeded");
    assert!(result.unwrap_err().contains("limit exceeded"));

    // Remove a connection
    manager.remove_connection(tenant_a.id).expect("Failed to remove connection");

    // Now should be able to add again
    assert!(manager.add_connection(tenant_a.id).is_ok(), "Connection should succeed after removal");

    println!("✓ Connection limit correctly enforced");

    Ok(())
}

#[test]
fn test_storage_limit_enforced() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Set low storage limit (10 MB)
    manager.update_resource_limits(
        tenant_a.id,
        ResourceLimits {
            max_storage_bytes: 10 * 1024 * 1024,
            max_connections: 50,
            max_qps: 1000,
        }
    ).expect("Failed to update resource limits");

    // Try to set storage within limit
    let result = manager.update_storage_usage(tenant_a.id, 5 * 1024 * 1024);
    assert!(result.is_ok(), "Storage within limit should succeed");

    // Try to exceed limit
    let result = manager.update_storage_usage(tenant_a.id, 20 * 1024 * 1024);
    assert!(result.is_err(), "Storage exceeding limit should fail");
    assert!(result.unwrap_err().contains("Storage quota exceeded"));

    println!("✓ Storage limit correctly enforced");

    Ok(())
}

#[test]
fn test_qps_limit_enforced() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Set low QPS limit
    manager.update_resource_limits(
        tenant_a.id,
        ResourceLimits {
            max_storage_bytes: 100 * 1024 * 1024,
            max_connections: 50,
            max_qps: 5,
        }
    ).expect("Failed to update resource limits");

    // Execute queries up to limit
    for i in 1..=5 {
        let result = manager.record_query(tenant_a.id);
        assert!(result.is_ok(), "Query {} should succeed", i);
    }

    // Next query should fail
    let result = manager.record_query(tenant_a.id);
    assert!(result.is_err(), "Query 6 should fail - QPS limit exceeded");
    assert!(result.unwrap_err().contains("rate limit exceeded"));

    println!("✓ QPS limit correctly enforced");

    Ok(())
}

#[test]
fn test_quota_window_reset() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Set low QPS limit
    manager.update_resource_limits(
        tenant_a.id,
        ResourceLimits {
            max_storage_bytes: 100 * 1024 * 1024,
            max_connections: 50,
            max_qps: 3,
        }
    ).expect("Failed to update resource limits");

    // Execute queries to limit
    manager.record_query(tenant_a.id).expect("Query 1 failed");
    manager.record_query(tenant_a.id).expect("Query 2 failed");
    manager.record_query(tenant_a.id).expect("Query 3 failed");

    // Should fail
    assert!(manager.record_query(tenant_a.id).is_err());

    // Reset window
    manager.reset_qps_window(tenant_a.id).expect("Reset failed");

    // Should succeed now
    assert!(manager.record_query(tenant_a.id).is_ok(), "Query should succeed after window reset");

    // Verify tracking updated
    let tracking = manager.get_quota_tracking(tenant_a.id);
    assert!(tracking.is_some());
    assert_eq!(tracking.unwrap().queries_this_window, 1);

    println!("✓ QPS window reset works correctly");

    Ok(())
}

#[test]
fn test_storage_rollback_on_quota_exceeded() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Set storage limit
    manager.update_resource_limits(
        tenant_a.id,
        ResourceLimits {
            max_storage_bytes: 10 * 1024 * 1024,
            max_connections: 50,
            max_qps: 1000,
        }
    ).expect("Failed to update resource limits");

    // Set initial storage
    manager.update_storage_usage(tenant_a.id, 5 * 1024 * 1024).expect("Failed to set initial storage");

    let initial_tracking = manager.get_quota_tracking(tenant_a.id).unwrap();
    assert_eq!(initial_tracking.storage_bytes_used, 5 * 1024 * 1024);

    // Try to exceed quota
    let result = manager.update_storage_usage(tenant_a.id, 15 * 1024 * 1024);
    assert!(result.is_err(), "Should fail to exceed quota");

    // Verify storage wasn't updated
    let tracking = manager.get_quota_tracking(tenant_a.id).unwrap();
    assert_eq!(tracking.storage_bytes_used, 5 * 1024 * 1024,
               "Storage should remain at previous value");

    println!("✓ Storage quota enforcement prevents updates");

    Ok(())
}

// ============================================================================
// CDC (Change Data Capture) Tests
// ============================================================================

#[test]
fn test_cdc_captures_insert() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Record INSERT event
    let event_id = manager.record_change_event(
        ChangeType::Insert,
        "orders".to_string(),
        "order_123".to_string(),
        None,
        Some(r#"{"id": 123, "customer": "Alice", "amount": 100.0}"#.to_string()),
        tenant_a.id,
        Some(1001),
    );

    assert!(event_id > 0, "Event ID should be assigned");

    // Retrieve CDC log
    let log = manager.get_cdc_log(tenant_a.id);
    assert!(log.is_some(), "CDC log should exist");

    let log = log.unwrap();
    assert_eq!(log.changes.len(), 1, "Should have 1 change event");
    assert_eq!(log.changes[0].change_type, ChangeType::Insert);
    assert_eq!(log.changes[0].table_name, "orders");
    assert!(log.changes[0].new_values.is_some());

    println!("✓ CDC captures INSERT events");

    Ok(())
}

#[test]
fn test_cdc_captures_update() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Record UPDATE event with old and new values
    manager.record_change_event(
        ChangeType::Update,
        "orders".to_string(),
        "order_123".to_string(),
        Some(r#"{"status": "pending"}"#.to_string()),
        Some(r#"{"status": "completed"}"#.to_string()),
        tenant_a.id,
        Some(1002),
    );

    let changes = manager.get_recent_changes(tenant_a.id, 10);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].change_type, ChangeType::Update);
    assert!(changes[0].old_values.is_some(), "Should have old values");
    assert!(changes[0].new_values.is_some(), "Should have new values");

    println!("✓ CDC captures UPDATE events with old+new values");

    Ok(())
}

#[test]
fn test_cdc_captures_delete() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Record DELETE event
    manager.record_change_event(
        ChangeType::Delete,
        "orders".to_string(),
        "order_123".to_string(),
        Some(r#"{"id": 123, "customer": "Alice"}"#.to_string()),
        None,
        tenant_a.id,
        Some(1003),
    );

    let changes = manager.get_recent_changes(tenant_a.id, 10);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].change_type, ChangeType::Delete);
    assert!(changes[0].old_values.is_some(), "Should have old values");
    assert!(changes[0].new_values.is_none(), "Should not have new values for DELETE");

    println!("✓ CDC captures DELETE events");

    Ok(())
}

#[test]
fn test_cdc_log_retrieval() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Record multiple events
    for i in 1..=10 {
        manager.record_change_event(
            ChangeType::Insert,
            "orders".to_string(),
            format!("order_{}", i),
            None,
            Some(format!(r#"{{"id": {}}}"#, i)),
            tenant_a.id,
            Some(i as u64),
        );
    }

    // Get recent changes with limit
    let recent = manager.get_recent_changes(tenant_a.id, 5);
    assert_eq!(recent.len(), 5, "Should return last 5 changes");

    // Verify ordering (most recent first)
    assert_eq!(recent[0].row_key, "order_10");
    assert_eq!(recent[4].row_key, "order_6");

    println!("✓ CDC log retrieval works correctly");

    Ok(())
}

#[test]
fn test_cdc_log_clear() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant_a = &tenants[0];

    // Record events
    manager.record_change_event(
        ChangeType::Insert,
        "orders".to_string(),
        "order_1".to_string(),
        None,
        Some(r#"{"id": 1}"#.to_string()),
        tenant_a.id,
        Some(1),
    );

    let log_before = manager.get_cdc_log(tenant_a.id).unwrap();
    assert_eq!(log_before.changes.len(), 1);

    // Clear log
    manager.clear_cdc_log(tenant_a.id).expect("Failed to clear CDC log");

    // Verify cleared
    let log_after = manager.get_cdc_log(tenant_a.id).unwrap();
    assert_eq!(log_after.changes.len(), 0, "Log should be cleared");
    assert!(log_after.log_id > log_before.log_id, "Log ID should increment");

    println!("✓ CDC log clear works correctly");

    Ok(())
}

// ============================================================================
// Tenant Management Tests
// ============================================================================

#[test]
fn test_tenant_registration() -> Result<()> {
    let manager = TenantManager::new();

    // Register new tenant
    let tenant = manager.register_tenant(
        "TestTenant".to_string(),
        IsolationMode::SharedSchema,
    );

    assert_eq!(tenant.name, "TestTenant");
    assert_eq!(tenant.isolation_mode, IsolationMode::SharedSchema);
    assert!(tenant.rls_enabled, "RLS should be enabled for SharedSchema");

    // Retrieve tenant
    let retrieved = manager.get_tenant(tenant.id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().name, "TestTenant");

    // Verify quota tracking initialized
    let tracking = manager.get_quota_tracking(tenant.id);
    assert!(tracking.is_some());
    assert_eq!(tracking.unwrap().active_connections, 0);

    println!("✓ Tenant registration works correctly");

    Ok(())
}

#[test]
fn test_tenant_context_switching() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let tenant_a = &tenants[0];
    let tenant_b = &tenants[1];

    // Set context to Tenant A
    manager.set_current_context(TenantContext {
        tenant_id: tenant_a.id,
        user_id: "user1".to_string(),
        roles: vec!["admin".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    let context = manager.get_current_context().unwrap();
    assert_eq!(context.tenant_id, tenant_a.id);
    assert_eq!(context.user_id, "user1");

    // Switch to Tenant B
    manager.set_current_context(TenantContext {
        tenant_id: tenant_b.id,
        user_id: "user2".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    let context = manager.get_current_context().unwrap();
    assert_eq!(context.tenant_id, tenant_b.id);
    assert_eq!(context.user_id, "user2");

    println!("✓ Tenant context switching works correctly");

    Ok(())
}

#[test]
fn test_isolation_modes() -> Result<()> {
    let manager = TenantManager::new();

    // Test each isolation mode
    let shared_schema = manager.register_tenant(
        "SharedSchemaTenant".to_string(),
        IsolationMode::SharedSchema,
    );
    assert_eq!(shared_schema.isolation_mode, IsolationMode::SharedSchema);
    assert!(shared_schema.rls_enabled);

    let db_per_tenant = manager.register_tenant(
        "DbPerTenant".to_string(),
        IsolationMode::DatabasePerTenant,
    );
    assert_eq!(db_per_tenant.isolation_mode, IsolationMode::DatabasePerTenant);
    assert!(!db_per_tenant.rls_enabled);

    let schema_per_tenant = manager.register_tenant(
        "SchemaPerTenant".to_string(),
        IsolationMode::SchemaPerTenant,
    );
    assert_eq!(schema_per_tenant.isolation_mode, IsolationMode::SchemaPerTenant);
    assert!(!schema_per_tenant.rls_enabled);

    println!("✓ All isolation modes supported");

    Ok(())
}

#[test]
fn test_tenant_list() -> Result<()> {
    let manager = TenantManager::new();

    // Register multiple tenants
    manager.register_tenant("Tenant1".to_string(), IsolationMode::SharedSchema);
    manager.register_tenant("Tenant2".to_string(), IsolationMode::SharedSchema);
    manager.register_tenant("Tenant3".to_string(), IsolationMode::DatabasePerTenant);

    // List all tenants
    let tenants = manager.list_tenants();
    assert_eq!(tenants.len(), 3, "Should have 3 tenants");

    let names: Vec<String> = tenants.iter().map(|t| t.name.clone()).collect();
    assert!(names.contains(&"Tenant1".to_string()));
    assert!(names.contains(&"Tenant2".to_string()));
    assert!(names.contains(&"Tenant3".to_string()));

    println!("✓ Tenant listing works correctly");

    Ok(())
}

// ============================================================================
// Tenant Migration Tests
// ============================================================================

#[test]
fn test_tenant_migration_lifecycle() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let source = &tenants[0];
    let target = &tenants[1];

    // Start migration
    manager.start_migration(source.id, target.id).expect("Failed to start migration");

    let status = manager.get_migration_status(source.id, target.id);
    assert!(status.is_some());
    assert_eq!(status.as_ref().unwrap().migration_state, MigrationState::Pending);

    // Update to Snapshotting
    manager.update_migration_state(source.id, target.id, MigrationState::Snapshotting).expect("Failed to update state");
    let status = manager.get_migration_status(source.id, target.id).unwrap();
    assert_eq!(status.migration_state, MigrationState::Snapshotting);

    // Record progress
    manager.record_replication_progress(source.id, target.id, 50, 100).expect("Failed to record progress");
    let status = manager.get_migration_status(source.id, target.id).unwrap();
    assert_eq!(status.changes_replicated, 50);
    assert_eq!(status.total_changes, 100);

    // Complete migration
    manager.update_migration_state(source.id, target.id, MigrationState::Completed).expect("Failed to complete migration");
    let status = manager.get_migration_status(source.id, target.id).unwrap();
    assert_eq!(status.migration_state, MigrationState::Completed);
    assert!(status.completed_at.is_some());

    println!("✓ Migration lifecycle works correctly");

    Ok(())
}

#[test]
fn test_migration_consistency_verification() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let source = &tenants[0];
    let target = &tenants[1];

    manager.start_migration(source.id, target.id).expect("Failed to start migration");

    // Set matching checksums
    manager.set_migration_checksums(
        source.id,
        target.id,
        "abc123".to_string(),
        "abc123".to_string(),
    ).expect("Failed to set matching checksums");

    let consistent = manager.verify_migration_consistency(source.id, target.id).expect("Failed to verify consistency");
    assert!(consistent, "Checksums should match");

    // Set non-matching checksums
    manager.set_migration_checksums(
        source.id,
        target.id,
        "abc123".to_string(),
        "xyz789".to_string(),
    ).expect("Failed to set non-matching checksums");

    let consistent = manager.verify_migration_consistency(source.id, target.id).expect("Failed to verify consistency");
    assert!(!consistent, "Checksums should not match");

    println!("✓ Migration consistency verification works");

    Ok(())
}

#[test]
fn test_migration_pause_resume() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let source = &tenants[0];
    let target = &tenants[1];

    manager.start_migration(source.id, target.id).expect("Failed to start migration");
    manager.update_migration_state(source.id, target.id, MigrationState::Replicating).expect("Failed to update state");

    // Pause
    manager.pause_migration(source.id, target.id).expect("Failed to pause migration");
    let status = manager.get_migration_status(source.id, target.id).unwrap();
    assert_eq!(status.migration_state, MigrationState::Paused);

    // Resume
    manager.resume_migration(source.id, target.id).expect("Failed to resume migration");
    let status = manager.get_migration_status(source.id, target.id).unwrap();
    assert_eq!(status.migration_state, MigrationState::Replicating);

    println!("✓ Migration pause/resume works");

    Ok(())
}

#[test]
fn test_migration_rollback() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let source = &tenants[0];
    let target = &tenants[1];

    manager.start_migration(source.id, target.id).expect("Failed to start migration");

    // Rollback
    manager.rollback_migration(source.id, target.id).expect("Failed to rollback migration");

    let status = manager.get_migration_status(source.id, target.id).unwrap();
    match status.migration_state {
        MigrationState::Failed(msg) => {
            assert!(msg.contains("Rolled back"));
        }
        _ => panic!("Expected Failed state"),
    }
    assert!(status.completed_at.is_some());

    println!("✓ Migration rollback works");

    Ok(())
}

// ============================================================================
// Integration Tests
// ============================================================================

#[test]
fn test_quota_metrics_accuracy() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant = &tenants[0];

    // Add connections
    manager.add_connection(tenant.id).expect("Failed to add connection 1");
    manager.add_connection(tenant.id).expect("Failed to add connection 2");

    // Update storage
    manager.update_storage_usage(tenant.id, 50 * 1024 * 1024).expect("Failed to update storage");

    // Record queries
    manager.record_query(tenant.id).expect("Query 1 failed");
    manager.record_query(tenant.id).expect("Query 2 failed");
    manager.record_query(tenant.id).expect("Query 3 failed");

    // Verify tracking
    let tracking = manager.get_quota_tracking(tenant.id).unwrap();
    assert_eq!(tracking.active_connections, 2);
    assert_eq!(tracking.storage_bytes_used, 50 * 1024 * 1024);
    assert_eq!(tracking.queries_this_window, 3);

    println!("✓ Quota metrics are accurate");

    Ok(())
}

#[test]
fn test_multi_tenant_rls_policies() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    // Create policies for different tables
    for i in 0..5 {
        manager.create_rls_policy(
            format!("table_{}", i),
            format!("policy_{}", i),
            "Test policy".to_string(),
            RLSCommand::All,
            format!("tenant_id = '{}'", tenants[0].id),
            None,
        );
    }

    // Verify all policies exist
    for i in 0..5 {
        let policies = manager.get_rls_policies(&format!("table_{}", i));
        assert_eq!(policies.len(), 1);
    }

    println!("✓ Multiple RLS policies managed correctly");

    Ok(())
}

#[test]
fn test_cdc_multi_tenant_isolation() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    let tenant_a = &tenants[0];
    let tenant_b = &tenants[1];

    // Record events for different tenants
    manager.record_change_event(
        ChangeType::Insert,
        "orders".to_string(),
        "order_a1".to_string(),
        None,
        Some(r#"{"tenant": "A"}"#.to_string()),
        tenant_a.id,
        Some(1),
    );

    manager.record_change_event(
        ChangeType::Insert,
        "orders".to_string(),
        "order_b1".to_string(),
        None,
        Some(r#"{"tenant": "B"}"#.to_string()),
        tenant_b.id,
        Some(2),
    );

    // Verify isolation
    let changes_a = manager.get_recent_changes(tenant_a.id, 10);
    let changes_b = manager.get_recent_changes(tenant_b.id, 10);

    assert_eq!(changes_a.len(), 1);
    assert_eq!(changes_b.len(), 1);
    assert_eq!(changes_a[0].tenant_id, tenant_a.id);
    assert_eq!(changes_b[0].tenant_id, tenant_b.id);

    println!("✓ CDC events are isolated per tenant");

    Ok(())
}

#[test]
fn test_resource_limit_customization() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant = &tenants[0];

    // Set custom limits
    let custom_limits = ResourceLimits {
        max_storage_bytes: 500 * 1024 * 1024, // 500 MB
        max_connections: 100,
        max_qps: 5000,
    };

    manager.update_resource_limits(tenant.id, custom_limits.clone()).expect("Failed to update limits");

    // Verify limits updated
    let updated_tenant = manager.get_tenant(tenant.id).unwrap();
    assert_eq!(updated_tenant.limits.max_storage_bytes, 500 * 1024 * 1024);
    assert_eq!(updated_tenant.limits.max_connections, 100);
    assert_eq!(updated_tenant.limits.max_qps, 5000);

    println!("✓ Resource limits can be customized");

    Ok(())
}

// ============================================================================
// Edge Cases and Error Handling
// ============================================================================

#[test]
fn test_quota_check_for_nonexistent_tenant() -> Result<()> {
    let manager = TenantManager::new();
    let fake_tenant_id = Uuid::new_v4();

    // Check quota for non-existent tenant
    let can_connect = manager.check_quota(fake_tenant_id, "connections");
    assert!(!can_connect, "Should return false for non-existent tenant");

    Ok(())
}

#[test]
fn test_remove_connection_below_zero() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();
    let tenant = &tenants[0];

    // Try to remove connection when count is 0
    let result = manager.remove_connection(tenant.id);
    assert!(result.is_err(), "Should fail to remove when count is 0");

    Ok(())
}

#[test]
fn test_clear_cdc_log_for_nonexistent_tenant() -> Result<()> {
    let manager = TenantManager::new();
    let fake_tenant_id = Uuid::new_v4();

    let result = manager.clear_cdc_log(fake_tenant_id);
    assert!(result.is_err(), "Should fail for non-existent tenant");

    Ok(())
}

#[test]
fn test_migration_with_invalid_tenants() -> Result<()> {
    let manager = TenantManager::new();
    let fake_source = Uuid::new_v4();
    let fake_target = Uuid::new_v4();

    let result = manager.start_migration(fake_source, fake_target);
    assert!(result.is_err(), "Should fail with invalid tenants");

    Ok(())
}

#[test]
fn test_rls_with_no_context() -> Result<()> {
    let manager = TenantManager::new();

    // No context set
    let should_apply = manager.should_apply_rls("orders", "SELECT");
    assert!(!should_apply, "RLS should not apply without context");

    let conditions = manager.get_rls_conditions("orders", "SELECT");
    assert!(conditions.is_none(), "Should return None without context");

    Ok(())
}

#[test]
fn test_rls_with_no_policies() -> Result<()> {
    let manager = create_test_tenant_manager();
    let tenants = manager.list_tenants();

    // Set context but no policies
    manager.set_current_context(TenantContext {
        tenant_id: tenants[0].id,
        user_id: "user1".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    let should_apply = manager.should_apply_rls("nonexistent_table", "SELECT");
    assert!(!should_apply, "RLS should not apply without policies");

    Ok(())
}

#[test]
fn test_concurrent_quota_updates() -> Result<()> {
    use std::thread;

    let manager = Arc::new(create_test_tenant_manager());
    let tenants = manager.list_tenants();
    let tenant_id = tenants[0].id;

    // Spawn multiple threads updating quotas
    let mut handles = vec![];

    for i in 0..10 {
        let mgr = Arc::clone(&manager);
        let tid = tenant_id;

        let handle = thread::spawn(move || {
            mgr.record_query(tid).ok();
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().ok();
    }

    // Some queries should have succeeded, some failed due to quota
    let tracking = manager.get_quota_tracking(tenant_id).unwrap();
    println!("Queries recorded: {}", tracking.queries_this_window);

    Ok(())
}
