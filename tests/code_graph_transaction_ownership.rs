//! The indexer's SQL cleanup and batched graph rows must share one owner.
#![cfg(feature = "code-graph")]

use heliosdb_nano::{code_graph::CodeIndexOptions, Config, EmbeddedDatabase, Error, Result, Tuple, Value};

const BODY: &str = "pub fn callee() {}\npub fn caller() { callee(); }\n";
const TABLES: [&str; 3] = ["_hdb_code_files", "_hdb_code_symbols", "_hdb_code_symbol_refs"];

fn db(history: bool) -> Result<EmbeddedDatabase> {
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.path = None;
    config.storage.time_travel_enabled = history;
    config.storage.version_gc_interval_secs = Some(0);
    // A bounded reproduction of the actual 30-second self-lock. Correct
    // same-owner execution never waits for this timeout.
    config.session.lock_timeout_ms = 100;
    let db = EmbeddedDatabase::with_config(config)?;
    db.execute("CREATE TABLE cg_src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
    db.execute("CREATE TABLE cg_user (id INT PRIMARY KEY)")?;
    // Empty input bootstraps the real FK-bearing graph schema without rows.
    assert_eq!(db.code_index(CodeIndexOptions::for_table("cg_src"))?.files_parsed, 0);
    Ok(db)
}

fn source(db: &EmbeddedDatabase) -> Result<()> {
    db.execute_params_returning(
        "INSERT INTO cg_src VALUES ($1, 'rust', $2)",
        &[Value::String("a.rs".into()), Value::String(BODY.into())],
    )?;
    Ok(())
}

fn number(value: Option<&Value>) -> Result<i64> {
    match value {
        Some(Value::Int4(n)) => Ok(i64::from(*n)),
        Some(Value::Int8(n)) => Ok(*n),
        _ => Err(Error::storage("expected one integer result")),
    }
}

fn count(db: &EmbeddedDatabase, table: &str) -> Result<i64> {
    let rows = db.query(&format!("SELECT COUNT(*) FROM {table}"), &[])?;
    number(rows.first().and_then(|row| row.values.first()))
}

fn image(db: &EmbeddedDatabase) -> Result<Vec<Vec<Tuple>>> {
    TABLES
        .iter()
        .map(|table| db.query(&format!("SELECT * FROM {table} ORDER BY 1"), &[]))
        .collect()
}

fn complete(db: &EmbeddedDatabase) -> Result<()> {
    assert_eq!(count(db, "_hdb_code_files")?, 1);
    let rows = db.query("SELECT node_id, name FROM _hdb_code_symbols ORDER BY name", &[])?;
    assert_eq!(rows.len(), 2);
    let callee = rows.first().ok_or_else(|| Error::storage("missing callee"))?;
    let caller = rows.get(1).ok_or_else(|| Error::storage("missing caller"))?;
    assert_eq!(callee.values.get(1), Some(&Value::String("callee".into())));
    assert_eq!(caller.values.get(1), Some(&Value::String("caller".into())));
    let callee_id = number(callee.values.first())?;
    let caller_id = number(caller.values.first())?;
    let refs = db.query(
        "SELECT from_symbol, to_symbol FROM _hdb_code_symbol_refs WHERE to_name = 'callee'",
        &[],
    )?;
    assert_eq!(refs.len(), 1, "the call must resolve to its matching inserted symbol");
    let edge = refs.first().ok_or_else(|| Error::storage("missing call edge"))?;
    assert_eq!(number(edge.values.first())?, caller_id);
    assert_eq!(number(edge.values.get(1))?, callee_id);
    Ok(())
}

fn seed_old_graph(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("INSERT INTO _hdb_code_files (node_id, source_table, path, lang, sha256) VALUES (1, 'cg_src', 'a.rs', 'rust', 'old')")?;
    db.execute("INSERT INTO _hdb_code_symbols (node_id, file_id, name, qualified, kind) VALUES (101, 1, 'old', 'old', 'function')")?;
    db.execute("INSERT INTO _hdb_code_symbol_refs (edge_id, file_id, from_symbol, to_symbol, to_name, kind, resolution) VALUES (201, 1, 101, 101, 'old', 'call', 'exact')")?;
    Ok(())
}

#[test]
fn a_cold_public_index_joins_its_own_fk_cleanup_transaction() -> Result<()> {
    for history in [false, true] {
        let db = db(history)?;
        source(&db)?;
        eprintln!("codegraph-cold-premise history={history} source_rows=1 graph_rows=0 lock_timeout_ms=100");
        let stats = db.code_index(CodeIndexOptions::for_table("cg_src"))?;
        assert_eq!(stats.files_parsed, 1);
        assert_eq!(stats.symbols_written, 2);
        assert!(stats.refs_written > 0);
        complete(&db)?;
        assert!(!db.in_transaction(), "self-managed chunk must finish its owner");
        eprintln!("codegraph-transaction-cell cold history={history}");
    }
    Ok(())
}

#[test]
fn a_caller_transaction_owns_graph_commit_and_rollback() -> Result<()> {
    for history in [false, true] {
        for commit in [false, true] {
            let db = db(history)?;
            source(&db)?;
            db.begin()?;
            db.execute("INSERT INTO cg_user VALUES (7)")?;
            let stats = db.code_index(CodeIndexOptions::for_table("cg_src"))?;
            assert_eq!(stats.files_parsed, 1);
            assert!(
                db.in_transaction(),
                "manage_txn=false must leave the caller's owner open"
            );
            complete(&db)?;
            if commit {
                db.commit()?;
            } else {
                db.rollback()?;
            }
            assert_eq!(count(&db, "cg_user")?, if commit { 1 } else { 0 });
            if commit {
                complete(&db)?;
            } else {
                for table in TABLES {
                    assert_eq!(count(&db, table)?, 0, "a raw graph row escaped rollback");
                }
            }
            eprintln!("codegraph-transaction-cell caller history={history} commit={commit}");
        }
    }
    Ok(())
}

#[test]
fn a_late_reference_refusal_restores_old_graph_rows_and_deletes() -> Result<()> {
    for history in [false, true] {
        for caller in [false, true] {
            let db = db(history)?;
            source(&db)?;
            seed_old_graph(&db)?;
            // Symbol INSERTs precede reference INSERTs in the real indexer.
            // Refuse the real call edge only after new symbols were staged.
            db.execute("ALTER TABLE _hdb_code_symbol_refs ADD CONSTRAINT cg_ref_refusal CHECK (to_name <> 'callee')")?;
            let before = image(&db)?;
            if caller {
                db.begin()?;
                db.execute("INSERT INTO cg_user VALUES (7)")?;
            }
            let error = db
                .code_index(CodeIndexOptions::for_table("cg_src"))
                .err()
                .ok_or_else(|| Error::storage("reference CHECK unexpectedly accepted the edge"))?;
            assert!(
                error.to_string().to_ascii_lowercase().contains("check"),
                "the late constraint must be reached: {error}"
            );
            if caller {
                db.rollback()?;
            }
            assert_eq!(
                image(&db)?,
                before,
                "failed chunk leaked symbols or lost its staged stale-row deletes"
            );
            assert_eq!(count(&db, "cg_user")?, 0);
            assert!(!db.in_transaction());
            db.execute("ALTER TABLE _hdb_code_symbol_refs DROP CONSTRAINT cg_ref_refusal")?;
            db.code_index(CodeIndexOptions::for_table("cg_src"))?;
            complete(&db)?;
            eprintln!("codegraph-transaction-cell late-refusal history={history} caller={caller}");
        }
    }
    Ok(())
}

#[test]
fn a_branch_index_keeps_rows_and_rollback_on_its_own_branch() -> Result<()> {
    for history in [false, true] {
        let db = db(history)?;
        db.create_branch("cg_child")?;
        db.switch_branch("cg_child")?;
        source(&db)?;
        db.begin()?;
        db.code_index(CodeIndexOptions::for_table("cg_src"))?;
        complete(&db)?;
        db.rollback()?;
        for table in TABLES {
            assert_eq!(count(&db, table)?, 0);
        }
        db.code_index(CodeIndexOptions::for_table("cg_src"))?;
        complete(&db)?;
        db.switch_branch("main")?;
        assert_eq!(count(&db, "cg_src")?, 0);
        for table in TABLES {
            assert_eq!(count(&db, table)?, 0, "branch graph row reached main");
        }
        db.switch_branch("cg_child")?;
        complete(&db)?;
        eprintln!("codegraph-transaction-cell branch history={history}");
    }
    Ok(())
}
