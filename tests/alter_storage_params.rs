//! SET STORAGE uses the same complete plan and atomic rewrite on both families.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{ColumnStorageMode, Config, EmbeddedDatabase, Value, DDL_OVER_STAGED_ROWS_MARKER};
use rocksdb::{IteratorMode, WriteBatchIterator};
use std::sync::Arc;
use std::time::Duration;

fn mode(db: &EmbeddedDatabase, table: &str, column: &str) -> ColumnStorageMode {
    db.storage
        .catalog()
        .get_table_schema(table)
        .unwrap()
        .columns
        .iter()
        .find(|candidate| candidate.name == column)
        .unwrap()
        .storage_mode
}

fn image(db: &EmbeddedDatabase) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.storage
        .db()
        .iterator(IteratorMode::Start)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

#[derive(Default)]
struct Keys(Vec<Vec<u8>>);

impl WriteBatchIterator for Keys {
    fn put(&mut self, key: Box<[u8]>, _value: Box<[u8]>) {
        self.0.push(key.to_vec());
    }

    fn delete(&mut self, key: Box<[u8]>) {
        self.0.push(key.to_vec());
    }
}

#[test]
fn params_storage_rewrites_publish_rows_and_schema_in_one_group() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.path = Some(directory.path().to_path_buf());
    config.storage.memory_only = false;
    config.replication.physical.enabled = true;
    config.storage.wal_checkpoint_interval_entries = 0;
    config.storage.wal_checkpoint_interval_secs = 0;
    let db = EmbeddedDatabase::with_config(config).unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
    let payload = "large logical payload ".repeat(200);
    db.execute_params(
        "INSERT INTO s VALUES ($1, $2)",
        &[Value::Int4(1), Value::String(payload.clone())],
    )
    .unwrap();
    let expected = vec![vec![Value::Int4(1), Value::String(payload)]];
    for (spelling, expected_mode) in [
        ("DICTIONARY", ColumnStorageMode::Dictionary),
        ("CONTENT_ADDRESSED", ColumnStorageMode::ContentAddressed),
        ("COLUMNAR", ColumnStorageMode::Columnar),
        ("DEFAULT", ColumnStorageMode::Default),
    ] {
        let before = db.storage.db().latest_sequence_number();
        db.execute_params(&format!("ALTER TABLE s ALTER COLUMN v SET STORAGE {spelling}"), &[])
            .unwrap();
        assert_eq!(mode(&db, "s", "v"), expected_mode);
        let rows = db.query_params("SELECT id, v FROM s", &[]).unwrap();
        assert_eq!(rows.into_iter().map(|row| row.values).collect::<Vec<_>>(), expected);
        assert!(db.storage.db().get(b"data:s:1").unwrap().is_some());
        assert!(db.storage.db().get(b"data:s:2").unwrap().is_none());
        let mut updates = db.storage.db().get_updates_since(before).unwrap();
        let groups = updates
            .by_ref()
            .map(|entry| {
                let (_, batch) = entry.unwrap();
                let mut keys = Keys::default();
                batch.iterate(&mut keys);
                keys.0
            })
            .filter(|keys| {
                keys.iter()
                    .any(|key| key.as_slice() == b"meta:table:s" || key.starts_with(b"data:s:"))
            })
            .collect::<Vec<_>>();
        updates.status().unwrap();
        assert_eq!(groups.len(), 1, "{spelling}: the rewrite is one durable group");
        assert!(groups[0].iter().any(|key| key.as_slice() == b"meta:table:s"));
        assert!(groups[0].iter().any(|key| key.starts_with(b"data:s:")));
    }
}

#[test]
fn both_families_resolve_quoted_storage_targets_with_tokenized_whitespace_and_comments() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute(r#"CREATE TABLE "MiX""ed" (id INT PRIMARY KEY, "VaL" TEXT)"#)
            .unwrap();
        db.execute(r#"INSERT INTO "MiX""ed" VALUES (1, 'logical')"#).unwrap();
        let statement = "/* leading */ ALTER\nTABLE public.\"MiX\"\"ed\" ALTER /* between */ COLUMN \"VaL\" \
                         SET\nSTORAGE DICTIONARY; -- trailing";
        if params {
            db.execute_params(statement, &[]).unwrap();
        } else {
            db.execute(statement).unwrap();
        }
        assert_eq!(mode(&db, "MiX\"ed", "VaL"), ColumnStorageMode::Dictionary);
        assert_eq!(
            db.query(r#"SELECT "VaL" FROM "MiX""ed" WHERE id = 1"#, &[]).unwrap()[0].values,
            vec![Value::String("logical".into())]
        );
    }
}

#[test]
fn params_storage_planning_uses_each_sessions_search_path() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'public')").unwrap();
    db.execute("CREATE SCHEMA tenant").unwrap();
    db.execute("CREATE TABLE tenant.s (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO tenant.s VALUES (2, 'tenant')").unwrap();
    let tenant = db.create_wire_session("tenant_client").unwrap();
    let public = db.create_wire_session("public_client").unwrap();
    db.execute_params_for_session(tenant, "SET search_path TO tenant, public", &[])
        .unwrap();
    db.execute_params_for_session(tenant, "ALTER TABLE s ALTER COLUMN v SET STORAGE DICTIONARY", &[])
        .unwrap();
    assert_eq!(mode(&db, "tenant.s", "v"), ColumnStorageMode::Dictionary);
    assert_eq!(mode(&db, "s", "v"), ColumnStorageMode::Default);
    db.execute_params_for_session(public, "ALTER TABLE s ALTER COLUMN v SET STORAGE COLUMNAR", &[])
        .unwrap();
    assert_eq!(mode(&db, "s", "v"), ColumnStorageMode::Columnar);
    assert_eq!(
        db.query_params_for_session(tenant, "SELECT v FROM s", &[]).unwrap()[0].values,
        vec![Value::String("tenant".into())]
    );
    assert_eq!(
        db.query_params_for_session(public, "SELECT v FROM s", &[]).unwrap()[0].values,
        vec![Value::String("public".into())]
    );
}

#[test]
fn refused_storage_clause_lists_and_trailing_input_leave_every_byte_unchanged() {
    for params in [false, true] {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
        db.execute("INSERT INTO s VALUES (1, 'original')").unwrap();
        for statement in [
            "ALTER TABLE s ALTER COLUMN v SET STORAGE UNKNOWN",
            "ALTER TABLE s ALTER COLUMN v SET STORAGE DICTIONARY, ADD COLUMN extra INT",
            "ALTER TABLE s ADD COLUMN extra INT, ALTER COLUMN v SET STORAGE DICTIONARY",
            "ALTER TABLE s ALTER COLUMN v SET STORAGE DICTIONARY; DELETE FROM s",
        ] {
            let before = image(&db);
            let sequence = db.storage.db().latest_sequence_number();
            let result = if params {
                db.execute_params(statement, &[])
            } else {
                db.execute(statement)
            };
            assert!(result.is_err(), "{statement}: must refuse before writing");
            assert_eq!(image(&db), before, "{statement}");
            assert_eq!(db.storage.db().latest_sequence_number(), sequence, "{statement}");
        }
        // Ordinary supported multi-ALTER still reaches its one-group planner.
        db.execute_params("ALTER TABLE s ADD COLUMN extra INT DEFAULT 7, DROP COLUMN v", &[])
            .unwrap();
        assert_eq!(
            db.query_params("SELECT id, extra FROM s", &[]).unwrap()[0].values,
            vec![Value::Int4(1), Value::Int4(7)]
        );
    }
}

#[test]
fn params_storage_refuses_own_staged_rows_and_the_session_aborts() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'original')").unwrap();
    let session = db.create_wire_session("writer").unwrap();
    db.execute_params_for_session(session, "BEGIN", &[]).unwrap();
    db.execute_params_for_session(
        session,
        "UPDATE s SET v = $1 WHERE id = 1",
        &[Value::String("staged".into())],
    )
    .unwrap();
    let error = db
        .execute_params_for_session(session, "ALTER TABLE s ALTER COLUMN v SET STORAGE DICTIONARY", &[])
        .unwrap_err();
    assert!(error.to_string().contains(DDL_OVER_STAGED_ROWS_MARKER), "{error}");
    let aborted = db.execute_params_for_session(session, "COMMIT", &[]).unwrap_err();
    assert!(matches!(aborted, heliosdb_nano::Error::Transaction(_)), "{aborted}");
    assert!(
        aborted.to_string().contains("current transaction is aborted"),
        "{aborted}"
    );
    assert_eq!(mode(&db, "s", "v"), ColumnStorageMode::Default);
    assert_eq!(
        db.query_params("SELECT v FROM s", &[]).unwrap()[0].values,
        vec![Value::String("original".into())]
    );
    db.execute_params_for_session(session, "BEGIN", &[]).unwrap();
    db.execute_params_for_session(session, "UPDATE s SET v = 'retry' WHERE id = 1", &[])
        .unwrap();
    db.execute_params_for_session(session, "COMMIT", &[]).unwrap();
    assert_eq!(
        db.query_params("SELECT v FROM s", &[]).unwrap()[0].values,
        vec![Value::String("retry".into())]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wire_named_prepared_storage_rewrites_preserve_values_and_session_resolution() {
    use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};

    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'public')").unwrap();
    db.execute("CREATE SCHEMA tenant").unwrap();
    db.execute("CREATE TABLE tenant.s (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO tenant.s VALUES (2, 'tenant')").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = PgServer::new(PgServerConfig::with_address(address), Arc::clone(&db)).unwrap();
    let serving = tokio::spawn(async move { server.serve_with_listener(listener).await });
    tokio::time::timeout(Duration::from_secs(20), async {
        let connection_string = format!("host=127.0.0.1 port={} user=postgres", address.port());
        let (client, connection) = tokio_postgres::connect(&connection_string, tokio_postgres::NoTls)
            .await
            .unwrap();
        let connection = tokio::spawn(connection);
        client.execute("SET search_path TO tenant, public", &[]).await.unwrap();
        for (spelling, expected_mode) in [
            ("DICTIONARY", ColumnStorageMode::Dictionary),
            ("CONTENT_ADDRESSED", ColumnStorageMode::ContentAddressed),
            ("COLUMNAR", ColumnStorageMode::Columnar),
            ("DEFAULT", ColumnStorageMode::Default),
        ] {
            let before_parse = image(&db);
            let sequence = db.storage.db().latest_sequence_number();
            let statement = client
                .prepare(&format!("ALTER TABLE s ALTER COLUMN v SET STORAGE {spelling}"))
                .await
                .unwrap();
            assert!(statement.columns().is_empty(), "SET STORAGE describes NoData");
            assert!(statement.params().is_empty(), "SET STORAGE has no parameters");
            assert_eq!(image(&db), before_parse, "Parse/Describe cannot rewrite storage");
            assert_eq!(db.storage.db().latest_sequence_number(), sequence);
            client.execute(&statement, &[]).await.unwrap();
            let rows = client.query("SELECT v FROM s WHERE id = 2", &[]).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get::<_, String>(0), "tenant");
            assert_eq!(mode(&db, "tenant.s", "v"), expected_mode);
            assert_eq!(mode(&db, "s", "v"), ColumnStorageMode::Default);
        }
        let before_parse = image(&db);
        let error = client
            .prepare("ALTER TABLE s ALTER COLUMN v SET STORAGE $1")
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(&tokio_postgres::error::SqlState::SYNTAX_ERROR));
        assert_eq!(image(&db), before_parse, "invalid custom grammar cannot write");
        // Ordinary DDL may be prepared before its target is created. Syntax
        // succeeds at Parse; Execute plans under the current session/layout.
        let future = client
            .prepare("ALTER TABLE future_storage ALTER COLUMN v SET STORAGE DICTIONARY")
            .await
            .unwrap();
        assert_eq!(image(&db), before_parse);
        client
            .execute("CREATE TABLE future_storage (id INT PRIMARY KEY, v TEXT)", &[])
            .await
            .unwrap();
        client
            .execute("INSERT INTO future_storage VALUES (1, 'future')", &[])
            .await
            .unwrap();
        client.execute(&future, &[]).await.unwrap();
        assert_eq!(mode(&db, "tenant.future_storage", "v"), ColumnStorageMode::Dictionary);
        let rows = client
            .query("SELECT v FROM future_storage WHERE id = 1", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, String>(0), "future");
        drop(client);
        connection.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    serving.abort();
}
