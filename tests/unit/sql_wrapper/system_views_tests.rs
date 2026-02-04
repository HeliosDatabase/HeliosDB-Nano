// System Views Tests for Phase 3

#[cfg(test)]
mod system_views_tests {
    use heliosdb_lite::storage::engine::StorageEngine;
    use tempfile::TempDir;

    async fn setup_test_db() -> (StorageEngine, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let engine = StorageEngine::new(temp_dir.path()).await.unwrap();
        (engine, temp_dir)
    }

    #[tokio::test]
    async fn test_pg_mv_staleness_view() {
        let (engine, _temp) = setup_test_db().await;

        // Create a materialized view
        engine.execute_sql(r#"
            CREATE TABLE orders (user_id INT, amount DECIMAL);
            CREATE MATERIALIZED VIEW user_totals AS
            SELECT user_id, SUM(amount) as total
            FROM orders
            GROUP BY user_id;
        "#).await.unwrap();

        // Query staleness
        let result = engine.execute_sql("SELECT * FROM pg_mv_staleness()").await;
        assert!(result.is_ok(), "pg_mv_staleness() should work");

        let rows = result.unwrap();
        assert!(rows.len() > 0, "Should return at least one MV");

        // Verify columns exist
        let first_row = &rows[0];
        assert!(first_row.contains_key("view_name"));
        assert!(first_row.contains_key("staleness_sec"));
        assert!(first_row.contains_key("last_refresh"));
        assert!(first_row.contains_key("delta_rows"));
    }

    #[tokio::test]
    async fn test_pg_mv_cpu_usage_view() {
        let (engine, _temp) = setup_test_db().await;

        // Create MV with CPU limit
        engine.execute_sql(r#"
            CREATE TABLE test (id INT);
            CREATE MATERIALIZED VIEW test_mv AS
            SELECT COUNT(*) as cnt FROM test
            WITH (auto_refresh = true, max_cpu_percent = 10);
        "#).await.unwrap();

        // Query CPU usage
        let result = engine.execute_sql("SELECT * FROM pg_mv_cpu_usage()").await;
        assert!(result.is_ok());

        let rows = result.unwrap();
        if rows.len() > 0 {
            let first_row = &rows[0];
            assert!(first_row.contains_key("view_name"));
            assert!(first_row.contains_key("cpu_percent"));
            assert!(first_row.contains_key("max_cpu_percent"));
        }
    }

    #[tokio::test]
    async fn test_pg_vector_index_stats_view() {
        let (engine, _temp) = setup_test_db().await;

        // Create table with vector index
        engine.execute_sql(r#"
            CREATE TABLE documents (id SERIAL, embedding VECTOR(768));
            CREATE INDEX vec_idx ON documents USING hnsw (embedding vector_cosine_ops);
        "#).await.unwrap();

        // Insert some vectors
        for i in 0..100 {
            engine.execute_sql(&format!(
                "INSERT INTO documents (embedding) VALUES ('{}')",
                generate_random_vector_string(768)
            )).await.unwrap();
        }

        // Query index stats
        let result = engine.execute_sql("SELECT * FROM pg_vector_index_stats('vec_idx')").await;
        assert!(result.is_ok());

        let rows = result.unwrap();
        assert_eq!(rows.len(), 1);

        let stats = &rows[0];
        assert!(stats.contains_key("index_name"));
        assert!(stats.contains_key("table_name"));
        assert!(stats.contains_key("vector_count"));
        assert!(stats.contains_key("memory_bytes"));
        assert!(stats.contains_key("quantization"));
        assert!(stats.contains_key("compression_ratio"));
    }

    #[tokio::test]
    async fn test_pg_vector_index_stats_with_pq() {
        let (engine, _temp) = setup_test_db().await;

        // Create table with PQ index
        engine.execute_sql(r#"
            CREATE TABLE docs (id SERIAL, embedding VECTOR(768));
            CREATE INDEX vec_idx_pq ON docs
            USING hnsw (embedding vector_cosine_ops)
            WITH (quantization = 'product', pq_subquantizers = 8);
        "#).await.unwrap();

        // Insert vectors
        for i in 0..1000 {
            engine.execute_sql(&format!(
                "INSERT INTO docs (embedding) VALUES ('{}')",
                generate_random_vector_string(768)
            )).await.unwrap();
        }

        // Query PQ stats
        let result = engine.execute_sql("SELECT * FROM pg_vector_index_stats('vec_idx_pq')").await;
        assert!(result.is_ok());

        let stats = &result.unwrap()[0];
        assert_eq!(stats.get("quantization").unwrap(), "product");

        // Verify compression ratio
        let compression_ratio: f64 = stats.get("compression_ratio").unwrap().parse().unwrap();
        assert!(compression_ratio >= 8.0, "PQ should achieve 8x+ compression");
        assert!(compression_ratio <= 20.0, "Compression ratio should be reasonable");
    }

    #[tokio::test]
    async fn test_pg_compression_stats_view() {
        let (engine, _temp) = setup_test_db().await;

        // Create table with compression
        engine.execute_sql(r#"
            CREATE TABLE metrics (
                timestamp TIMESTAMP,
                value DOUBLE PRECISION,
                description TEXT
            ) WITH (compression = true);
        "#).await.unwrap();

        // Insert data
        for i in 0..1000 {
            engine.execute_sql(&format!(
                "INSERT INTO metrics VALUES (NOW(), {}.5, 'metric {}')",
                i, i
            )).await.unwrap();
        }

        // Query compression stats
        let result = engine.execute_sql(r#"
            SELECT * FROM pg_compression_stats WHERE table_name = 'metrics'
        "#).await;
        assert!(result.is_ok());

        let rows = result.unwrap();
        assert!(rows.len() > 0);

        // Verify stats structure
        let first_row = &rows[0];
        assert!(first_row.contains_key("table_name"));
        assert!(first_row.contains_key("column_name"));
        assert!(first_row.contains_key("compression_codec"));
        assert!(first_row.contains_key("compression_ratio"));
        assert!(first_row.contains_key("uncompressed_bytes"));
        assert!(first_row.contains_key("compressed_bytes"));
    }

    #[tokio::test]
    async fn test_pg_mv_dependencies_view() {
        let (engine, _temp) = setup_test_db().await;

        // Create tables and dependent MVs
        engine.execute_sql(r#"
            CREATE TABLE orders (id INT, user_id INT, amount DECIMAL);
            CREATE TABLE users (id INT, name TEXT);

            CREATE MATERIALIZED VIEW user_totals AS
            SELECT user_id, SUM(amount) as total FROM orders GROUP BY user_id;

            CREATE MATERIALIZED VIEW user_summary AS
            SELECT u.name, ut.total
            FROM users u
            JOIN user_totals ut ON u.id = ut.user_id;
        "#).await.unwrap();

        // Query dependencies
        let result = engine.execute_sql("SELECT * FROM pg_mv_dependencies()").await;
        assert!(result.is_ok());

        let rows = result.unwrap();
        assert!(rows.len() > 0);

        // Verify user_summary depends on user_totals
        let user_summary_dep = rows.iter().find(|r| {
            r.get("view_name").unwrap() == "user_summary" &&
            r.get("depends_on").unwrap() == "user_totals"
        });
        assert!(user_summary_dep.is_some(), "Should detect MV dependency");
    }

    // Helper function to generate random vector string
    fn generate_random_vector_string(dim: usize) -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();

        let values: Vec<String> = (0..dim)
            .map(|_| format!("{:.6}", rng.gen::<f32>()))
            .collect();

        format!("[{}]", values.join(","))
    }
}
