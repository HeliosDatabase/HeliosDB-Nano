//! Spec P1.1b, part 1: DDL on a primary leaves no catalog orphan and no
//! index that lags its rows.
//!
//! # Why
//!
//! A physical standby is built from a primary's checkpoint through strict
//! hydration (`EmbeddedDatabase::open_physical_snapshot`), which refuses a
//! catalog record that names something absent rather than guess. Three DDL
//! paths left exactly that, or indexes no reopen would build:
//!
//! * DROP TABLE left `table_constraints:{t}` (and the table's statistics,
//!   dictionaries and grants) behind: hydration refused every primary that
//!   had ever dropped a table with a PRIMARY KEY, and a new table of the same
//!   name took the dead constraint set over.
//! * RENAME COLUMN left the `meta:index:` definitions on the old name: the
//!   reopen registered those indexes over a column the schema no longer had,
//!   and hydration refused the store.
//! * MERGE BRANCH wrote main's rows below the index funnels: the primary kept
//!   answering PK lookups from its pre-merge indexes until it reopened.
//!
//! # What
//!
//! Each test drives the primary through SQL and compares it with what a
//! reopen (or the strict hydration of its checkpoint) builds from the same
//! bytes. Each is red before the fix; the doc of each says how.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::storage::ArtIndexManager;
use heliosdb_nano::{Config, EmbeddedDatabase, Value};
use rocksdb::checkpoint::Checkpoint;
use std::path::Path;

fn config(path: &Path) -> Config {
    let mut config = Config::default();
    config.storage.path = Some(path.to_path_buf());
    config.storage.memory_only = false;
    config
}

fn run(db: &EmbeddedDatabase, sql: &str) {
    db.execute(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .iter()
        .map(|row| match row.values.first() {
            Some(Value::Int4(v)) => i64::from(*v),
            Some(Value::Int8(v)) => *v,
            other => panic!("{sql}: expected an integer column, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// Checkpoint the live primary into `path` and hydrate the checkpoint
/// strictly: what a physical standby is built from.
fn hydrate_checkpoint(db: &EmbeddedDatabase, path: &Path) -> EmbeddedDatabase {
    Checkpoint::new(&db.storage.db())
        .unwrap()
        .create_checkpoint(path)
        .unwrap();
    EmbeddedDatabase::open_physical_snapshot(config(path))
        .unwrap_or_else(|e| panic!("strict hydration refused the primary's checkpoint: {e}"))
        .database
}

/// Every live key of a per-table family that belongs to `table` (and the
/// definition of its index `index`), read with a total-order iterator.
fn keys_of(db: &EmbeddedDatabase, table: &str, index: &str) -> Vec<String> {
    let mut exact: Vec<String> = [
        "meta:table:",
        "table_constraints:",
        "statistics:table:",
        "counter:",
        "compression:config:",
        "compression:stats:",
        "meta:identity:",
        "idxsnap:",
        "idxsnapv:",
        "meta:partchild:",
        "meta:partparent:",
        "colpm:",
    ]
    .iter()
    .map(|family| format!("{family}{table}"))
    .collect();
    exact.push(format!("meta:index:{index}"));
    let prefixes: Vec<String> = [
        "data:",
        "trigger:",
        "trigger_rowmut:",
        "dict:",
        "meta:col_storage:",
        "meta:acl:table:",
        "col:",
        "colz:",
        "colzm:",
        "colp:",
    ]
    .iter()
    .map(|family| format!("{family}{table}:"))
    .collect();
    let raw = db.storage.db();
    let mut read = rocksdb::ReadOptions::default();
    read.set_total_order_seek(true);
    raw.iterator_opt(rocksdb::IteratorMode::Start, read)
        .map(|entry| String::from_utf8_lossy(&entry.unwrap().0).into_owned())
        .filter(|key| exact.contains(key) || prefixes.iter().any(|prefix| key.starts_with(prefix.as_str())))
        .collect()
}

/// Every ART index of `table`, entry for entry, keyed by kind and columns.
fn census(db: &EmbeddedDatabase, table: &str) -> Vec<(String, Vec<String>, Vec<(Vec<u8>, u64)>)> {
    let art = db.storage.art_indexes();
    let mut census: Vec<_> = art
        .list_table_indexes(table)
        .into_iter()
        .map(|(name, kind, columns)| {
            let mut entries = art.index_range_scan(&name, None, None, None);
            entries.sort();
            (format!("{kind:?}"), columns, entries)
        })
        .collect();
    census.sort();
    census
}

/// DROP TABLE of a table with every kind of per-table record leaves none of
/// them, its checkpoint hydrates strictly, and a new table of the same name
/// starts with its own constraints only. Red before the fix: the constraint
/// record, the statistics, the dictionary and the grant survived the drop
/// (the first assertion after it), the hydration refused "constraints
/// reference missing table 'doomed'", and the new table inherited the dead
/// UNIQUE, CHECK and FOREIGN KEY records.
#[test]
fn drop_table_leaves_no_catalog_orphan_and_its_checkpoint_hydrates() {
    let root = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::with_config(config(&root.path().join("primary"))).unwrap();
    for sql in [
        "CREATE TABLE parent (id INT PRIMARY KEY)",
        "INSERT INTO parent VALUES (1)",
        "CREATE ROLE reader",
        "CREATE TABLE doomed (id INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email TEXT UNIQUE, qty INT, \
         pid INT REFERENCES parent(id), status TEXT, CONSTRAINT doomed_qty_pos CHECK (qty > 0))",
        "INSERT INTO doomed (email, qty, pid, status) VALUES ('a@example.com', 1, 1, 'open')",
        "INSERT INTO doomed (email, qty, pid, status) VALUES ('b@example.com', 2, 1, 'open')",
        "CREATE INDEX doomed_qty_idx ON doomed (qty)",
        // `dict:` is written by the storage-mode migration of a populated
        // column and by the inserts after it (tests/keyspace_wal_corpus.rs).
        "ALTER TABLE doomed ALTER COLUMN status SET STORAGE DICTIONARY",
        "INSERT INTO doomed (email, qty, pid, status) VALUES ('c@example.com', 3, 1, 'closed')",
        "GRANT SELECT ON doomed TO reader",
        "CREATE FUNCTION doomed_rewrite() RETURNS TRIGGER AS $$ BEGIN NEW.status = 'rewritten'; RETURN NEW; END $$ \
         LANGUAGE plpgsql",
        "CREATE TRIGGER doomed_trigger BEFORE INSERT ON doomed FOR EACH ROW EXECUTE FUNCTION doomed_rewrite()",
    ] {
        run(&db, sql);
    }
    db.storage.analyze_table("doomed").unwrap();
    let before = keys_of(&db, "doomed", "doomed_qty_idx");
    for family in [
        "table_constraints:doomed",
        "statistics:table:doomed",
        "meta:identity:doomed",
        "meta:index:doomed_qty_idx",
        "dict:doomed:status",
        "meta:acl:table:doomed:reader",
        "trigger:doomed:doomed_trigger",
    ] {
        assert!(
            before.iter().any(|key| key == family),
            "vacuity: {family} must exist before the drop, the store holds {before:?}"
        );
    }

    run(&db, "DROP TABLE doomed");
    assert_eq!(
        keys_of(&db, "doomed", "doomed_qty_idx"),
        Vec::<String>::new(),
        "DROP TABLE left catalog records of the dropped table behind"
    );
    let hydrated = hydrate_checkpoint(&db, &root.path().join("after-drop"));
    assert!(hydrated.query("SELECT * FROM doomed", &[]).is_err());
    assert_eq!(ids(&hydrated, "SELECT id FROM parent"), vec![1]);
    drop(hydrated);

    run(&db, "CREATE TABLE doomed (id INT PRIMARY KEY, note TEXT)");
    let constraints = db.storage.catalog().load_table_constraints("doomed").unwrap();
    let unique: Vec<&str> = constraints.unique_constraints.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(unique, vec!["doomed_id_pkey"], "the new table inherited UNIQUE records");
    assert!(
        constraints.check_constraints.is_empty(),
        "the new table inherited a CHECK"
    );
    assert!(
        constraints.foreign_keys.is_empty(),
        "the new table inherited a FOREIGN KEY"
    );
    run(&db, "INSERT INTO doomed VALUES (1, 'again')");
    // The dropped table's FOREIGN KEY no longer guards its parent.
    run(&db, "DELETE FROM parent WHERE id = 1");
    assert!(ids(&db, "SELECT id FROM parent").is_empty());
    let hydrated = hydrate_checkpoint(&db, &root.path().join("after-recreate"));
    assert_eq!(ids(&hydrated, "SELECT id FROM doomed"), vec![1]);
}

/// A constraint record whose table is gone — what DROP TABLE left behind
/// before the fix, and still holds in the stores it wrote — is never taken
/// over by a new table of that name. Debug builds (the test suites) refuse it
/// loudly in `Catalog::create_table`; release builds discard it there, so the
/// new table's record holds its own constraints only. Red before the fix in
/// both: no refusal, and the dead UNIQUE and PRIMARY KEY records merged into
/// the new table's set.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "catalog orphan"))]
fn an_orphaned_constraint_record_is_never_merged_into_a_new_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    run(&db, "CREATE TABLE ghost (id INT PRIMARY KEY, v INT UNIQUE)");
    let key = b"table_constraints:ghost".to_vec();
    let record = db.storage.get(&key).unwrap().expect("the table's constraint record");
    run(&db, "DROP TABLE ghost");
    // The record as an older DROP TABLE left it: the table gone, the record not.
    db.storage.put(&key, &record).unwrap();

    run(&db, "CREATE TABLE ghost (id INT PRIMARY KEY, w TEXT)");
    let constraints = db.storage.catalog().load_table_constraints("ghost").unwrap();
    let unique: Vec<&str> = constraints.unique_constraints.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        unique,
        vec!["ghost_id_pkey"],
        "the orphaned record was merged into the new table's"
    );
}

/// RENAME COLUMN rewrites the index definitions naming the column, so a
/// reopen registers both indexes on the new name and the composite UNIQUE is
/// enforced over the rows it already holds. Red before the fix: the
/// definitions kept `username`, the checkpoint did not hydrate ("index has
/// invalid columns"), the reopened indexes sat on a missing column (empty),
/// and the duplicate `(bob, 20)` was accepted.
#[test]
fn rename_column_rewrites_the_index_definitions_a_reopen_registers() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("primary");
    {
        let db = EmbeddedDatabase::with_config(config(&path)).unwrap();
        for sql in [
            "CREATE TABLE accounts (id INT PRIMARY KEY, username TEXT, score INT)",
            "INSERT INTO accounts VALUES (1, 'ada', 10), (2, 'bob', 20), (3, 'cy', 20)",
            "CREATE INDEX accounts_username_idx ON accounts (username)",
            "CREATE UNIQUE INDEX accounts_username_score_uidx ON accounts (username, score)",
            "ALTER TABLE accounts RENAME COLUMN username TO login",
        ] {
            run(&db, sql);
        }
        let definition = db
            .storage
            .catalog()
            .get_index_definition("accounts_username_idx")
            .unwrap()
            .expect("the index definition");
        assert_eq!(definition.column_name, "login");
        let hydrated = hydrate_checkpoint(&db, &root.path().join("checkpoint"));
        assert_eq!(ids(&hydrated, "SELECT id FROM accounts WHERE login = 'bob'"), vec![2]);
    }

    let db = EmbeddedDatabase::with_config(config(&path)).unwrap();
    let art = db.storage.art_indexes();
    let indexes = art.list_table_indexes("accounts");
    let columns_of = |name: &str| {
        indexes
            .iter()
            .find(|(index, _, _)| index == name)
            .map(|(_, _, columns)| columns.clone())
    };
    assert_eq!(columns_of("accounts_username_idx"), Some(vec!["login".to_string()]));
    assert_eq!(
        columns_of("accounts_username_score_uidx"),
        Some(vec!["login".to_string(), "score".to_string()])
    );
    let bob = ArtIndexManager::encode_key(&[Value::String("bob".into())]);
    assert_eq!(art.index_get_all("accounts_username_idx", &bob), vec![2]);
    assert_eq!(ids(&db, "SELECT id FROM accounts WHERE login = 'bob'"), vec![2]);
    assert!(
        db.execute("INSERT INTO accounts VALUES (4, 'bob', 20)").is_err(),
        "the composite UNIQUE index over the renamed column stopped enforcing after the reopen"
    );
    run(&db, "INSERT INTO accounts VALUES (5, 'bob', 30)");
}

/// MERGE BRANCH into main re-derives main's indexes from the rows it wrote,
/// so the live primary answers exactly like the strict hydration of its own
/// checkpoint. Red before the fix: the merged row 3 was missing from the
/// primary's PRIMARY KEY and secondary indexes (its PK lookup missed), and the
/// row the branch deleted was still in them.
#[test]
fn merge_branch_leaves_the_primary_indexes_a_reopen_would_build() {
    let root = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::with_config(config(&root.path().join("primary"))).unwrap();
    for sql in [
        "CREATE TABLE t (id INT PRIMARY KEY, val TEXT)",
        "CREATE INDEX t_val_idx ON t (val)",
        "INSERT INTO t VALUES (1, 'one'), (2, 'two')",
        "CREATE BRANCH dev AS OF NOW",
        "USE BRANCH dev",
        "INSERT INTO t VALUES (3, 'three')",
        "DELETE FROM t WHERE id = 2",
        "USE BRANCH main",
        "MERGE BRANCH dev INTO main",
    ] {
        run(&db, sql);
    }
    let reopened = hydrate_checkpoint(&db, &root.path().join("checkpoint"));
    assert_eq!(ids(&reopened, "SELECT id FROM t"), vec![1, 3]);
    assert_eq!(
        db.storage.art_indexes().pk_index_len("t"),
        reopened.storage.art_indexes().pk_index_len("t"),
        "the COUNT(*) fast path"
    );
    assert_eq!(census(&db, "t"), census(&reopened, "t"), "the primary's ART indexes");
    for id in [1, 2, 3] {
        assert_eq!(
            db.storage.get_row_by_pk("t", &Value::Int4(id)).unwrap(),
            reopened.storage.get_row_by_pk("t", &Value::Int4(id)).unwrap(),
            "PK lookup {id}"
        );
    }
    assert_eq!(ids(&db, "SELECT id FROM t WHERE id = 3"), vec![3]);
    assert_eq!(ids(&db, "SELECT id FROM t WHERE val = 'three'"), vec![3]);
    assert!(ids(&db, "SELECT id FROM t WHERE val = 'two'").is_empty());
}

/// The same for main's HNSW index: the merged row's vector is indexed, as
/// a hydration of the primary's checkpoint indexes it. Red before the fix:
/// the primary's index kept its pre-merge two entries.
#[cfg(feature = "vector-search")]
#[test]
fn merge_branch_maintains_the_primary_vector_index() {
    let root = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::with_config(config(&root.path().join("primary"))).unwrap();
    for sql in [
        "CREATE TABLE docs (id INT PRIMARY KEY, emb VECTOR(3))",
        "INSERT INTO docs VALUES (1, CAST('[1.0,0.0,0.0]' AS VECTOR(3))), (2, CAST('[0.0,1.0,0.0]' AS VECTOR(3)))",
        "CREATE INDEX docs_emb_idx ON docs USING hnsw (emb)",
        "CREATE BRANCH dev AS OF NOW",
        "USE BRANCH dev",
        "INSERT INTO docs VALUES (3, CAST('[0.0,0.0,1.0]' AS VECTOR(3)))",
        "USE BRANCH main",
        "MERGE BRANCH dev INTO main",
    ] {
        run(&db, sql);
    }
    let reopened = hydrate_checkpoint(&db, &root.path().join("checkpoint"));
    assert_eq!(
        reopened.storage.vector_indexes().index_live_size("docs_emb_idx"),
        Some(3)
    );
    assert_eq!(
        db.storage.vector_indexes().index_live_size("docs_emb_idx"),
        Some(3),
        "the merged row's vector is missing from the primary's HNSW index"
    );
}
