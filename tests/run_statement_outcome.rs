//! `EmbeddedDatabase::run_statement`: the engine decides whether a statement
//! returned rows or an affected count, and names the columns of the rows.
//!
//! The shell and `execute_sql_script` used to guess from the first keyword; the
//! cases here are the ones the guess got wrong, in both directions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::{EmbeddedDatabase, StatementOutcome, Value};

fn db_with_table() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')").unwrap();
    db
}

/// The columns and rows of an outcome that must be `Rows`.
fn rows_of(outcome: StatementOutcome) -> (Vec<String>, Vec<Vec<Value>>) {
    match outcome {
        StatementOutcome::Rows { columns, rows } => (columns, rows.into_iter().map(|t| t.values).collect()),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn affected(outcome: StatementOutcome) -> u64 {
    match outcome {
        StatementOutcome::Affected(n) => n,
        other => panic!("expected an affected count, got {other:?}"),
    }
}

#[test]
fn parenthesised_union_returns_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let (columns, rows) = rows_of(db.run_statement("(SELECT 1 AS a) UNION (SELECT 2)").unwrap());
    assert_eq!(columns, vec!["a"]);
    assert_eq!(rows.len(), 2);
}

#[test]
fn values_returns_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let (_, rows) = rows_of(db.run_statement("VALUES (1, 2)").unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);
}

#[test]
fn with_select_returns_rows() {
    let db = db_with_table();
    let (columns, rows) = rows_of(
        db.run_statement("WITH x AS (SELECT id FROM t WHERE id = 2) SELECT id FROM x")
            .unwrap(),
    );
    assert_eq!(columns, vec!["id"]);
    assert_eq!(rows, vec![vec![Value::Int4(2)]]);
}

#[test]
fn leading_comment_does_not_hide_the_keyword() {
    let db = db_with_table();
    let (_, rows) = rows_of(db.run_statement("-- the rows\nSELECT id FROM t ORDER BY id").unwrap());
    assert_eq!(rows.len(), 2);
    let n = affected(db.run_statement("/* a change */ DELETE FROM t WHERE id = 1").unwrap());
    assert_eq!(n, 1);
}

#[test]
fn literal_with_the_word_returning_is_an_affected_count() {
    let db = db_with_table();
    let n = affected(db.run_statement("INSERT INTO t VALUES (9, 'x returning y')").unwrap());
    assert_eq!(n, 1);
    let n = affected(db.run_statement("INSERT INTO t VALUES (10, ' RETURNING id ')").unwrap());
    assert_eq!(n, 1);
    let n = affected(
        db.run_statement("UPDATE t SET v = 'now returning' WHERE id = 9")
            .unwrap(),
    );
    assert_eq!(n, 1);
}

#[test]
fn insert_returning_names_the_columns() {
    let db = db_with_table();
    let (columns, rows) = rows_of(
        db.run_statement("INSERT INTO t VALUES (3, 'three') RETURNING id AS x")
            .unwrap(),
    );
    assert_eq!(columns, vec!["x"]);
    assert_eq!(rows, vec![vec![Value::Int4(3)]]);

    let (columns, rows) = rows_of(
        db.run_statement("INSERT INTO t VALUES (4, 'four') RETURNING *")
            .unwrap(),
    );
    assert_eq!(columns, vec!["id", "v"]);
    assert_eq!(rows.len(), 1);
}

#[test]
fn update_and_delete_returning_name_the_columns() {
    let db = db_with_table();
    let (columns, rows) = rows_of(
        db.run_statement("UPDATE t SET v = 'uno' WHERE id = 1 RETURNING id AS the_id, v")
            .unwrap(),
    );
    assert_eq!(columns, vec!["the_id", "v"]);
    assert_eq!(rows.len(), 1);

    let (columns, rows) = rows_of(db.run_statement("DELETE FROM t WHERE id = 2 RETURNING id, v").unwrap());
    assert_eq!(columns, vec!["id", "v"]);
    assert_eq!(rows.len(), 1);
}

#[test]
fn returning_with_no_matching_row_is_rows_with_names_and_no_rows() {
    let db = db_with_table();
    let (columns, rows) = rows_of(
        db.run_statement("UPDATE t SET v = 'x' WHERE id = 99 RETURNING id")
            .unwrap(),
    );
    assert_eq!(columns, vec!["id"]);
    assert!(rows.is_empty());
}

#[test]
fn dml_without_returning_is_an_affected_count() {
    let db = db_with_table();
    assert_eq!(affected(db.run_statement("UPDATE t SET v = 'x'").unwrap()), 2);
    assert_eq!(affected(db.run_statement("DELETE FROM t WHERE id = 1").unwrap()), 1);
    assert_eq!(
        affected(db.run_statement("INSERT INTO t VALUES (7, 'seven')").unwrap()),
        1
    );
}

#[test]
fn prepared_select_returns_rows_and_prepared_insert_a_count() {
    let db = db_with_table();
    assert_eq!(
        affected(
            db.run_statement("PREPARE sel AS SELECT id, v FROM t WHERE id > $1")
                .unwrap()
        ),
        0
    );
    let (columns, rows) = rows_of(db.run_statement("EXECUTE sel(0)").unwrap());
    assert_eq!(columns, vec!["id", "v"]);
    assert_eq!(rows.len(), 2);

    db.run_statement("PREPARE ins AS INSERT INTO t VALUES ($1, $2)")
        .unwrap();
    assert_eq!(affected(db.run_statement("EXECUTE ins(5, 'five')").unwrap()), 1);
    let (_, rows) = rows_of(db.run_statement("SELECT id FROM t WHERE id = 5").unwrap());
    assert_eq!(rows.len(), 1);
}

#[test]
fn executing_an_unknown_prepared_statement_is_an_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let error = db.run_statement("EXECUTE nothing_prepared").unwrap_err();
    assert!(error.to_string().contains("does not exist"), "{error}");
}

#[test]
fn show_and_pragma_return_rows() {
    let db = db_with_table();
    let (_, rows) = rows_of(db.run_statement("SHOW BRANCHES").unwrap());
    assert!(!rows.is_empty());

    let (columns, rows) = rows_of(db.run_statement("SHOW search_path").unwrap());
    assert_eq!(columns.len(), 1, "{columns:?}");
    assert_eq!(rows.len(), 1);

    let (columns, rows) = rows_of(db.run_statement("PRAGMA table_info(t)").unwrap());
    assert_eq!(columns, vec!["cid", "name", "type", "notnull", "dflt_value", "pk"]);
    assert_eq!(rows.len(), 2);
}

#[test]
fn explain_returns_rows() {
    let db = db_with_table();
    let (_, rows) = rows_of(db.run_statement("EXPLAIN SELECT * FROM t").unwrap());
    assert!(!rows.is_empty());
}

#[cfg(feature = "ha-tier1")]
#[test]
fn cluster_status_family_returns_rows() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for sql in ["SHOW CLUSTER STATUS", "SHOW REPLICATION STATUS"] {
        let (columns, _) = rows_of(db.run_statement(sql).unwrap());
        assert_eq!(
            columns,
            vec!["node_id", "role", "address", "is_healthy", "lsn", "lag_ms", "priority"],
            "{sql}"
        );
    }
    // The topology answers with rows too (a handle that is no cluster member has none).
    let outcome = db.run_statement("SHOW TOPOLOGY").unwrap();
    assert!(matches!(outcome, StatementOutcome::Rows { .. }), "{outcome:?}");
}

#[test]
fn ddl_and_transaction_control_are_affected_counts() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    assert_eq!(affected(db.run_statement("CREATE TABLE u (id INT)").unwrap()), 0);
    assert_eq!(affected(db.run_statement("BEGIN").unwrap()), 0);
    assert_eq!(affected(db.run_statement("INSERT INTO u VALUES (1)").unwrap()), 1);
    assert_eq!(affected(db.run_statement("COMMIT").unwrap()), 0);
    assert_eq!(affected(db.run_statement("DROP TABLE u").unwrap()), 0);
}

#[test]
fn a_read_inside_a_transaction_sees_the_transactions_writes() {
    let db = db_with_table();
    db.run_statement("BEGIN").unwrap();
    db.run_statement("INSERT INTO t VALUES (9, 'nine')").unwrap();
    let (columns, rows) = rows_of(db.run_statement("SELECT id, v FROM t ORDER BY id").unwrap());
    assert_eq!(columns, vec!["id", "v"]);
    assert_eq!(rows.len(), 3);
    db.run_statement("ROLLBACK").unwrap();
    let (_, rows) = rows_of(db.run_statement("SELECT id FROM t").unwrap());
    assert_eq!(rows.len(), 2);
}

#[test]
fn branch_statements_run() {
    let db = db_with_table();
    db.run_statement("CREATE DATABASE BRANCH r FROM main AS OF NOW")
        .unwrap();
    db.run_statement("USE BRANCH r").unwrap();
    db.run_statement("CREATE TABLE only_on_r (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.run_statement("INSERT INTO only_on_r VALUES (7, 'seven')").unwrap();
    let (columns, rows) = rows_of(db.run_statement("SELECT id, v FROM only_on_r").unwrap());
    assert_eq!(columns, vec!["id", "v"]);
    assert_eq!(rows.len(), 1);
    db.run_statement("USE BRANCH main").unwrap();
    // An ordinary merge is a command; an ABORT, when it answers, answers with rows.
    if let Ok(outcome) = db.run_statement("MERGE BRANCH r ABORT") {
        assert!(matches!(outcome, StatementOutcome::Rows { .. }), "{outcome:?}");
    }
    assert!(matches!(
        db.run_statement("MERGE BRANCH r INTO main").unwrap(),
        StatementOutcome::Affected(_)
    ));
}

#[test]
fn execute_sql_script_runs_a_returning_statement_once() {
    let db = db_with_table();
    let executed = db
        .execute_sql_script("INSERT INTO t VALUES (3, 'three') RETURNING id; SELECT * FROM t; INSERT INTO t VALUES (4, 'x returning y');")
        .unwrap();
    assert_eq!(executed, 3);
    let (_, rows) = rows_of(db.run_statement("SELECT id FROM t").unwrap());
    assert_eq!(rows.len(), 4);
}

#[test]
fn unparsable_statements_and_unknown_tables_are_errors() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    assert!(db.run_statement("SELEC 1").is_err());
    assert!(db.run_statement("SELECT * FROM missing_table").is_err());
}

#[cfg(all(feature = "ha-tier1", target_os = "linux"))]
mod standby {
    use super::*;
    use heliosdb_nano::replication::checkpoint_transfer::{
        OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits,
    };
    use heliosdb_nano::replication::history::load_or_create_primary_history;
    use heliosdb_nano::storage::{StorageEngine, WriteGate};
    use heliosdb_nano::Config;
    use std::path::Path;
    use std::sync::Arc;
    use uuid::Uuid;

    fn primary_config(path: &Path) -> Config {
        let mut config = Config::default();
        config.storage.path = Some(path.to_path_buf());
        config.storage.memory_only = false;
        config.replication.physical.enabled = true;
        config
    }

    fn stage(source: &rocksdb::DB, history: Uuid, root: &Path, config: &Config) -> PhysicalStage {
        let limits = TransferLimits::default();
        let export = OnlineCheckpointExport::create(source, history, root, limits).unwrap();
        let mut receiver = ReceiverStaging::begin(
            &root.join("standby"),
            &export.manifest_bytes().unwrap(),
            history,
            limits,
        )
        .unwrap();
        for (index, file) in export.manifest().files.iter().enumerate() {
            let mut offset = 0;
            while offset < file.size {
                let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
                receiver.append_chunk(index as u32, offset, &bytes).unwrap();
                offset += bytes.len() as u64;
            }
        }
        receiver.finish_download(config).unwrap().open(config).unwrap()
    }

    #[test]
    fn a_write_through_run_statement_is_refused_on_a_physical_standby() {
        let root = tempfile::tempdir().unwrap();
        let config = primary_config(&root.path().join("primary"));
        let primary = EmbeddedDatabase::with_config(config.clone()).unwrap();
        primary.execute("CREATE TABLE s (id INT PRIMARY KEY)").unwrap();
        primary.execute("INSERT INTO s VALUES (1)").unwrap();
        let raw = primary.storage.db();
        let history = load_or_create_primary_history(raw.path()).unwrap();
        let stage = stage(&raw, history, root.path(), &config);
        let path = stage.staging_path().to_path_buf();
        let engine = Arc::new(
            StorageEngine::open_physical_standby(
                &path,
                &primary_config(&path),
                Arc::clone(stage.writer()),
                WriteGate::closed("physical standby"),
            )
            .unwrap(),
        );
        let database = EmbeddedDatabase::with_physical_standby(primary_config(&path), Arc::clone(&engine)).unwrap();

        // The read is served.
        let (_, rows) = rows_of(database.run_statement("SELECT id FROM s").unwrap());
        assert_eq!(rows.len(), 1);
        // The writes are refused, with the refusal `execute` gives.
        let refused = database.run_statement("INSERT INTO s VALUES (2)").unwrap_err();
        let by_execute = database.execute("INSERT INTO s VALUES (2)").unwrap_err();
        assert_eq!(refused.to_string(), by_execute.to_string());
        assert!(refused.to_string().to_lowercase().contains("standby"), "{refused}");
        assert!(database.run_statement("INSERT INTO s VALUES (3) RETURNING id").is_err());
        drop(database);
        drop(engine);
        drop(stage);
    }
}
