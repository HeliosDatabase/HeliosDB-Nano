// SQL Wrapper Parser Tests for Phase 3

use heliosdb_nano::sql::parser::*;
use sqlparser::ast::{Statement, Expr};

#[test]
fn test_parse_create_materialized_view_basic() {
    let sql = r#"
        CREATE MATERIALIZED VIEW user_stats AS
        SELECT user_id, COUNT(*) as order_count
        FROM orders
        GROUP BY user_id
    "#;

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse basic CREATE MATERIALIZED VIEW");

    let stmts = result.unwrap();
    assert_eq!(stmts.len(), 1);

    match &stmts[0] {
        Statement::CreateView { name, columns, query, materialized, .. } => {
            assert!(materialized.unwrap_or(false), "Should be materialized");
            assert_eq!(name.to_string(), "user_stats");
        }
        _ => panic!("Expected CreateView statement"),
    }
}

#[test]
fn test_parse_create_materialized_view_with_auto_refresh() {
    let sql = r#"
        CREATE MATERIALIZED VIEW user_totals AS
        SELECT user_id, SUM(amount) as total
        FROM orders
        GROUP BY user_id
        WITH (auto_refresh = true, max_cpu_percent = 15, refresh_interval = '1 second')
    "#;

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse MV with auto_refresh options");

    // In a real implementation, we'd extract and validate the WITH options
    // This is a placeholder for the actual validation logic
}

#[test]
fn test_parse_create_vector_index_with_hnsw() {
    let sql = r#"
        CREATE INDEX vec_idx ON documents
        USING hnsw (embedding vector_cosine_ops)
    "#;

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse HNSW index creation");

    let stmts = result.unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_parse_create_vector_index_with_product_quantization() {
    let sql = r#"
        CREATE INDEX vec_idx ON documents
        USING hnsw (embedding vector_cosine_ops)
        WITH (
            quantization = 'product',
            pq_subquantizers = 8,
            m = 16,
            ef_construction = 200,
            ef_search = 100
        )
    "#;

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse HNSW index with PQ options");

    // Validate PQ-specific options would be extracted here
}

#[test]
fn test_parse_refresh_materialized_view() {
    let sql = "REFRESH MATERIALIZED VIEW user_stats";

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse REFRESH MATERIALIZED VIEW");
}

#[test]
fn test_parse_vector_distance_operator() {
    let sql = r#"
        SELECT id, embedding <=> '[0.1, 0.2, 0.3]' as distance
        FROM documents
        ORDER BY distance
        LIMIT 10
    "#;

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse vector distance operator");
}

#[test]
fn test_parse_create_index_with_compression_options() {
    let sql = r#"
        CREATE TABLE metrics (
            timestamp TIMESTAMP,
            value DOUBLE PRECISION
        ) WITH (
            compression = 'alp',
            compression_level = 9
        )
    "#;

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse table with compression options");
}

#[test]
fn test_parse_invalid_materialized_view() {
    let sql = r#"
        CREATE MATERIALIZED VIEW invalid_view AS
        SELECT * FROM nonexistent_table
        WITH (invalid_option = true)
    "#;

    let result = parse_sql(sql);
    // Parser should succeed, but validation should fail later
    assert!(result.is_ok(), "Parser should accept syntactically valid SQL");
}

#[test]
fn test_parse_system_view_query() {
    let sql = "SELECT * FROM pg_mv_staleness()";

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse system view query");
}

#[test]
fn test_parse_vector_index_stats_query() {
    let sql = "SELECT * FROM pg_vector_index_stats('vec_idx')";

    let result = parse_sql(sql);
    assert!(result.is_ok(), "Failed to parse pg_vector_index_stats query");
}

// Helper function to parse SQL
fn parse_sql(sql: &str) -> Result<Vec<Statement>, String> {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| format!("Parse error: {}", e))
}

#[cfg(test)]
mod edge_cases {
    use super::*;

    #[test]
    fn test_parse_empty_sql() {
        let sql = "";
        let result = parse_sql(sql);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn test_parse_multiple_statements() {
        let sql = r#"
            CREATE TABLE test (id INT);
            CREATE MATERIALIZED VIEW test_mv AS SELECT * FROM test;
            REFRESH MATERIALIZED VIEW test_mv;
        "#;

        let result = parse_sql(sql);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 3);
    }

    #[test]
    fn test_parse_sql_with_comments() {
        let sql = r#"
            -- Create a materialized view
            CREATE MATERIALIZED VIEW user_stats AS
            SELECT user_id, COUNT(*) as cnt  -- count orders
            FROM orders
            GROUP BY user_id
        "#;

        let result = parse_sql(sql);
        assert!(result.is_ok());
    }
}
