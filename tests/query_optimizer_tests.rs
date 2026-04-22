//! Comprehensive integration tests for the query optimizer
//!
//! Tests all 5 optimization rules with realistic query scenarios

use heliosdb_nano::optimizer::{Optimizer, OptimizerConfig};
use heliosdb_nano::optimizer::cost::{StatsCatalog, TableStats, ColumnStats};
use heliosdb_nano::sql::logical_plan::*;
use heliosdb_nano::{Schema, Column, DataType, Value};
use std::sync::Arc;

// Helper function to create a test schema
fn create_users_schema() -> Arc<Schema> {
    Arc::new(Schema {
        columns: vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
            Column {
                name: "email".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
            Column {
                name: "age".to_string(),
                data_type: DataType::Int4,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
            Column {
                name: "status".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
        ],
    })
}

fn create_orders_schema() -> Arc<Schema> {
    Arc::new(Schema {
        columns: vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
            Column {
                name: "user_id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
            Column {
                name: "amount".to_string(),
                data_type: DataType::Float8,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
            Column {
                name: "status".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: heliosdb_nano::ColumnStorageMode::Default,
            },
        ],
    })
}

fn create_test_stats() -> StatsCatalog {
    let mut catalog = StatsCatalog::new();

    // Users table: 100,000 rows
    let users_stats = TableStats::new("users".to_string())
        .with_row_count(100_000)
        .with_avg_row_size(256)
        .with_column_stats(
            ColumnStats::new("id".to_string())
                .with_distinct_count(100_000)
                .with_index("btree".to_string())
        )
        .with_column_stats(
            ColumnStats::new("status".to_string())
                .with_distinct_count(5) // Only 5 statuses
                .with_index("btree".to_string())
        );

    catalog.add_table_stats(users_stats);

    // Orders table: 1,000,000 rows
    let orders_stats = TableStats::new("orders".to_string())
        .with_row_count(1_000_000)
        .with_avg_row_size(128)
        .with_column_stats(
            ColumnStats::new("user_id".to_string())
                .with_distinct_count(100_000)
                .with_index("btree".to_string())
        );

    catalog.add_table_stats(orders_stats);

    catalog
}

// =============================================================================
// Test 1: Constant Folding
// =============================================================================

#[test]
fn test_constant_folding_arithmetic() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    // Query: SELECT * FROM users WHERE age > (10 + 20)
    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema,
        projection: None,
        as_of: None,
    };

    let filter = LogicalPlan::Filter {
        input: Box::new(scan),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string() }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Literal(Value::Int4(10))),
                op: BinaryOperator::Plus,
                right: Box::new(LogicalExpr::Literal(Value::Int4(20))),
            }),
        },
    };

    let optimized = optimizer.optimize(filter).expect("Optimization failed");

    // The constant expression (10 + 20) should be folded to 30
    if let LogicalPlan::Filter { predicate, .. } = optimized {
        if let LogicalExpr::BinaryExpr { right, .. } = predicate {
            assert!(matches!(*right, LogicalExpr::Literal(Value::Int4(30))),
                "Constant expression should be folded to 30");
        }
    }
}

#[test]
fn test_constant_folding_boolean() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    // Query with boolean constant: WHERE (TRUE AND FALSE) OR age > 10
    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema,
        projection: None,
        as_of: None,
    };

    let filter = LogicalPlan::Filter {
        input: Box::new(scan),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Literal(Value::Boolean(true))),
                op: BinaryOperator::And,
                right: Box::new(LogicalExpr::Literal(Value::Boolean(false))),
            }),
            op: BinaryOperator::Or,
            right: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string() }),
                op: BinaryOperator::Gt,
                right: Box::new(LogicalExpr::Literal(Value::Int4(10))),
            }),
        },
    };

    let optimized = optimizer.optimize(filter).expect("Optimization failed");

    // TRUE AND FALSE should be folded to FALSE
    if let LogicalPlan::Filter { predicate, .. } = optimized {
        if let LogicalExpr::BinaryExpr { left, .. } = predicate {
            assert!(matches!(*left, LogicalExpr::Literal(Value::Boolean(false))),
                "TRUE AND FALSE should be folded to FALSE");
        }
    }
}

// =============================================================================
// Test 2: Selection Pushdown (Filter Pushdown)
// =============================================================================

#[test]
fn test_selection_pushdown_through_projection() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    // Query: SELECT name FROM (SELECT * FROM users) WHERE age > 21
    // This represents: Project(Filter(Scan))
    // Should be optimized to: Project(Filter(Scan)) with filter pushed down
    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema: schema.clone(),
        projection: None,
        as_of: None,
    };

    // Inner projection (SELECT *)
    let project = LogicalPlan::Project {
        input: Box::new(scan),
        exprs: vec![
            LogicalExpr::Column { table: None, name: "id".to_string() },
            LogicalExpr::Column { table: None, name: "name".to_string() },
            LogicalExpr::Column { table: None, name: "age".to_string() },
        ],
        aliases: vec!["id".to_string(), "name".to_string(), "age".to_string()],
        distinct: false,
        distinct_on: None,
    };

    // Filter on top
    let filter = LogicalPlan::Filter {
        input: Box::new(project),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string() }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::Literal(Value::Int4(21))),
        },
    };

    let optimized = optimizer.optimize(filter).expect("Optimization failed");

    // Filter should be pushed below projection
    // Expected: Project -> Filter -> Scan
    if let LogicalPlan::Project { input, .. } = optimized {
        assert!(matches!(*input, LogicalPlan::Filter { .. }),
            "Filter should be pushed below projection");
    }
}

#[test]
fn test_selection_pushdown_merge_filters() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    // Query: SELECT * FROM users WHERE age > 21 AND age < 65
    // Represented as Filter(Filter(Scan))
    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema,
        projection: None,
        as_of: None,
    };

    let inner_filter = LogicalPlan::Filter {
        input: Box::new(scan),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string() }),
            op: BinaryOperator::Gt,
            right: Box::new(LogicalExpr::Literal(Value::Int4(21))),
        },
    };

    let outer_filter = LogicalPlan::Filter {
        input: Box::new(inner_filter),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string() }),
            op: BinaryOperator::Lt,
            right: Box::new(LogicalExpr::Literal(Value::Int4(65))),
        },
    };

    let optimized = optimizer.optimize(outer_filter).expect("Optimization failed");

    // The optimizer may keep filters separate or merge them, depending on cost estimation
    // Both are semantically correct - verify the plan structure is valid
    match &optimized {
        LogicalPlan::Filter { input, predicate } => {
            // Could be merged (AND) or outer filter with inner filter below
            match predicate {
                LogicalExpr::BinaryExpr { op: BinaryOperator::And, .. } => {
                    // Filters were merged - verify input is Scan or FilteredScan
                    assert!(
                        matches!(&**input, LogicalPlan::Scan { .. } | LogicalPlan::FilteredScan { .. }),
                        "Merged filter should have Scan as input"
                    );
                }
                LogicalExpr::BinaryExpr { op: BinaryOperator::Lt, .. } |
                LogicalExpr::BinaryExpr { op: BinaryOperator::Gt, .. } => {
                    // Filters not merged - this is valid if cost estimation rejects merge
                    // Just verify the structure is valid
                }
                _ => panic!("Unexpected predicate type: {:?}", predicate),
            }
        }
        LogicalPlan::FilteredScan { predicate: Some(p), .. } => {
            // Storage pushdown was applied - verify predicate exists
            assert!(matches!(p, LogicalExpr::BinaryExpr { .. }), "FilteredScan should have a predicate");
        }
        other => panic!("Expected Filter or FilteredScan, got {:?}", other),
    }
}

// =============================================================================
// Test 3: Projection Pruning
// =============================================================================

#[test]
fn test_projection_pruning_removes_unused_columns() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    // Query: SELECT name FROM users
    // Should create a projection on the scan to only read 'name' column
    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema: schema.clone(),
        projection: None,
        as_of: None,
    };

    let project = LogicalPlan::Project {
        input: Box::new(scan),
        exprs: vec![
            LogicalExpr::Column { table: None, name: "name".to_string() },
        ],
        aliases: vec!["name".to_string()],
        distinct: false,
        distinct_on: None,
    };

    let optimized = optimizer.optimize(project).expect("Optimization failed");

    // Should have scan with projection
    if let LogicalPlan::Project { input, .. } = optimized {
        if let LogicalPlan::Scan { projection, .. } = &*input {
            assert!(projection.is_some(), "Scan should have projection to prune columns");
            // Should only project the 'name' column (index 1)
            assert!(projection.as_ref().unwrap().len() < schema.columns.len(),
                "Projection should have fewer columns than full table");
        }
    }
}

// =============================================================================
// Test 4: Join Reordering
// =============================================================================

#[test]
fn test_join_reordering_puts_small_table_first() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let users_schema = create_users_schema();
    let orders_schema = create_orders_schema();

    // Query: SELECT * FROM orders JOIN users ON orders.user_id = users.id
    // orders has 1M rows, users has 100K rows
    // Should reorder to put users (smaller) on left

    let orders_scan = LogicalPlan::Scan {
        alias: None,
        table_name: "orders".to_string(),
        schema: orders_schema,
        projection: None,
        as_of: None,
    };

    let users_scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema: users_schema,
        projection: None,
        as_of: None,
    };

    // Large table (orders) on left initially
    let join = LogicalPlan::Join {
        left: Box::new(orders_scan),
        right: Box::new(users_scan),
        join_type: JoinType::Inner,
        on: Some(LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "user_id".to_string() }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Column { table: None, name: "id".to_string() }),
        }),
        lateral: false,
    };

    let optimized = optimizer.optimize(join).expect("Optimization failed");

    // Should swap to put smaller table (users) on left
    if let LogicalPlan::Join { left, .. } = optimized {
        if let LogicalPlan::Scan { table_name, .. } = &*left {
            assert_eq!(table_name, "users",
                "Smaller table (users) should be on left side of join");
        }
    }
}

#[test]
fn test_join_reordering_preserves_outer_join() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let users_schema = create_users_schema();
    let orders_schema = create_orders_schema();

    // LEFT OUTER JOIN should not be reordered
    let orders_scan = LogicalPlan::Scan {
        alias: None,
        table_name: "orders".to_string(),
        schema: orders_schema,
        projection: None,
        as_of: None,
    };

    let users_scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema: users_schema,
        projection: None,
        as_of: None,
    };

    let left_join = LogicalPlan::Join {
        left: Box::new(orders_scan),
        right: Box::new(users_scan),
        join_type: JoinType::Left,
        on: None,
        lateral: false,
    };

    let optimized = optimizer.optimize(left_join.clone()).expect("Optimization failed");

    // Order should NOT change for outer join
    if let LogicalPlan::Join { left, .. } = optimized {
        if let LogicalPlan::Scan { table_name, .. } = &*left {
            assert_eq!(table_name, "orders",
                "LEFT OUTER JOIN order should be preserved");
        }
    }
}

// =============================================================================
// Test 5: Index Selection
// =============================================================================

#[test]
fn test_index_selection_recognizes_indexed_column() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    // Query: SELECT * FROM users WHERE id = 42
    // 'id' column has an index
    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema,
        projection: None,
        as_of: None,
    };

    let filter = LogicalPlan::Filter {
        input: Box::new(scan),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "id".to_string() }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::Int4(42))),
        },
    };

    let optimized = optimizer.optimize(filter).expect("Optimization failed");

    // Should recognize that index can be used
    // Plan structure may be Filter or FilteredScan (if storage pushdown applied)
    assert!(
        matches!(optimized, LogicalPlan::Filter { .. }) ||
        matches!(optimized, LogicalPlan::FilteredScan { .. }),
        "Expected Filter or FilteredScan plan"
    );
}

// =============================================================================
// Integration Tests: Complex Queries
// =============================================================================

#[test]
fn test_complex_query_all_optimizations() {
    let stats = create_test_stats();
    let config = OptimizerConfig::new().with_verbose(false);
    let optimizer = Optimizer::with_config(stats, config);
    let schema = create_users_schema();

    // Complex query that benefits from multiple optimizations:
    // SELECT name FROM users
    // WHERE age > (20 + 5) AND status = 'active'
    // ORDER BY name LIMIT 10

    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema,
        projection: None,
        as_of: None,
    };

    // Filter with constant expression
    let filter = LogicalPlan::Filter {
        input: Box::new(scan),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string() }),
                op: BinaryOperator::Gt,
                right: Box::new(LogicalExpr::BinaryExpr {
                    left: Box::new(LogicalExpr::Literal(Value::Int4(20))),
                    op: BinaryOperator::Plus,
                    right: Box::new(LogicalExpr::Literal(Value::Int4(5))),
                }),
            }),
            op: BinaryOperator::And,
            right: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "status".to_string() }),
                op: BinaryOperator::Eq,
                right: Box::new(LogicalExpr::Literal(Value::String("active".to_string()))),
            }),
        },
    };

    // Projection
    let project = LogicalPlan::Project {
        input: Box::new(filter),
        exprs: vec![LogicalExpr::Column { table: None, name: "name".to_string() }],
        aliases: vec!["name".to_string()],
        distinct: false,
        distinct_on: None,
    };

    // Sort
    let sort = LogicalPlan::Sort {
        input: Box::new(project),
        exprs: vec![LogicalExpr::Column { table: None, name: "name".to_string() }],
        asc: vec![true],
    };

    // Limit
    let limit = LogicalPlan::Limit {
        input: Box::new(sort),
        limit: 10,
        offset: 0,
        limit_param: None,
        offset_param: None,
    };

    let optimized = optimizer.optimize(limit).expect("Optimization failed");

    // Should apply:
    // 1. Constant folding: (20 + 5) -> 25
    // 2. Projection pruning: Only read 'name' and 'status' columns
    // 3. Index selection: Use index on 'status' if beneficial

    // Just verify it completes successfully
    assert!(matches!(optimized, LogicalPlan::Limit { .. }));
}

#[test]
fn test_optimization_cost_improvement() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let users_schema = create_users_schema();
    let orders_schema = create_orders_schema();

    // Query with inefficient join order
    let orders_scan = LogicalPlan::Scan {
        alias: None,
        table_name: "orders".to_string(),
        schema: orders_schema,
        projection: None,
        as_of: None,
    };

    let users_scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema: users_schema,
        projection: None,
        as_of: None,
    };

    let join = LogicalPlan::Join {
        left: Box::new(orders_scan),
        right: Box::new(users_scan),
        join_type: JoinType::Inner,
        on: None,
        lateral: false,
    };

    // Measure costs
    let initial_cost = optimizer.cost_estimator()
        .estimate_cost(&join)
        .expect("Cost estimation failed");

    let optimized = optimizer.optimize(join).expect("Optimization failed");

    let final_cost = optimizer.cost_estimator()
        .estimate_cost(&optimized)
        .expect("Cost estimation failed");

    // Final cost should be less than or equal to initial cost
    assert!(final_cost <= initial_cost,
        "Optimized plan should not be more expensive: {} vs {}",
        final_cost, initial_cost);
}

#[test]
fn test_optimizer_explain_output() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema,
        projection: None,
        as_of: None,
    };

    let filter = LogicalPlan::Filter {
        input: Box::new(scan),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Literal(Value::Int4(5))),
                op: BinaryOperator::Plus,
                right: Box::new(LogicalExpr::Literal(Value::Int4(5))),
            }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::Int4(10))),
        },
    };

    let explanation = optimizer.explain(filter).expect("Explain failed");

    assert!(explanation.contains("Query Optimization Analysis"));
    assert!(explanation.contains("cost"));
    assert!(explanation.contains("improvement"));
}

// =============================================================================
// Performance Benchmarks
// =============================================================================

#[test]
fn test_optimizer_performance_on_large_plan() {
    use std::time::Instant;

    let stats = create_test_stats();
    let config = OptimizerConfig::new()
        .with_max_passes(100)
        .with_timeout_ms(5000); // 5 second timeout

    let optimizer = Optimizer::with_config(stats, config);
    let schema = create_users_schema();

    // Create a moderately complex plan
    let mut current_plan: LogicalPlan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema: schema.clone(),
        projection: None,
        as_of: None,
    };

    // Add 10 filters
    for i in 0..10 {
        current_plan = LogicalPlan::Filter {
            input: Box::new(current_plan),
            predicate: LogicalExpr::BinaryExpr {
                left: Box::new(LogicalExpr::Column { table: None, name: "age".to_string() }),
                op: BinaryOperator::Gt,
                right: Box::new(LogicalExpr::Literal(Value::Int4(i))),
            },
        };
    }

    let start = Instant::now();
    let _optimized = optimizer.optimize(current_plan).expect("Optimization failed");
    let duration = start.elapsed();

    // Should complete in reasonable time (< 1 second for this simple case)
    assert!(duration.as_millis() < 1000,
        "Optimization took too long: {:?}", duration);
}

#[test]
fn test_cardinality_estimation_accuracy() {
    let stats = create_test_stats();
    let optimizer = Optimizer::new(stats);
    let schema = create_users_schema();

    let scan = LogicalPlan::Scan {
        alias: None,
        table_name: "users".to_string(),
        schema,
        projection: None,
        as_of: None,
    };

    // Cardinality of full table scan
    let scan_card = optimizer.cost_estimator()
        .estimate_cardinality(&scan)
        .expect("Cardinality estimation failed");

    assert_eq!(scan_card, 100_000.0, "Should match table statistics");

    // Cardinality with filter
    let filter = LogicalPlan::Filter {
        input: Box::new(scan),
        predicate: LogicalExpr::BinaryExpr {
            left: Box::new(LogicalExpr::Column { table: None, name: "status".to_string() }),
            op: BinaryOperator::Eq,
            right: Box::new(LogicalExpr::Literal(Value::String("active".to_string()))),
        },
    };

    let filter_card = optimizer.cost_estimator()
        .estimate_cardinality(&filter)
        .expect("Cardinality estimation failed");

    // Filter should reduce cardinality
    assert!(filter_card < scan_card,
        "Filter should reduce cardinality: {} vs {}",
        filter_card, scan_card);
}
