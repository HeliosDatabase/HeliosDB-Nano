//! `DROP TABLE` tears down the table's INDEXES — item #90.
//!
//! # The defect
//!
//! `Catalog::drop_table` did exactly one index-related thing:
//! `ArtManager::drop_table_indexes`. It never touched the table's VECTOR index
//! and never deleted the table's `meta:index:<name>` definitions. That is not a
//! leak, it is a correctness bug, in four escalating ways:
//!
//!   1. **Silent wrong kNN results on a re-used table name.** `VectorIndexManager`
//!      metadata is keyed by index name with `table_name` as a FIELD, and DML
//!      resolves indexes per table through `indexes_on_table`. After
//!      `DROP TABLE t; CREATE TABLE t (…, e VECTOR(n))` the orphaned index was
//!      still registered against `t`, so new rows went into the OLD graph — which
//!      still held the dropped table's vectors. And because `drop_table` deletes
//!      `counter:{table}`, the new table's row ids restart at 1 and COLLIDE with
//!      the dead entries, so a kNN query resolved dropped-table row ids against
//!      live rows.
//!   2. **Resurrection at open.** `rebuild_all_indexes` / `rebuild_vector_indexes`
//!      replay every persisted definition with no table-existence filter, so the
//!      orphan either came back attached to a re-created table or warned and
//!      `mark_degraded`-ed at EVERY open — training operators to ignore startup
//!      warnings.
//!   3. **Name squatting.** `CREATE INDEX <name> ON another_table` failed, and
//!      `DROP INDEX <name>` succeeded against a table dropped long ago.
//!   4. **Unbounded space.** Checkpoints only write keys for LIVE indexes, so the
//!      `vecsnap:` blob, the `hnsw_snapshots/*.hnsw.graph|.data` dumps and the
//!      persistent-HNSW keyspace had nothing that would ever collect them.
//!
//! # The shape of the fix
//!
//! `Catalog::drop_table` — the ONE funnel every table removal goes through,
//! including WAL replay and the Stage-0 partition cascade — now calls
//! `drop_table_index_definitions`, which runs the SAME
//! `crate::storage::teardown_index_structures` body that `DROP INDEX` runs, then
//! deletes the definition. There is one teardown implementation, not two.
//!
//! # How these tests observe it
//!
//!   * live ART registration  → `pg_indexes` (`live_index_names`), which lists
//!     the live ART registry, not the catalog records
//!   * live vector registration → `db.storage.vector_indexes().index_exists(..)`
//!   * durable `meta:index:` record → `DROP INDEX <name>` must now FAIL, and
//!     `CREATE INDEX <name>` must now SUCCEED
//!
//! # Both executor families, always
//!
//!   * text family   — `db.execute()`        → `execute_in_transaction_inner`
//!   * params family — `db.execute_params()` → `execute_plan_with_params_inner`
//!     (the PG extended protocol: psycopg, JDBC, sqlx, node-postgres; plus REST)
//!
//! Both reach `DROP TABLE` through the shared `plan_to_operator` arm, which is
//! the point.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::EmbeddedDatabase;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn run(db: &EmbeddedDatabase, sql: &str, params_family: bool) -> heliosdb_nano::Result<u64> {
    if params_family {
        db.execute_params(sql, &[])
    } else {
        db.execute(sql)
    }
}

fn family_name(params_family: bool) -> &'static str {
    if params_family {
        "params"
    } else {
        "text"
    }
}

fn memory_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nano_droptbl_idx_{tag}_{id}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Index names visible through `pg_indexes` — the LIVE ART / vector
/// registrations, not the catalog records. Answers "is the backing structure
/// really gone", which a catalog check cannot see.
fn live_index_names(db: &EmbeddedDatabase) -> Vec<String> {
    let (rows, cols) = db
        .query_with_columns("SELECT * FROM pg_indexes")
        .expect("pg_indexes must be reachable");
    let idx = cols
        .iter()
        .position(|c| c == "indexname")
        .expect("pg_indexes must have an indexname column");
    rows.iter()
        .filter_map(|r| r.values.get(idx))
        .map(|v| match v {
            heliosdb_nano::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect()
}

fn art_index_is_live(db: &EmbeddedDatabase, name: &str) -> bool {
    live_index_names(db).iter().any(|n| n == name)
}

fn vector_index_is_live(db: &EmbeddedDatabase, name: &str) -> bool {
    db.storage.vector_indexes().index_exists(name)
}

/// True when a `meta:index:<name>` record still exists. Probed BEHAVIOURALLY:
/// `DROP INDEX` succeeds if and only if the definition is there (its ART arm
/// tolerates a missing live registration and deletes the record anyway), so a
/// successful drop after a `DROP TABLE` is exactly the orphan this item is
/// about. Consumes the record, so call it last in a test.
fn definition_survived(db: &EmbeddedDatabase, name: &str) -> bool {
    db.execute(&format!("DROP INDEX {name}")).is_ok()
}

fn seed_docs(db: &EmbeddedDatabase, table: &str) {
    db.execute(&format!(
        "CREATE TABLE {table} (id INT PRIMARY KEY, status TEXT, owner TEXT)"
    ))
    .unwrap();
    for i in 0..20 {
        let status = if i % 2 == 0 { "open" } else { "closed" };
        db.execute(&format!(
            "INSERT INTO {table} (id, status, owner) VALUES ({i}, '{status}', 'u{}')",
            i % 4
        ))
        .unwrap();
    }
}

fn seed_vectors(db: &EmbeddedDatabase, table: &str) {
    db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, emb VECTOR(3))"))
        .unwrap();
    db.execute(&format!(
        "INSERT INTO {table} (id, emb) VALUES (1, CAST('[5.0,0.0,0.0]' AS VECTOR(3)))"
    ))
    .unwrap();
    db.execute(&format!(
        "INSERT INTO {table} (id, emb) VALUES (2, CAST('[0.0,5.0,0.0]' AS VECTOR(3)))"
    ))
    .unwrap();
    db.execute(&format!(
        "INSERT INTO {table} (id, emb) VALUES (3, CAST('[0.0,0.0,5.0]' AS VECTOR(3)))"
    ))
    .unwrap();
}

fn ids(rows: &[heliosdb_nano::Tuple]) -> Vec<i64> {
    rows.iter()
        .map(|t| match t.values.first() {
            Some(heliosdb_nano::Value::Int4(v)) => i64::from(*v),
            Some(heliosdb_nano::Value::Int8(v)) => *v,
            other => panic!("expected an integer id, got {other:?}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. The definition dies with the table — both families
// ---------------------------------------------------------------------------

/// An ART secondary index's `meta:index:` record must go with its table. If it
/// survives, `rebuild_all_indexes` calls `create_manual_index(name, table, col)`
/// at the next open for a table that no longer exists — and, worse, attaches it
/// to whatever table is later created under that name.
#[test]
fn drop_table_removes_its_art_index_definition() {
    for params_family in [false, true] {
        let fam = family_name(params_family);
        let db = memory_db();

        seed_docs(&db, "docs");
        db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();
        assert!(
            art_index_is_live(&db, "docs_status_idx"),
            "[{fam}] sanity: the index must exist before the table is dropped"
        );

        run(&db, "DROP TABLE docs", params_family).unwrap_or_else(|e| panic!("[{fam}] DROP TABLE must succeed: {e}"));

        assert!(
            !art_index_is_live(&db, "docs_status_idx"),
            "[{fam}] the dropped table's ART index is still registered"
        );
        assert!(
            !definition_survived(&db, "docs_status_idx"),
            "[{fam}] *** ORPHAN *** the meta:index: record for docs_status_idx survived \
             DROP TABLE — the index will be rebuilt at the next open"
        );
    }
}

/// The index NAME must be free again afterwards. While the orphan record and its
/// live registration existed, `CREATE INDEX <same name> ON another_table` was
/// rejected with "already exists" and the user had no table to drop it from.
#[test]
fn an_index_name_is_reusable_after_its_table_is_dropped() {
    for params_family in [false, true] {
        let fam = family_name(params_family);
        let db = memory_db();

        seed_vectors(&db, "vt");
        db.execute("CREATE INDEX shared_name_idx ON vt USING hnsw (emb)")
            .unwrap();

        run(&db, "DROP TABLE vt", params_family).unwrap_or_else(|e| panic!("[{fam}] DROP TABLE must succeed: {e}"));

        seed_vectors(&db, "vt_other");
        db.execute("CREATE INDEX shared_name_idx ON vt_other USING hnsw (emb)")
            .unwrap_or_else(|e| {
                panic!("[{fam}] *** NAME SQUATTED *** the dropped table's index still owns its name: {e}")
            });
    }
}

// ---------------------------------------------------------------------------
// 2. Vector indexes — the half `drop_table_indexes` never covered
// ---------------------------------------------------------------------------

/// `ArtManager::drop_table_indexes` covers the ART family ONLY. An HNSW index
/// lives in `VectorIndexManager`, so before this fix the whole graph outlived its
/// table.
#[test]
fn drop_table_removes_its_hnsw_index() {
    for params_family in [false, true] {
        let fam = family_name(params_family);
        let db = memory_db();

        seed_vectors(&db, "vt");
        db.execute("CREATE INDEX vt_emb_idx ON vt USING hnsw (emb)").unwrap();
        assert!(
            vector_index_is_live(&db, "vt_emb_idx"),
            "[{fam}] sanity: the HNSW index must exist before the table is dropped"
        );

        run(&db, "DROP TABLE vt", params_family).unwrap_or_else(|e| panic!("[{fam}] DROP TABLE must succeed: {e}"));

        assert!(
            !vector_index_is_live(&db, "vt_emb_idx"),
            "[{fam}] *** LEAK *** the HNSW index outlived its table — it is still registered \
             against 'vt' and will absorb the rows of any table later created under that name"
        );
        assert!(
            !definition_survived(&db, "vt_emb_idx"),
            "[{fam}] the meta:index: record for the HNSW index survived DROP TABLE"
        );
    }
}

/// *** THE CORRECTNESS CASE. ***
///
/// Drop a vector table and re-create it under the same name. The old graph must
/// not still be attached: its entries were keyed by row id, `DROP TABLE` deletes
/// `counter:{table}`, so the new table's ids restart at 1 and collide with the
/// dead ones. A kNN query then ranks live rows against a dropped table's vectors.
#[test]
fn a_recreated_table_does_not_inherit_the_dropped_tables_vectors() {
    let db = memory_db();

    seed_vectors(&db, "rt");
    db.execute("CREATE INDEX rt_emb_idx ON rt USING hnsw (emb)").unwrap();
    db.execute("DROP TABLE rt").expect("DROP TABLE");

    assert!(
        !vector_index_is_live(&db, "rt_emb_idx"),
        "*** SILENT WRONG RESULTS *** the dropped table's HNSW index is still registered; \
         a table re-created as 'rt' would insert into a graph full of dead vectors"
    );

    // Re-create the table and its index under the same names. Both statements
    // FAIL while the orphan is alive — CREATE INDEX with "already exists".
    db.execute("CREATE TABLE rt (id INT PRIMARY KEY, emb VECTOR(3))")
        .expect("the table name must be re-usable");
    db.execute("INSERT INTO rt (id, emb) VALUES (7, CAST('[0.5,0.0,0.0]' AS VECTOR(3)))")
        .expect("insert into the re-created table");
    db.execute("CREATE INDEX rt_emb_idx ON rt USING hnsw (emb)")
        .expect("the index name must be re-usable after its table was dropped");

    let rows = db
        .query("SELECT id FROM rt ORDER BY emb <-> '[0.0, 0.0, 0.0]' LIMIT 5", &[])
        .expect("kNN over the re-created table");
    assert_eq!(
        ids(&rows),
        vec![7],
        "kNN over the re-created table returned rows that are not in it — the dropped \
         table's vectors are still in the graph"
    );
}

/// `IndexFamily::DdlOnly` (`gin` / `gist`) has no backing structure at all, so
/// deleting the catalog record IS the whole teardown. It still has to happen: the
/// record squats the name and is replayed at open.
#[test]
fn gin_index_definition_is_removed_with_its_table() {
    for params_family in [false, true] {
        let fam = family_name(params_family);
        let db = memory_db();

        db.execute("CREATE TABLE posts (id INT PRIMARY KEY, body TEXT)")
            .unwrap();
        db.execute("INSERT INTO posts (id, body) VALUES (1, 'hello world')")
            .unwrap();
        db.execute("CREATE INDEX posts_body_gin ON posts USING gin (body)")
            .unwrap();

        run(&db, "DROP TABLE posts", params_family).unwrap_or_else(|e| panic!("[{fam}] DROP TABLE must succeed: {e}"));

        assert!(
            !definition_survived(&db, "posts_body_gin"),
            "[{fam}] the gin definition survived its table"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. NEGATIVE — nothing else may be touched
// ---------------------------------------------------------------------------

/// *** The single most important guard on the `table_name` filter. *** Two
/// tables, each with an ART index and an HNSW index. Dropping one must leave the
/// other's definition, live registration and query results completely alone.
#[test]
fn drop_table_leaves_another_tables_indexes_alone() {
    let db = memory_db();

    seed_docs(&db, "keep_docs");
    db.execute("CREATE INDEX keep_status_idx ON keep_docs (status)")
        .unwrap();
    seed_vectors(&db, "keep_vecs");
    db.execute("CREATE INDEX keep_emb_idx ON keep_vecs USING hnsw (emb)")
        .unwrap();

    seed_docs(&db, "gone_docs");
    db.execute("CREATE INDEX gone_status_idx ON gone_docs (status)")
        .unwrap();
    seed_vectors(&db, "gone_vecs");
    db.execute("CREATE INDEX gone_emb_idx ON gone_vecs USING hnsw (emb)")
        .unwrap();

    db.execute("DROP TABLE gone_docs").expect("DROP TABLE gone_docs");
    db.execute("DROP TABLE gone_vecs").expect("DROP TABLE gone_vecs");

    assert!(
        art_index_is_live(&db, "keep_status_idx"),
        "DROP TABLE took an UNRELATED table's ART index with it: {:?}",
        live_index_names(&db)
    );
    assert!(
        vector_index_is_live(&db, "keep_emb_idx"),
        "DROP TABLE took an UNRELATED table's HNSW index with it"
    );
    assert_eq!(
        db.query("SELECT id FROM keep_docs WHERE status = 'open'", &[])
            .unwrap()
            .len(),
        10,
        "the surviving table's index-backed query changed answers"
    );
    let rows = db
        .query(
            "SELECT id FROM keep_vecs ORDER BY emb <-> '[5.0, 0.0, 0.0]' LIMIT 1",
            &[],
        )
        .expect("kNN on the surviving table");
    assert_eq!(ids(&rows), vec![1], "the surviving table's kNN changed answers");

    // And the surviving definitions are still there (this consumes them).
    assert!(
        definition_survived(&db, "keep_status_idx"),
        "DROP TABLE deleted an UNRELATED table's ART index definition"
    );
    assert!(
        definition_survived(&db, "keep_emb_idx"),
        "DROP TABLE deleted an UNRELATED table's HNSW index definition"
    );
}

/// Constraint indexes (PRIMARY KEY / UNIQUE / FOREIGN KEY) have NO `meta:index:`
/// record — they are registered under generated names by `create_pk_index` &c.
/// The new scan must therefore never reach them. If it somehow did, inserts would
/// simply stop being checked, silently.
#[test]
fn drop_table_does_not_disturb_constraint_indexes() {
    let db = memory_db();

    db.execute("CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE)")
        .unwrap();
    db.execute("INSERT INTO users (id, email) VALUES (1, 'a@x')").unwrap();

    // An unrelated table, with its own index, is dropped.
    seed_docs(&db, "scratch");
    db.execute("CREATE INDEX scratch_status_idx ON scratch (status)")
        .unwrap();
    db.execute("DROP TABLE scratch").expect("DROP TABLE scratch");

    assert!(
        db.execute("INSERT INTO users (id, email) VALUES (1, 'b@x')").is_err(),
        "the PRIMARY KEY stopped being enforced after an unrelated DROP TABLE"
    );
    assert!(
        db.execute("INSERT INTO users (id, email) VALUES (2, 'a@x')").is_err(),
        "the UNIQUE constraint stopped being enforced after an unrelated DROP TABLE"
    );
    assert_eq!(
        db.query("SELECT id FROM users", &[]).unwrap().len(),
        1,
        "an unrelated DROP TABLE changed this table's rows"
    );
}

/// A table with no user indexes at all must drop exactly as before — the new scan
/// finds nothing and reports nothing.
#[test]
fn drop_table_with_no_indexes_is_unaffected() {
    for params_family in [false, true] {
        let fam = family_name(params_family);
        let db = memory_db();
        seed_docs(&db, "plain");
        run(&db, "DROP TABLE plain", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP TABLE on an index-free table must succeed: {e}"));
        assert!(
            db.query("SELECT id FROM plain", &[]).is_err(),
            "[{fam}] the table survived its own DROP"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. The paths an `on_table_dropped`-level fix would miss
// ---------------------------------------------------------------------------

/// The Stage-0 partition cascade recurses through `Catalog::drop_table` for CHILD
/// tables, whose names never appear in the drop PLAN. Placing the teardown in the
/// catalog funnel covers them for free; placing it one layer up would not.
#[test]
fn partition_cascade_tears_down_child_table_indexes() {
    let db = memory_db();

    db.execute("CREATE TABLE p_parent (id INT NOT NULL, label TEXT) PARTITION BY RANGE (id)")
        .unwrap();
    db.execute("CREATE TABLE p_child PARTITION OF p_parent FOR VALUES FROM (0) TO (100)")
        .unwrap();
    db.execute("INSERT INTO p_child (id, label) VALUES (5, 'hello')")
        .unwrap();
    db.execute("CREATE INDEX p_child_label_idx ON p_child (label)").unwrap();
    assert!(
        art_index_is_live(&db, "p_child_label_idx"),
        "sanity: the child's index must exist before the parent is dropped"
    );

    db.execute("DROP TABLE p_parent").expect("cascade drop");

    assert!(
        !art_index_is_live(&db, "p_child_label_idx"),
        "the partition CHILD's index survived the cascade — its name never appears in the \
         drop plan, so only the catalog funnel can reach it"
    );
    assert!(
        !definition_survived(&db, "p_child_label_idx"),
        "the partition child's meta:index: record survived the cascade"
    );
}

/// Definitions are what `rebuild_all_indexes` / `rebuild_vector_indexes` replay at
/// open, so this is the whole durability story: a teardown that removed only the
/// in-memory registration would be undone by the very next process to attach.
#[test]
fn dropped_table_indexes_do_not_come_back_after_a_reopen() {
    let dir = scratch_dir("reopen");

    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        seed_docs(&db, "docs");
        db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();
        seed_vectors(&db, "vt");
        db.execute("CREATE INDEX vt_emb_idx ON vt USING hnsw (emb)").unwrap();

        seed_docs(&db, "keep_docs");
        db.execute("CREATE INDEX keep_status_idx ON keep_docs (status)")
            .unwrap();

        db.execute("DROP TABLE docs").expect("DROP TABLE docs");
        db.execute("DROP TABLE vt").expect("DROP TABLE vt");
    }

    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");

        assert!(
            !art_index_is_live(&db, "docs_status_idx"),
            "the dropped table's ART index was resurrected by the open-time rebuild: {:?}",
            live_index_names(&db)
        );
        assert!(
            !vector_index_is_live(&db, "vt_emb_idx"),
            "the dropped table's HNSW index was resurrected by the open-time rebuild"
        );
        assert!(
            art_index_is_live(&db, "keep_status_idx"),
            "the surviving table's index did not come back: {:?}",
            live_index_names(&db)
        );

        assert!(
            !definition_survived(&db, "docs_status_idx"),
            "the ART definition survived DROP TABLE across a reopen"
        );
        assert!(
            !definition_survived(&db, "vt_emb_idx"),
            "the HNSW definition survived DROP TABLE across a reopen"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 5. Encrypted data directory — the TDE read discipline this depends on
// ---------------------------------------------------------------------------

/// The new teardown reads through `Catalog::list_index_definitions`, which goes
/// via `StorageEngine::meta_blobs_with_prefix` (the one place decryption happens).
/// A raw RocksDB iterator would hand back CIPHERTEXT on a TDE data dir, every
/// record would fail to decode, and the teardown would silently do NOTHING —
/// exactly the v4.21.0 failure mode, re-introduced. This pins that it does not.
///
/// Gated on `encryption` only because a key manager cannot be constructed without
/// it; `encryption` is in the DEFAULT feature set, so this runs in the standard
/// gate.
#[cfg(feature = "encryption")]
#[test]
fn drop_table_teardown_works_on_an_encrypted_database() {
    const KEY_VAR: &str = "HELIOSDB_TEST_DROP_TABLE_IDX_TDE_KEY";
    std::env::set_var(
        KEY_VAR,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );

    let dir = scratch_dir("tde");
    let encrypted_config = || {
        let mut config = heliosdb_nano::Config::default();
        config.storage.memory_only = false;
        config.storage.path = Some(dir.clone());
        config.encryption.enabled = true;
        config.encryption.key_source = heliosdb_nano::KeySource::Environment(KEY_VAR.to_string());
        config
    };

    {
        let db = EmbeddedDatabase::with_config(encrypted_config()).expect("encrypted database");
        seed_docs(&db, "docs");
        db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();
        seed_vectors(&db, "vt");
        db.execute("CREATE INDEX vt_emb_idx ON vt USING hnsw (emb)").unwrap();
        assert!(
            art_index_is_live(&db, "docs_status_idx") && vector_index_is_live(&db, "vt_emb_idx"),
            "sanity: both indexes must exist in the creating session"
        );

        db.execute("DROP TABLE docs")
            .expect("DROP TABLE on an encrypted database");
        db.execute("DROP TABLE vt")
            .expect("DROP TABLE on an encrypted database");
    }

    {
        let db = EmbeddedDatabase::with_config(encrypted_config()).expect("reopen encrypted database");
        assert!(
            !art_index_is_live(&db, "docs_status_idx"),
            "*** TDE TEARDOWN NO-OP *** the ART index came back on an encrypted data dir: {:?}",
            live_index_names(&db)
        );
        assert!(
            !vector_index_is_live(&db, "vt_emb_idx"),
            "*** TDE TEARDOWN NO-OP *** the HNSW index came back on an encrypted data dir"
        );
        assert!(
            !definition_survived(&db, "docs_status_idx"),
            "the ART definition survived on an encrypted data dir — the teardown is reading ciphertext"
        );
        assert!(
            !definition_survived(&db, "vt_emb_idx"),
            "the HNSW definition survived on an encrypted data dir"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
