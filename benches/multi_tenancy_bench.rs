//! Multi-Tenancy Performance Benchmarks
//!
//! Measures the performance overhead of:
//! - RLS (Row-Level Security) query filtering
//! - Quota checking and enforcement
//! - CDC (Change Data Capture) event logging
//! - Tenant context switching
//! - Multi-tenant concurrent operations
//!
//! Run with: cargo bench --bench multi_tenancy_bench

use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput
};
use heliosdb_nano::tenant::{
    TenantManager, IsolationMode, RLSCommand, ChangeType,
    ResourceLimits, TenantContext
};
use uuid::Uuid;
use std::sync::Arc;

// ============================================================================
// Setup Functions
// ============================================================================

fn create_tenant_manager_with_n_tenants(n: usize) -> TenantManager {
    let manager = TenantManager::new();

    for i in 0..n {
        manager.register_tenant(
            format!("Tenant{}", i),
            IsolationMode::SharedSchema,
        );
    }

    manager
}

fn create_tenant_manager_with_policies(n_tables: usize) -> TenantManager {
    let manager = TenantManager::new();
    let tenant = manager.register_tenant(
        "TestTenant".to_string(),
        IsolationMode::SharedSchema,
    );

    // Create RLS policies for N tables
    for i in 0..n_tables {
        manager.create_rls_policy(
            format!("table_{}", i),
            format!("policy_{}", i),
            "Tenant isolation".to_string(),
            RLSCommand::All,
            format!("tenant_id = '{}'", tenant.id),
            None,
        );
    }

    // Set current context
    manager.set_current_context(TenantContext {
        tenant_id: tenant.id,
        user_id: "benchmark_user".to_string(),
        roles: vec!["user".to_string()],
        isolation_mode: IsolationMode::SharedSchema,
    });

    manager
}

// ============================================================================
// RLS Performance Benchmarks
// ============================================================================

fn benchmark_rls_check_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("rls_check");

    // Baseline: No RLS
    group.bench_function("baseline_no_rls", |b| {
        let manager = TenantManager::new();

        b.iter(|| {
            let should_apply = manager.should_apply_rls("orders", "SELECT");
            black_box(should_apply);
        });
    });

    // With RLS policy
    group.bench_function("with_rls_policy", |b| {
        let manager = create_tenant_manager_with_policies(1);

        b.iter(|| {
            let should_apply = manager.should_apply_rls("table_0", "SELECT");
            black_box(should_apply);
        });
    });

    // Get RLS conditions
    group.bench_function("get_rls_conditions", |b| {
        let manager = create_tenant_manager_with_policies(1);

        b.iter(|| {
            let conditions = manager.get_rls_conditions("table_0", "SELECT");
            black_box(conditions);
        });
    });

    group.finish();
}

fn benchmark_rls_scaling_with_policies(c: &mut Criterion) {
    let mut group = c.benchmark_group("rls_scaling");

    // Test RLS check performance with increasing number of policies
    for n_tables in [1, 10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*n_tables as u64));

        group.bench_with_input(
            BenchmarkId::new("check_all_tables", n_tables),
            n_tables,
            |b, &n| {
                let manager = create_tenant_manager_with_policies(n);

                b.iter(|| {
                    // Check RLS for all tables
                    for i in 0..n {
                        let should_apply = manager.should_apply_rls(
                            &format!("table_{}", i),
                            "SELECT"
                        );
                        black_box(should_apply);
                    }
                });
            },
        );
    }

    group.finish();
}

fn benchmark_rls_command_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("rls_commands");

    let commands = ["SELECT", "INSERT", "UPDATE", "DELETE"];

    for cmd in commands.iter() {
        group.bench_function(*cmd, |b| {
            let manager = create_tenant_manager_with_policies(1);

            b.iter(|| {
                let should_apply = manager.should_apply_rls("table_0", cmd);
                black_box(should_apply);
            });
        });
    }

    group.finish();
}

// ============================================================================
// Quota Enforcement Benchmarks
// ============================================================================

fn benchmark_quota_checking(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_check");

    group.bench_function("check_connections", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let can_connect = manager.check_quota(tenant_id, "connections");
            black_box(can_connect);
        });
    });

    group.bench_function("check_storage", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let can_store = manager.check_quota(tenant_id, "storage");
            black_box(can_store);
        });
    });

    group.bench_function("check_qps", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let can_query = manager.check_quota(tenant_id, "qps");
            black_box(can_query);
        });
    });

    group.finish();
}

fn benchmark_quota_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_update");

    group.bench_function("add_connection", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let _ = manager.add_connection(tenant_id);
            let _ = manager.remove_connection(tenant_id);
        });
    });

    group.bench_function("record_query", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        // Set high QPS limit to avoid hitting it during benchmark
        manager.update_resource_limits(
            tenant_id,
            ResourceLimits {
                max_storage_bytes: 100 * 1024 * 1024 * 1024,
                max_connections: 1000,
                max_qps: 1_000_000,
            }
        ).unwrap();

        b.iter(|| {
            let _ = manager.record_query(tenant_id);
        });
    });

    group.bench_function("update_storage", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let _ = manager.update_storage_usage(tenant_id, 50 * 1024 * 1024);
        });
    });

    group.finish();
}

fn benchmark_quota_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_scaling");

    // Test quota checking with increasing number of tenants
    for n_tenants in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*n_tenants as u64));

        group.bench_with_input(
            BenchmarkId::new("check_all_tenants", n_tenants),
            n_tenants,
            |b, &n| {
                let manager = create_tenant_manager_with_n_tenants(n);
                let tenants = manager.list_tenants();

                b.iter(|| {
                    // Check quota for all tenants
                    for tenant in &tenants {
                        let can_connect = manager.check_quota(tenant.id, "connections");
                        black_box(can_connect);
                    }
                });
            },
        );
    }

    group.finish();
}

// ============================================================================
// CDC Performance Benchmarks
// ============================================================================

fn benchmark_cdc_event_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("cdc_record");

    group.bench_function("record_insert", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let event_id = manager.record_change_event(
                ChangeType::Insert,
                "orders".to_string(),
                format!("order_{}", rand::random::<u32>()),
                None,
                Some(r#"{"id": 123, "amount": 100.0}"#.to_string()),
                tenant_id,
                Some(1),
            );
            black_box(event_id);
        });
    });

    group.bench_function("record_update", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let event_id = manager.record_change_event(
                ChangeType::Update,
                "orders".to_string(),
                "order_123".to_string(),
                Some(r#"{"status": "pending"}"#.to_string()),
                Some(r#"{"status": "completed"}"#.to_string()),
                tenant_id,
                Some(2),
            );
            black_box(event_id);
        });
    });

    group.bench_function("record_delete", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            let event_id = manager.record_change_event(
                ChangeType::Delete,
                "orders".to_string(),
                "order_123".to_string(),
                Some(r#"{"id": 123}"#.to_string()),
                None,
                tenant_id,
                Some(3),
            );
            black_box(event_id);
        });
    });

    group.finish();
}

fn benchmark_cdc_log_retrieval(c: &mut Criterion) {
    let mut group = c.benchmark_group("cdc_retrieve");

    // Benchmark with different log sizes
    for log_size in [10, 100, 1000, 10000].iter() {
        group.throughput(Throughput::Elements(*log_size as u64));

        group.bench_with_input(
            BenchmarkId::new("get_recent_changes", log_size),
            log_size,
            |b, &size| {
                let manager = create_tenant_manager_with_n_tenants(1);
                let tenant_id = manager.list_tenants()[0].id;

                // Populate CDC log
                for i in 0..size {
                    manager.record_change_event(
                        ChangeType::Insert,
                        "orders".to_string(),
                        format!("order_{}", i),
                        None,
                        Some(format!(r#"{{"id": {}}}"#, i)),
                        tenant_id,
                        Some(i as u64),
                    );
                }

                b.iter(|| {
                    let changes = manager.get_recent_changes(tenant_id, 100);
                    black_box(changes);
                });
            },
        );
    }

    group.finish();
}

fn benchmark_cdc_multi_tenant(c: &mut Criterion) {
    let mut group = c.benchmark_group("cdc_multi_tenant");

    for n_tenants in [10, 100].iter() {
        group.bench_with_input(
            BenchmarkId::new("record_across_tenants", n_tenants),
            n_tenants,
            |b, &n| {
                let manager = create_tenant_manager_with_n_tenants(n);
                let tenants = manager.list_tenants();

                b.iter(|| {
                    // Record one event for each tenant
                    for tenant in &tenants {
                        manager.record_change_event(
                            ChangeType::Insert,
                            "orders".to_string(),
                            "order_1".to_string(),
                            None,
                            Some(r#"{"id": 1}"#.to_string()),
                            tenant.id,
                            Some(1),
                        );
                    }
                });
            },
        );
    }

    group.finish();
}

// ============================================================================
// Tenant Context Switching Benchmarks
// ============================================================================

fn benchmark_context_switching(c: &mut Criterion) {
    let mut group = c.benchmark_group("context_switch");

    group.bench_function("set_context", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            manager.set_current_context(TenantContext {
                tenant_id,
                user_id: "user1".to_string(),
                roles: vec!["user".to_string()],
                isolation_mode: IsolationMode::SharedSchema,
            });
        });
    });

    group.bench_function("get_context", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        manager.set_current_context(TenantContext {
            tenant_id,
            user_id: "user1".to_string(),
            roles: vec!["user".to_string()],
            isolation_mode: IsolationMode::SharedSchema,
        });

        b.iter(|| {
            let context = manager.get_current_context();
            black_box(context);
        });
    });

    group.bench_function("switch_between_tenants", |b| {
        let manager = create_tenant_manager_with_n_tenants(10);
        let tenants = manager.list_tenants();

        let mut tenant_idx = 0;

        b.iter(|| {
            manager.set_current_context(TenantContext {
                tenant_id: tenants[tenant_idx].id,
                user_id: format!("user_{}", tenant_idx),
                roles: vec!["user".to_string()],
                isolation_mode: IsolationMode::SharedSchema,
            });

            tenant_idx = (tenant_idx + 1) % tenants.len();
        });
    });

    group.finish();
}

// ============================================================================
// Tenant Management Benchmarks
// ============================================================================

fn benchmark_tenant_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("tenant_ops");

    group.bench_function("register_tenant", |b| {
        let manager = TenantManager::new();
        let mut counter = 0;

        b.iter(|| {
            let tenant = manager.register_tenant(
                format!("Tenant{}", counter),
                IsolationMode::SharedSchema,
            );
            counter += 1;
            black_box(tenant);
        });
    });

    group.bench_function("get_tenant", |b| {
        let manager = create_tenant_manager_with_n_tenants(100);
        let tenant_id = manager.list_tenants()[50].id;

        b.iter(|| {
            let tenant = manager.get_tenant(tenant_id);
            black_box(tenant);
        });
    });

    group.bench_function("list_tenants", |b| {
        let manager = create_tenant_manager_with_n_tenants(100);

        b.iter(|| {
            let tenants = manager.list_tenants();
            black_box(tenants);
        });
    });

    group.bench_function("update_limits", |b| {
        let manager = create_tenant_manager_with_n_tenants(1);
        let tenant_id = manager.list_tenants()[0].id;

        let limits = ResourceLimits {
            max_storage_bytes: 200 * 1024 * 1024,
            max_connections: 100,
            max_qps: 2000,
        };

        b.iter(|| {
            let _ = manager.update_resource_limits(tenant_id, limits.clone());
        });
    });

    group.finish();
}

// ============================================================================
// Concurrent Access Benchmarks
// ============================================================================

fn benchmark_concurrent_quota_checks(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_quota");

    group.bench_function("parallel_quota_checks", |b| {
        use std::thread;

        let manager = Arc::new(create_tenant_manager_with_n_tenants(10));
        let tenants = manager.list_tenants();

        b.iter(|| {
            let mut handles = vec![];

            for tenant in &tenants {
                let mgr = Arc::clone(&manager);
                let tid = tenant.id;

                let handle = thread::spawn(move || {
                    for _ in 0..10 {
                        let _ = mgr.check_quota(tid, "connections");
                    }
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().ok();
            }
        });
    });

    group.finish();
}

// ============================================================================
// Migration Benchmarks
// ============================================================================

fn benchmark_migration_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("migration");

    group.bench_function("start_migration", |b| {
        let manager = create_tenant_manager_with_n_tenants(2);
        let tenants = manager.list_tenants();
        let source_id = tenants[0].id;
        let target_id = tenants[1].id;

        b.iter(|| {
            let _ = manager.start_migration(source_id, target_id);
        });
    });

    group.bench_function("get_migration_status", |b| {
        let manager = create_tenant_manager_with_n_tenants(2);
        let tenants = manager.list_tenants();
        let source_id = tenants[0].id;
        let target_id = tenants[1].id;

        manager.start_migration(source_id, target_id).unwrap();

        b.iter(|| {
            let status = manager.get_migration_status(source_id, target_id);
            black_box(status);
        });
    });

    group.bench_function("record_progress", |b| {
        let manager = create_tenant_manager_with_n_tenants(2);
        let tenants = manager.list_tenants();
        let source_id = tenants[0].id;
        let target_id = tenants[1].id;

        manager.start_migration(source_id, target_id).unwrap();

        b.iter(|| {
            let _ = manager.record_replication_progress(source_id, target_id, 50, 100);
        });
    });

    group.finish();
}

// ============================================================================
// Composite Workload Benchmarks
// ============================================================================

fn benchmark_typical_query_workflow(c: &mut Criterion) {
    let mut group = c.benchmark_group("workflow");

    group.bench_function("typical_multi_tenant_query", |b| {
        let manager = create_tenant_manager_with_policies(10);
        let tenant_id = manager.list_tenants()[0].id;

        b.iter(|| {
            // Typical workflow for a query
            // 1. Check quota
            let can_query = manager.check_quota(tenant_id, "qps");
            if can_query {
                // 2. Record query
                let _ = manager.record_query(tenant_id);

                // 3. Check RLS
                let should_apply = manager.should_apply_rls("table_0", "SELECT");

                if should_apply {
                    // 4. Get RLS conditions
                    let conditions = manager.get_rls_conditions("table_0", "SELECT");
                    black_box(conditions);
                }

                // 5. Record CDC event (on write)
                manager.record_change_event(
                    ChangeType::Insert,
                    "table_0".to_string(),
                    "row_1".to_string(),
                    None,
                    Some(r#"{"data": "value"}"#.to_string()),
                    tenant_id,
                    Some(1),
                );
            }
        });
    });

    group.finish();
}

// ============================================================================
// Criterion Configuration
// ============================================================================

criterion_group!(
    rls_benches,
    benchmark_rls_check_overhead,
    benchmark_rls_scaling_with_policies,
    benchmark_rls_command_types,
);

criterion_group!(
    quota_benches,
    benchmark_quota_checking,
    benchmark_quota_updates,
    benchmark_quota_scaling,
);

criterion_group!(
    cdc_benches,
    benchmark_cdc_event_recording,
    benchmark_cdc_log_retrieval,
    benchmark_cdc_multi_tenant,
);

criterion_group!(
    context_benches,
    benchmark_context_switching,
);

criterion_group!(
    tenant_benches,
    benchmark_tenant_operations,
);

criterion_group!(
    concurrent_benches,
    benchmark_concurrent_quota_checks,
);

criterion_group!(
    migration_benches,
    benchmark_migration_operations,
);

criterion_group!(
    workflow_benches,
    benchmark_typical_query_workflow,
);

criterion_main!(
    rls_benches,
    quota_benches,
    cdc_benches,
    context_benches,
    tenant_benches,
    concurrent_benches,
    migration_benches,
    workflow_benches,
);
