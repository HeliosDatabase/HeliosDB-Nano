//! CodeGraph's optional indexes must not enable generic branch index DDL.
#![cfg(feature = "code-graph")]

use heliosdb_nano::{
    code_graph::CodeIndexOptions, Config, EmbeddedDatabase, Error, Result, BRANCH_SCHEMA_UNSUPPORTED_MARKER,
};

#[test]
fn unsupported_branch_index_still_aborts_until_rollback() -> Result<()> {
    for history in [false, true] {
        let mut config = Config::default();
        config.storage.memory_only = true;
        config.storage.path = None;
        config.storage.time_travel_enabled = history;
        config.storage.version_gc_interval_secs = Some(0);
        config.session.lock_timeout_ms = 100;
        let db = EmbeddedDatabase::with_config(config)?;
        db.execute("CREATE TABLE cg_src (path TEXT PRIMARY KEY, lang TEXT, content TEXT)")?;
        assert_eq!(db.code_index(CodeIndexOptions::for_table("cg_src"))?.files_parsed, 0);
        db.create_branch("cg_child")?;
        db.switch_branch("cg_child")?;
        assert_eq!(db.current_branch(), "cg_child");

        db.begin()?;
        let error = db
            .execute("CREATE INDEX IF NOT EXISTS idx_hdb_code_symbols_file_id ON _hdb_code_symbols(file_id)")
            .err()
            .ok_or_else(|| Error::storage("generic branch CREATE INDEX unexpectedly succeeded"))?;
        assert!(matches!(&error, Error::QueryExecution(_)), "{error}");
        let message = error.to_string();
        assert!(message.contains(BRANCH_SCHEMA_UNSUPPORTED_MARKER), "{message}");
        assert!(message.contains("CREATE INDEX"), "{message}");
        assert!(message.contains("cg_child"), "{message}");
        let aborted = db
            .query("SELECT path FROM cg_src", &[])
            .err()
            .ok_or_else(|| Error::storage("refused branch DDL did not abort its transaction"))?;
        assert!(matches!(&aborted, Error::Transaction(_)), "{aborted}");
        assert!(aborted.to_string().contains("current transaction is aborted"));
        db.rollback()?;

        assert!(!db.in_transaction());
        assert!(db.query("SELECT path FROM cg_src", &[])?.is_empty());
        // A fresh caller transaction remains usable after the explicit rollback.
        db.begin()?;
        db.execute("INSERT INTO cg_src VALUES ('rolled-back.rs', 'rust', 'pub fn f() {}')")?;
        assert_eq!(db.query("SELECT path FROM cg_src", &[])?.len(), 1);
        db.rollback()?;
        assert!(db.query("SELECT path FROM cg_src", &[])?.is_empty());
        db.switch_branch("main")?;
        assert!(db.query("SELECT path FROM cg_src", &[])?.is_empty());
        eprintln!("codegraph-branch-ddl-control history={history} unsupported=true aborted=true rollback=true main_unchanged=true");
    }
    Ok(())
}

/// v09.3: the graph-rag tables gain their read indexes on main only; on a
/// branch `ensure_tables` must not issue the refused CREATE INDEX, which would
/// abort the caller's transaction.
#[cfg(feature = "graph-rag")]
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
fn graph_tables_on_branch_do_not_issue_create_index() -> Result<()> {
    let mut config = Config::default();
    config.storage.memory_only = true;
    config.storage.path = None;
    config.session.lock_timeout_ms = 100;
    let db = EmbeddedDatabase::with_config(config)?;
    db.create_branch("g_child")?;
    db.switch_branch("g_child")?;
    db.begin()?;
    heliosdb_nano::graph_rag::ensure_tables(&db)?;
    // The transaction was not aborted by a refused index statement.
    assert_eq!(db.query("SELECT node_id FROM _hdb_graph_nodes", &[])?.len(), 0);
    db.commit()?;
    db.switch_branch("main")?;
    Ok(())
}
