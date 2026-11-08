//! Custom MERGE BRANCH grammar is planned without effects and executes through
//! the same final-FK/policy funnel on params and named PG extended protocol.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::session::IsolationLevel;
use heliosdb_nano::{EmbeddedDatabase, Value};
use std::sync::Arc;
use std::time::Duration;
fn seed(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE p(id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO p VALUES(1)").unwrap();
    db.execute("CREATE TABLE c(id INT PRIMARY KEY,pid INT REFERENCES p(id) ON UPDATE CASCADE)")
        .unwrap();
    db.execute("INSERT INTO c VALUES(1,1)").unwrap();
    db.execute("CREATE DATABASE BRANCH work").unwrap();
    db.execute("USE BRANCH work").unwrap();
    db.execute("UPDATE p SET id=10 WHERE id=1").unwrap();
    db.execute("USE BRANCH main").unwrap();
}
fn values(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    db.query(&format!("SELECT * FROM {table} ORDER BY id"), &[])
        .unwrap()
        .into_iter()
        .map(|row| row.values)
        .collect()
}
#[test]
fn parameterized_session_merge_preserves_branch_binding_and_runtime_fk_policy() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    seed(&db);
    let session = db.create_session("merger", IsolationLevel::ReadCommitted).unwrap();
    db.execute_params_for_session(session, "MERGE DATABASE BRANCH work INTO main", &[])
        .unwrap();
    assert_eq!(values(&db, "p"), vec![vec![Value::Int4(10)]]);
    assert_eq!(values(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(10)]]);
    assert_eq!(
        db.query_params_for_session(session, "SELECT id FROM p", &[]).unwrap()[0].values,
        vec![Value::Int4(10)]
    );
    db.destroy_session(session).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn named_wire_merge_parse_has_no_effects_then_checks_final_fks_once_at_execute() {
    use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
    let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
    seed(&db);
    db.execute("INSERT INTO c VALUES(2,1)").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = PgServer::new(PgServerConfig::with_address(address), Arc::clone(&db)).unwrap();
    let serving = tokio::spawn(async move { server.serve_with_listener(listener).await });
    tokio::time::timeout(Duration::from_secs(30), async {
        let (client, connection) = tokio_postgres::connect(
            &format!("host=127.0.0.1 port={} user=postgres", address.port()),
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        let connection = tokio::spawn(connection);
        let sequence = db.storage.db().latest_sequence_number();
        let statement = client.prepare("MERGE BRANCH work INTO main").await.unwrap();
        assert!(statement.params().is_empty());
        assert!(statement.columns().is_empty());
        assert_eq!(
            db.storage.db().latest_sequence_number(),
            sequence,
            "Parse/Describe cannot create merge intent or land rows"
        );
        assert_eq!(values(&db, "p"), vec![vec![Value::Int4(1)]]);
        let error = client.execute(&statement, &[]).await.unwrap_err();
        assert_eq!(error.code().map(|code| code.code()), Some("23503"), "{error}");
        assert_eq!(
            db.storage.db().latest_sequence_number(),
            sequence,
            "genuine final-FK refusal is before intent"
        );
        assert_eq!(
            values(&db, "c"),
            vec![
                vec![Value::Int4(1), Value::Int4(1)],
                vec![Value::Int4(2), Value::Int4(1)]
            ]
        );
        client.execute("DELETE FROM c WHERE id=2", &[]).await.unwrap();
        assert_eq!(client.execute(&statement, &[]).await.unwrap(), 0);
        assert_eq!(values(&db, "p"), vec![vec![Value::Int4(10)]]);
        assert_eq!(values(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(10)]]);
        // Like ordinary DDL, Parse may precede the existence of a custom
        // grammar target; only Execute resolves and validates the real branch.
        let sequence = db.storage.db().latest_sequence_number();
        let future = client.prepare("MERGE BRANCH future INTO main").await.unwrap();
        assert_eq!(db.storage.db().latest_sequence_number(), sequence);
        db.execute("CREATE DATABASE BRANCH future").unwrap();
        db.execute("USE BRANCH future").unwrap();
        db.execute("UPDATE p SET id=20 WHERE id=10").unwrap();
        db.execute("USE BRANCH main").unwrap();
        client.execute(&future, &[]).await.unwrap();
        assert_eq!(values(&db, "p"), vec![vec![Value::Int4(20)]]);
        assert_eq!(values(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(20)]]);
        drop(client);
        connection.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    serving.abort();
}
