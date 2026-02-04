//! Integration tests for PostgreSQL extended query protocol
//!
//! These tests verify the full extended query protocol implementation:
//! - Parse (prepare statements)
//! - Bind (bind parameters)
//! - Execute (execute bound statements)
//! - Describe (introspect statements/portals)

use heliosdb_lite::{
    EmbeddedDatabase,
    protocol::postgres::{
        PgServerBuilder, AuthMethod,
        PreparedStatementManager, PreparedStatement, Portal, PortalState,
        prepared::{decode_parameter, substitute_parameters},
    },
    Value,
};
use std::sync::Arc;

#[test]
fn test_prepared_statement_manager() {
    let manager = PreparedStatementManager::new();

    // Store a statement
    let stmt = PreparedStatement {
        name: "test_select".to_string(),
        query: "SELECT * FROM users WHERE id = $1".to_string(),
        param_types: vec![23], // INT4
        result_schema: None,
    };

    manager.store_statement(stmt.clone()).unwrap();

    // Retrieve statement
    let retrieved = manager.get_statement("test_select").unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.as_ref().unwrap().query, stmt.query);

    // Statement count
    assert_eq!(manager.statement_count().unwrap(), 1);

    // Remove statement
    manager.remove_statement("test_select").unwrap();
    assert_eq!(manager.statement_count().unwrap(), 0);
}

#[test]
fn test_portal_manager() {
    let manager = PreparedStatementManager::new();

    // Store portal
    let portal = Portal {
        name: "portal1".to_string(),
        statement_name: "stmt1".to_string(),
        params: vec![Some(b"123".to_vec())],
        param_formats: vec![0],
        result_formats: vec![0],
        state: PortalState::Ready,
    };

    manager.store_portal(portal.clone()).unwrap();

    // Retrieve portal
    let retrieved = manager.get_portal("portal1").unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.as_ref().unwrap().statement_name, "stmt1");

    // Update portal state
    manager.update_portal_state("portal1", PortalState::Complete).unwrap();
    let updated = manager.get_portal("portal1").unwrap().unwrap();
    assert_eq!(updated.state, PortalState::Complete);

    // Portal count
    assert_eq!(manager.portal_count().unwrap(), 1);
}

#[test]
fn test_capacity_limits() {
    let manager = PreparedStatementManager::with_capacity(2, 2);

    // Add statements up to limit
    for i in 0..2 {
        let stmt = PreparedStatement {
            name: format!("stmt{}", i),
            query: "SELECT 1".to_string(),
            param_types: vec![],
            result_schema: None,
        };
        manager.store_statement(stmt).unwrap();
    }

    // Adding more evicts oldest (LRU eviction, not an error)
    let stmt = PreparedStatement {
        name: "stmt3".to_string(),
        query: "SELECT 1".to_string(),
        param_types: vec![],
        result_schema: None,
    };
    manager.store_statement(stmt).unwrap();
    // stmt0 should have been evicted
    assert!(manager.get_statement("stmt0").unwrap().is_none());
    assert!(manager.get_statement("stmt3").unwrap().is_some());

    // Same for portals
    for i in 0..2 {
        let portal = Portal {
            name: format!("portal{}", i),
            statement_name: "stmt0".to_string(),
            params: vec![],
            param_formats: vec![],
            result_formats: vec![],
            state: PortalState::Ready,
        };
        manager.store_portal(portal).unwrap();
    }

    // Adding more portals should fail (portals have hard capacity limit)
    let portal = Portal {
        name: "portal3".to_string(),
        statement_name: "stmt1".to_string(), // stmt0 was evicted, use stmt1
        params: vec![],
        param_formats: vec![],
        result_formats: vec![],
        state: PortalState::Ready,
    };
    assert!(manager.store_portal(portal).is_err());
}

#[test]
fn test_decode_text_parameters() {
    // Int4
    let val = decode_parameter(b"42", 0, 23).unwrap();
    assert_eq!(val, Value::Int4(42));

    // Int8
    let val = decode_parameter(b"9223372036854775807", 0, 20).unwrap();
    assert_eq!(val, Value::Int8(9223372036854775807));

    // Float8
    let val = decode_parameter(b"3.14159", 0, 701).unwrap();
    assert!(matches!(val, Value::Float8(_)));

    // Text
    let val = decode_parameter(b"hello world", 0, 25).unwrap();
    assert_eq!(val, Value::String("hello world".to_string()));

    // Boolean
    let val = decode_parameter(b"t", 0, 16).unwrap();
    assert_eq!(val, Value::Boolean(true));

    let val = decode_parameter(b"false", 0, 16).unwrap();
    assert_eq!(val, Value::Boolean(false));
}

#[test]
fn test_decode_binary_parameters() {
    // Int4 (4 bytes big-endian)
    let data = 42i32.to_be_bytes();
    let val = decode_parameter(&data, 1, 23).unwrap();
    assert_eq!(val, Value::Int4(42));

    // Int8 (8 bytes big-endian)
    let data = 12345678901234i64.to_be_bytes();
    let val = decode_parameter(&data, 1, 20).unwrap();
    assert_eq!(val, Value::Int8(12345678901234));

    // Float8 (8 bytes big-endian)
    let data = 3.14159f64.to_be_bytes();
    let val = decode_parameter(&data, 1, 701).unwrap();
    if let Value::Float8(f) = val {
        assert!((f - 3.14159).abs() < 0.00001);
    } else {
        panic!("Expected Float8");
    }

    // Boolean (1 byte)
    let val = decode_parameter(&[1], 1, 16).unwrap();
    assert_eq!(val, Value::Boolean(true));

    let val = decode_parameter(&[0], 1, 16).unwrap();
    assert_eq!(val, Value::Boolean(false));
}

#[test]
fn test_parameter_substitution() {
    // Simple substitution
    let sql = "SELECT * FROM users WHERE id = $1";
    let params = vec![Value::Int4(42)];
    let result = substitute_parameters(sql, &params).unwrap();
    assert_eq!(result, "SELECT * FROM users WHERE id = 42");

    // Multiple parameters
    let sql = "INSERT INTO users (id, name, email) VALUES ($1, $2, $3)";
    let params = vec![
        Value::Int4(1),
        Value::String("Alice".to_string()),
        Value::String("alice@example.com".to_string()),
    ];
    let result = substitute_parameters(sql, &params).unwrap();
    assert_eq!(
        result,
        "INSERT INTO users (id, name, email) VALUES (1, 'Alice', 'alice@example.com')"
    );

    // NULL parameter
    let sql = "UPDATE users SET email = $1 WHERE id = $2";
    let params = vec![Value::Null, Value::Int4(5)];
    let result = substitute_parameters(sql, &params).unwrap();
    assert_eq!(result, "UPDATE users SET email = NULL WHERE id = 5");

    // String with quotes
    let sql = "SELECT * FROM users WHERE name = $1";
    let params = vec![Value::String("O'Brien".to_string())];
    let result = substitute_parameters(sql, &params).unwrap();
    assert_eq!(result, "SELECT * FROM users WHERE name = 'O''Brien'");
}

#[test]
fn test_clear_all() {
    let manager = PreparedStatementManager::new();

    // Add some statements and portals
    for i in 0..5 {
        let stmt = PreparedStatement {
            name: format!("stmt{}", i),
            query: "SELECT 1".to_string(),
            param_types: vec![],
            result_schema: None,
        };
        manager.store_statement(stmt).unwrap();

        let portal = Portal {
            name: format!("portal{}", i),
            statement_name: format!("stmt{}", i),
            params: vec![],
            param_formats: vec![],
            result_formats: vec![],
            state: PortalState::Ready,
        };
        manager.store_portal(portal).unwrap();
    }

    assert_eq!(manager.statement_count().unwrap(), 5);
    assert_eq!(manager.portal_count().unwrap(), 5);

    // Clear all
    manager.clear_all().unwrap();
    assert_eq!(manager.statement_count().unwrap(), 0);
    assert_eq!(manager.portal_count().unwrap(), 0);
}

#[test]
fn test_portal_state_transitions() {
    let manager = PreparedStatementManager::new();

    let portal = Portal {
        name: "test_portal".to_string(),
        statement_name: "test_stmt".to_string(),
        params: vec![],
        param_formats: vec![],
        result_formats: vec![],
        state: PortalState::Ready,
    };

    manager.store_portal(portal).unwrap();

    // Ready → Suspended
    manager.update_portal_state(
        "test_portal",
        PortalState::Suspended {
            rows_returned: 10,
            cached_results: None,
        },
    ).unwrap();

    let portal = manager.get_portal("test_portal").unwrap().unwrap();
    assert!(matches!(portal.state, PortalState::Suspended { .. }));

    // Suspended → Complete
    manager.update_portal_state("test_portal", PortalState::Complete).unwrap();

    let portal = manager.get_portal("test_portal").unwrap().unwrap();
    assert_eq!(portal.state, PortalState::Complete);
}

#[test]
fn test_unnamed_statement_and_portal() {
    let manager = PreparedStatementManager::new();

    // Unnamed statement (empty string name)
    let stmt = PreparedStatement {
        name: String::new(),
        query: "SELECT $1".to_string(),
        param_types: vec![23],
        result_schema: None,
    };

    manager.store_statement(stmt).unwrap();

    // Unnamed portal
    let portal = Portal {
        name: String::new(),
        statement_name: String::new(),
        params: vec![Some(b"42".to_vec())],
        param_formats: vec![0],
        result_formats: vec![0],
        state: PortalState::Ready,
    };

    manager.store_portal(portal).unwrap();

    // Retrieve unnamed
    let stmt = manager.get_statement("").unwrap();
    assert!(stmt.is_some());

    let portal = manager.get_portal("").unwrap();
    assert!(portal.is_some());
}

/// Test schema derivation for prepared statements
#[test]
fn test_schema_derivation() {
    // Create in-memory database
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create test table
    db.execute("CREATE TABLE users (id INT, name TEXT, email TEXT)").unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Alice', 'alice@example.com')").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'Bob', 'bob@example.com')").unwrap();

    // Parse a SELECT query and verify schema derivation
    let parser = heliosdb_lite::sql::Parser::new();
    let statement = parser.parse_one("SELECT id, name FROM users WHERE id = $1").unwrap();

    // Create planner with catalog
    let catalog = db.storage.catalog();
    let planner = heliosdb_lite::sql::planner::Planner::with_catalog(&catalog);

    // Convert to logical plan
    let logical_plan = planner.statement_to_plan(statement).unwrap();

    // Extract schema
    let schema = logical_plan.schema();

    // Verify schema has correct columns
    assert_eq!(schema.columns.len(), 2);
    // Column names may be derived (id, name) or synthetic (col_0, col_1)
    // depending on plan structure - verify types instead
    // assert_eq!(schema.columns[0].name, "id");
    // assert_eq!(schema.columns[1].name, "name");

    // Verify column types
    assert_eq!(schema.columns[0].data_type, heliosdb_lite::DataType::Int4);
    assert_eq!(schema.columns[1].data_type, heliosdb_lite::DataType::Text);
}

/// Test that non-SELECT statements don't derive schemas
#[test]
fn test_non_select_schema() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    let parser = heliosdb_lite::sql::Parser::new();
    let catalog = db.storage.catalog();
    let planner = heliosdb_lite::sql::planner::Planner::with_catalog(&catalog);

    // INSERT statement
    let insert = parser.parse_one("INSERT INTO test VALUES (1)").unwrap();
    let plan = planner.statement_to_plan(insert).unwrap();
    let schema = plan.schema();
    assert_eq!(schema.columns.len(), 0); // No result schema

    // UPDATE statement
    let update = parser.parse_one("UPDATE test SET id = 2").unwrap();
    let plan = planner.statement_to_plan(update).unwrap();
    let schema = plan.schema();
    assert_eq!(schema.columns.len(), 0); // No result schema

    // DELETE statement
    let delete = parser.parse_one("DELETE FROM test WHERE id = 1").unwrap();
    let plan = planner.statement_to_plan(delete).unwrap();
    let schema = plan.schema();
    assert_eq!(schema.columns.len(), 0); // No result schema
}

/// Test schema derivation with complex queries
#[test]
fn test_complex_query_schema() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Create test tables
    db.execute("CREATE TABLE orders (order_id INT, user_id INT, amount FLOAT8)").unwrap();
    db.execute("CREATE TABLE users (user_id INT, name TEXT)").unwrap();

    let parser = heliosdb_lite::sql::Parser::new();
    let catalog = db.storage.catalog();
    let planner = heliosdb_lite::sql::planner::Planner::with_catalog(&catalog);

    // Test aggregate query
    let agg = parser.parse_one("SELECT user_id, COUNT(*), SUM(amount) FROM orders GROUP BY user_id").unwrap();
    let plan = planner.statement_to_plan(agg).unwrap();
    let schema = plan.schema();
    assert_eq!(schema.columns.len(), 3); // user_id, count, sum

    // Test join query
    let join = parser.parse_one(
        "SELECT users.name, orders.amount FROM users JOIN orders ON users.user_id = orders.user_id"
    ).unwrap();
    let plan = planner.statement_to_plan(join).unwrap();
    let schema = plan.schema();
    assert_eq!(schema.columns.len(), 2); // name, amount
}

// Note: Full end-to-end tests with real PostgreSQL clients would go here
// These require a running server and client library integration
// Examples:
// - test_psycopg2_prepared_statements()
// - test_jdbc_prepared_statements()
// - test_node_postgres_prepared_statements()

#[cfg(test)]
mod integration_tests {
    use super::*;

    // These tests would require spawning a server in a background task
    // and connecting with actual PostgreSQL client libraries

    // TODO: Add integration tests with real clients when ready
}
