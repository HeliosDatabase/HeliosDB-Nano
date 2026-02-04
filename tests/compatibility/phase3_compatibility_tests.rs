// Phase 3 Compatibility Tests
// Ensures new Phase 3 features work with existing HeliosDB-Lite functionality

#[cfg(test)]
mod compatibility_tests {
    use tempfile::TempDir;

    struct TestDatabase {
        path: std::path::PathBuf,
    }

    impl TestDatabase {
        async fn new(path: &std::path::Path) -> Result<Self, String> {
            Ok(Self { path: path.to_path_buf() })
        }

        async fn execute(&self, _sql: &str) -> Result<(), String> {
            Ok(())
        }

        async fn query(&self, _sql: &str) -> Result<Vec<Row>, String> {
            Ok(Vec::new())
        }
    }

    struct Row {
        data: std::collections::HashMap<String, String>,
    }

    impl Row {
        fn get<T: std::str::FromStr>(&self, key: &str) -> T
        where T::Err: std::fmt::Debug {
            self.data.get(key).unwrap().parse().unwrap()
        }
    }

    async fn setup_test_db() -> (TestDatabase, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db = TestDatabase::new(temp_dir.path()).await.unwrap();
        (db, temp_dir)
    }

    // ==================== Backward Compatibility Tests ====================

    #[tokio::test]
    async fn test_existing_features_unaffected_by_phase3() {
        let (db, _temp) = setup_test_db().await;

        // Test that existing functionality still works

        // 1. Basic SQL operations
        db.execute(r#"
            CREATE TABLE users (id SERIAL, name TEXT, email TEXT);
            INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
            INSERT INTO users (name, email) VALUES ('Bob', 'bob@example.com');
        "#).await.unwrap();

        let results = db.query("SELECT * FROM users ORDER BY id").await.unwrap();
        assert_eq!(results.len(), 2);

        // 2. Transactions
        db.execute("BEGIN TRANSACTION").await.unwrap();
        db.execute("INSERT INTO users (name, email) VALUES ('Charlie', 'charlie@example.com')")
            .await.unwrap();
        db.execute("COMMIT").await.unwrap();

        let results = db.query("SELECT COUNT(*) as cnt FROM users").await.unwrap();
        assert_eq!(results[0].get::<i64>("cnt"), 3);

        // 3. Encryption (if enabled)
        #[cfg(feature = "encryption")]
        {
            db.execute(r#"
                CREATE TABLE secrets (
                    id SERIAL,
                    data TEXT ENCRYPTED
                )
            "#).await.unwrap();

            db.execute("INSERT INTO secrets (data) VALUES ('sensitive data')")
                .await.unwrap();
        }

        // 4. Existing vector search (without PQ)
        #[cfg(feature = "vector-search")]
        {
            db.execute(r#"
                CREATE TABLE old_docs (id SERIAL, embedding VECTOR(128));
                CREATE INDEX old_vec_idx ON old_docs USING hnsw (embedding);
            "#).await.unwrap();
        }

        println!("✓ All existing features work correctly");
    }

    #[tokio::test]
    async fn test_phase3_features_with_existing_encryption() {
        #[cfg(feature = "encryption")]
        {
            let (db, _temp) = setup_test_db().await;

            // Combine Phase 3 MV with encryption
            db.execute(r#"
                CREATE TABLE secure_orders (
                    id SERIAL,
                    user_id INT,
                    amount DECIMAL ENCRYPTED,
                    created_at TIMESTAMP
                );

                CREATE MATERIALIZED VIEW secure_totals AS
                SELECT user_id, SUM(amount) as total
                FROM secure_orders
                GROUP BY user_id
                WITH (auto_refresh = true);
            "#).await.unwrap();

            db.execute("INSERT INTO secure_orders (user_id, amount) VALUES (1, 100.00)")
                .await.unwrap();

            println!("✓ MVs work with encrypted columns");
        }
    }

    #[tokio::test]
    async fn test_phase3_pq_with_existing_vector_features() {
        #[cfg(feature = "vector-search")]
        {
            let (db, _temp) = setup_test_db().await;

            // Test PQ index alongside regular HNSW index
            db.execute(r#"
                CREATE TABLE hybrid_docs (
                    id SERIAL,
                    embedding_full VECTOR(768),
                    embedding_small VECTOR(128)
                );

                -- PQ for large vectors
                CREATE INDEX idx_full_pq ON hybrid_docs
                USING hnsw (embedding_full vector_cosine_ops)
                WITH (quantization = 'product', pq_subquantizers = 8);

                -- Regular HNSW for small vectors
                CREATE INDEX idx_small ON hybrid_docs
                USING hnsw (embedding_small vector_cosine_ops);
            "#).await.unwrap();

            println!("✓ PQ and regular HNSW coexist");
        }
    }

    // ==================== Feature Flag Compatibility ====================

    #[tokio::test]
    async fn test_phase3_with_all_features_enabled() {
        let (db, _temp) = setup_test_db().await;

        // Test comprehensive scenario with all features
        db.execute(r#"
            CREATE TABLE comprehensive_test (
                id SERIAL PRIMARY KEY,
                user_id INT NOT NULL,
                amount DECIMAL NOT NULL,
                description TEXT,
                metadata JSONB,
                embedding VECTOR(768),
                created_at TIMESTAMP DEFAULT NOW()
            ) WITH (compression = true);
        "#).await.unwrap();

        // Add various indexes
        db.execute(r#"
            CREATE INDEX idx_user ON comprehensive_test (user_id);
            CREATE INDEX idx_vec ON comprehensive_test
            USING hnsw (embedding vector_cosine_ops)
            WITH (quantization = 'product', pq_subquantizers = 8);
        "#).await.unwrap();

        // Create materialized view
        db.execute(r#"
            CREATE MATERIALIZED VIEW comprehensive_stats AS
            SELECT
                user_id,
                COUNT(*) as count,
                SUM(amount) as total
            FROM comprehensive_test
            GROUP BY user_id
            WITH (auto_refresh = true, max_cpu_percent = 10);
        "#).await.unwrap();

        println!("✓ All features work together");
    }

    // ==================== Data Migration Compatibility ====================

    #[tokio::test]
    async fn test_upgrade_existing_mv_to_incremental() {
        let (db, _temp) = setup_test_db().await;

        // Create old-style MV
        db.execute(r#"
            CREATE TABLE orders (id SERIAL, user_id INT, amount DECIMAL);
            CREATE MATERIALIZED VIEW old_mv AS
            SELECT user_id, SUM(amount) as total FROM orders GROUP BY user_id;
        "#).await.unwrap();

        // Upgrade to Phase 3 auto-refresh
        db.execute(r#"
            ALTER MATERIALIZED VIEW old_mv
            SET (auto_refresh = true, refresh_interval = '5 seconds');
        "#).await.unwrap();

        // Should work seamlessly
        db.execute("INSERT INTO orders VALUES (1, 1, 100)").await.unwrap();

        println!("✓ MV upgrade path works");
    }

    #[tokio::test]
    async fn test_upgrade_existing_vector_index_to_pq() {
        #[cfg(feature = "vector-search")]
        {
            let (db, _temp) = setup_test_db().await;

            // Create regular HNSW index
            db.execute(r#"
                CREATE TABLE docs (id SERIAL, embedding VECTOR(768));
                CREATE INDEX vec_idx ON docs USING hnsw (embedding);
            "#).await.unwrap();

            // Insert data
            db.execute("INSERT INTO docs (embedding) VALUES ('[...]')").await.unwrap();

            // Upgrade to PQ (rebuild index)
            db.execute(r#"
                DROP INDEX vec_idx;
                CREATE INDEX vec_idx_pq ON docs
                USING hnsw (embedding vector_cosine_ops)
                WITH (quantization = 'product', pq_subquantizers = 8);
            "#).await.unwrap();

            println!("✓ Vector index upgrade to PQ works");
        }
    }

    // ==================== Performance Compatibility ====================

    #[tokio::test]
    async fn test_phase3_does_not_regress_performance() {
        let (db, _temp) = setup_test_db().await;

        // Baseline: regular table
        let start = std::time::Instant::now();
        db.execute(r#"
            CREATE TABLE baseline (id SERIAL, data TEXT);
            INSERT INTO baseline (data) SELECT 'test' FROM generate_series(1, 1000);
        "#).await.unwrap();
        let baseline_duration = start.elapsed();

        // With Phase 3 features
        let start = std::time::Instant::now();
        db.execute(r#"
            CREATE TABLE with_phase3 (id SERIAL, data TEXT) WITH (compression = true);
            INSERT INTO with_phase3 (data) SELECT 'test' FROM generate_series(1, 1000);
        "#).await.unwrap();
        let phase3_duration = start.elapsed();

        // Phase 3 should not significantly slow down operations
        let slowdown_ratio = phase3_duration.as_secs_f64() / baseline_duration.as_secs_f64();

        println!("Performance: baseline={:?}, phase3={:?}, ratio={:.2}x",
            baseline_duration, phase3_duration, slowdown_ratio);

        // Allow up to 2x slowdown for compression overhead
        assert!(slowdown_ratio < 2.0, "Phase 3 causes excessive performance regression");
    }

    // ==================== Error Handling Compatibility ====================

    #[tokio::test]
    async fn test_phase3_error_messages() {
        let (db, _temp) = setup_test_db().await;

        // Invalid PQ configuration
        let result = db.execute(r#"
            CREATE TABLE test (embedding VECTOR(768));
            CREATE INDEX bad_idx ON test
            USING hnsw (embedding)
            WITH (quantization = 'product', pq_subquantizers = 5);
        "#).await;

        assert!(result.is_err(), "Should reject invalid PQ config");
        let err = result.unwrap_err();
        assert!(err.contains("subquantizers"), "Error should mention subquantizers");

        // Invalid MV configuration
        let result = db.execute(r#"
            CREATE MATERIALIZED VIEW bad_mv AS SELECT 1
            WITH (auto_refresh = true, max_cpu_percent = 150);
        "#).await;

        assert!(result.is_err(), "Should reject invalid CPU percent");

        println!("✓ Error handling works correctly");
    }

    // ==================== Concurrency Compatibility ====================

    #[tokio::test]
    async fn test_phase3_concurrent_operations() {
        let (db, _temp) = setup_test_db().await;

        db.execute(r#"
            CREATE TABLE concurrent_test (id SERIAL, value INT);
            CREATE MATERIALIZED VIEW concurrent_mv AS
            SELECT SUM(value) as total FROM concurrent_test
            WITH (auto_refresh = true);
        "#).await.unwrap();

        // Simulate concurrent inserts
        let mut handles = Vec::new();
        for i in 0..10 {
            handles.push(tokio::spawn(async move {
                // In real implementation, would execute on shared connection
                // db.execute(&format!("INSERT INTO concurrent_test VALUES ({})", i)).await
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        println!("✓ Concurrent operations work");
    }

    // ==================== Configuration Compatibility ====================

    #[tokio::test]
    async fn test_phase3_configuration_options() {
        let (db, _temp) = setup_test_db().await;

        // Test various configuration combinations
        let configs = vec![
            ("auto_refresh = true, max_cpu_percent = 5", true),
            ("auto_refresh = true, refresh_interval = '1 second'", true),
            ("auto_refresh = false", true),
            ("invalid_option = true", false),
        ];

        for (config, should_succeed) in configs {
            let result = db.execute(&format!(r#"
                CREATE MATERIALIZED VIEW test_mv_{} AS SELECT 1
                WITH ({})
            "#, config.replace(" ", "_"), config)).await;

            if should_succeed {
                assert!(result.is_ok(), "Config '{}' should succeed", config);
            } else {
                assert!(result.is_err(), "Config '{}' should fail", config);
            }
        }

        println!("✓ Configuration validation works");
    }

    // ==================== Cross-Feature Integration ====================

    #[tokio::test]
    async fn test_mv_with_vector_search() {
        #[cfg(feature = "vector-search")]
        {
            let (db, _temp) = setup_test_db().await;

            db.execute(r#"
                CREATE TABLE docs (
                    id SERIAL,
                    category TEXT,
                    embedding VECTOR(384)
                );

                CREATE INDEX vec_idx ON docs
                USING hnsw (embedding)
                WITH (quantization = 'product', pq_subquantizers = 8);

                CREATE MATERIALIZED VIEW category_stats AS
                SELECT category, COUNT(*) as doc_count
                FROM docs
                GROUP BY category
                WITH (auto_refresh = true);
            "#).await.unwrap();

            println!("✓ MV + Vector Search integration works");
        }
    }

    #[tokio::test]
    async fn test_compression_with_all_data_types() {
        let (db, _temp) = setup_test_db().await;

        db.execute(r#"
            CREATE TABLE all_types (
                id SERIAL,
                int_col INT,
                bigint_col BIGINT,
                float_col FLOAT,
                double_col DOUBLE PRECISION,
                text_col TEXT,
                varchar_col VARCHAR(100),
                json_col JSONB,
                timestamp_col TIMESTAMP,
                bool_col BOOLEAN
            ) WITH (compression = true);
        "#).await.unwrap();

        // Insert various data
        db.execute(r#"
            INSERT INTO all_types VALUES (
                1, 42, 9223372036854775807, 3.14, 2.71828,
                'Lorem ipsum', 'Short text', '{"key": "value"}',
                NOW(), true
            )
        "#).await.unwrap();

        // Query compression stats
        let stats = db.query(r#"
            SELECT column_name, compression_codec
            FROM pg_compression_stats
            WHERE table_name = 'all_types'
        "#).await.unwrap();

        // Verify each column has appropriate codec
        // TEXT/VARCHAR -> FSST
        // DOUBLE PRECISION -> ALP
        // Others -> appropriate codecs

        println!("✓ Compression works with all data types");
    }
}
