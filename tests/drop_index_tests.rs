//! `DROP INDEX` — the second half of roadmap §2.1, and the TDE
//! index-definition read hazard that had to be fixed before it could work.
//!
//! # Why this file exists
//!
//! `DROP INDEX` was this repo's signature defect in its purest form. Every
//! piece of the drop already existed and NOTHING CALLED ANY OF IT:
//! `Catalog::drop_index_definition`, `ArtManager::drop_index`,
//! `VectorIndexManager::drop_index`, `StorageEngine::log_drop_index` (zero
//! callers) and the `WalOperation::DropIndex` replay arm. Meanwhile the
//! statement itself fell through the planner's `_ => LogicalPlan::DropTable`
//! catch-all and DESTROYED A TABLE that happened to share the index's name.
//! v4.20.0 deleted the catch-all and made it a loud error; v4.21.0 makes it a
//! real drop. The v4.20.0 guard — "a same-named TABLE is not touched" — is
//! asserted here too, because it must keep holding now that the statement
//! actually does something.
//!
//! # The prerequisite (`list_index_definitions` on an encrypted data dir)
//!
//! `save_index_definition` writes through `StorageEngine::put`, which ENCRYPTS
//! when a key manager is configured. `list_index_definitions` used to read
//! record VALUES straight off a raw RocksDB iterator, so on a TDE data dir it
//! got CIPHERTEXT — and because decoding is per-record resilient (an
//! undecodable record is warned and SKIPPED), the failure was silent and total:
//! `Catalog::rebuild_all_indexes` is the ONLY thing that re-registers user
//! secondary indexes at open, so every `CREATE INDEX` index vanished at every
//! restart. Queries stayed correct and full-scanned forever. That is what
//! `secondary_indexes_survive_reopen_on_an_encrypted_database` pins.
//!
//! # Both executor families, always
//!
//!   * text family   — `db.execute()`        → `execute_in_transaction_inner`
//!   * params family — `db.execute_params()` → `execute_plan_with_params_inner`
//!                     (the PG extended protocol: psycopg, JDBC, sqlx,
//!                      node-postgres; plus REST/BaaS)
//!
//! Both reach `DROP INDEX` through the SHARED `plan_to_operator` arm, which is
//! the point — a fix that lands in only one family is what this repo keeps
//! shipping.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::EmbeddedDatabase;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Run `sql` through the requested executor family.
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

/// A unique scratch directory for a disk-backed (reopen) test.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nano_dropidx_{tag}_{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Index names currently visible through `pg_indexes`, which reports the LIVE
/// ART / vector registrations (not the catalog records) — so it answers "is the
/// backing structure really gone", the half of the drop a catalog check cannot
/// see.
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

fn table_exists(db: &EmbeddedDatabase, table: &str) -> bool {
    db.query(&format!("SELECT COUNT(*) FROM {table}"), &[]).is_ok()
}

fn seed_docs(db: &EmbeddedDatabase, table: &str) {
    db.execute(&format!(
        "CREATE TABLE {table} (id INT PRIMARY KEY, status TEXT, owner TEXT)"
    ))
    .unwrap();
    for i in 0..40 {
        let status = if i % 2 == 0 { "open" } else { "closed" };
        db.execute(&format!(
            "INSERT INTO {table} (id, status, owner) VALUES ({i}, '{status}', 'u{}')",
            i % 4
        ))
        .unwrap();
    }
}

// ---------------------------------------------------------------------------
// 1. The drop actually drops — and the data does not change
// ---------------------------------------------------------------------------

/// The core claim. An ART secondary index is removed from the live registry,
/// and a query that was being answered THROUGH that index still returns exactly
/// the same rows afterwards (it falls back to a scan). A drop that changed
/// query RESULTS would be a correctness bug, not a drop.
#[test]
fn art_index_drops_and_query_results_are_unchanged() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();
        assert!(
            live_index_names(&db).iter().any(|n| n == "docs_status_idx"),
            "[{fam}] the index must exist before we drop it"
        );

        let before = db
            .query("SELECT id FROM docs WHERE status = 'open'", &[])
            .unwrap()
            .len();
        assert_eq!(before, 20, "[{fam}] sanity: 20 open docs");

        run(&db, "DROP INDEX docs_status_idx", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP INDEX must succeed: {e}"));

        assert!(
            !live_index_names(&db).iter().any(|n| n == "docs_status_idx"),
            "[{fam}] the index is still registered after DROP INDEX — the drop did nothing"
        );

        let after = db
            .query("SELECT id FROM docs WHERE status = 'open'", &[])
            .unwrap()
            .len();
        assert_eq!(
            after, before,
            "[{fam}] dropping an index changed query RESULTS ({before} -> {after})"
        );
        assert_eq!(
            db.query("SELECT id FROM docs", &[]).unwrap().len(),
            40,
            "[{fam}] the table lost rows to DROP INDEX"
        );
    }
}

/// Drop, then recreate under the same name. This fails if the drop leaves
/// EITHER half behind: a live ART registration makes `create_manual_index`
/// return `IndexAlreadyExists`, and a surviving `meta:index:` record makes the
/// index reappear at the next open.
#[test]
fn index_can_be_dropped_and_recreated() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        db.execute("CREATE INDEX docs_owner_idx ON docs (owner)").unwrap();
        run(&db, "DROP INDEX docs_owner_idx", params_family).unwrap();
        db.execute("CREATE INDEX docs_owner_idx ON docs (owner)")
            .unwrap_or_else(|e| panic!("[{fam}] recreate after drop must succeed: {e}"));

        assert!(
            live_index_names(&db).iter().any(|n| n == "docs_owner_idx"),
            "[{fam}] the recreated index is not registered"
        );
        assert_eq!(
            db.query("SELECT id FROM docs WHERE owner = 'u1'", &[]).unwrap().len(),
            10,
            "[{fam}] the recreated index must return the same rows"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Missing index: error, and IF EXISTS
// ---------------------------------------------------------------------------

/// `DROP INDEX <missing>` errors and NAMES the index; `IF EXISTS` makes it a
/// genuine no-op success.
///
/// This is a deliberate reversal of v4.20.0, where `IF EXISTS` did NOT silence
/// the error. That was correct while nothing was ever dropped — reporting
/// success would have been a silent no-op. Now that a real drop exists,
/// PostgreSQL semantics apply.
#[test]
fn missing_index_errors_and_if_exists_succeeds() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        let err = run(&db, "DROP INDEX no_such_index", params_family)
            .err()
            .unwrap_or_else(|| panic!("[{fam}] dropping a missing index must error"))
            .to_string();
        assert!(
            err.contains("no_such_index"),
            "[{fam}] the error must name the index: {err}"
        );

        run(&db, "DROP INDEX IF EXISTS no_such_index", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] IF EXISTS on a missing index must succeed: {e}"));
    }
}

/// Message-shape guard for the PG wire's SQLSTATE classifier. The missing-index
/// error must say "index" and must NOT say "table"/"relation", or
/// `sqlstate_for_query_execution_message` maps it to 42P01 undefined_table
/// instead of 42704 undefined_object. This is the same trap the DROP ROLE
/// messages were shaped around.
///
/// It must ALSO carry the quoted shape `index "<name>"`: the classifier's
/// `message_names_an_index` anchors on the noun being followed by a quote,
/// precisely so it stops matching the string "index" inside somebody else's
/// object name (`Table 'search_index' does not exist`). A message that said
/// `index does not exist: ghost_index` would satisfy the old bare-substring
/// test and be classified XX000 by the real one.
#[test]
fn missing_index_error_is_about_an_index_not_a_relation() {
    let db = memory_db();
    seed_docs(&db, "docs");

    let err = db
        .execute("DROP INDEX ghost_index")
        .expect_err("must error")
        .to_string();
    let lower = err.to_ascii_lowercase();
    assert!(lower.contains("index"), "error must be about an INDEX: {err}");
    assert!(
        !lower.contains("table") && !lower.contains("relation"),
        "error must not claim anything about a table/relation (it would mis-map to 42P01): {err}"
    );
    assert!(
        lower.contains("does not exist"),
        "error must use the recognised not-found shape: {err}"
    );
    assert!(
        lower.contains("index \"") || lower.contains("index '"),
        "the classifier anchors on the QUOTED shape `index \"x\"`; an unquoted message \
         would fall through to XX000: {err}"
    );
}

/// The other half of that anchor, from the engine side: a missing TABLE whose
/// NAME merely contains "index" must still produce a message the classifier
/// reads as a relation error. `search_index`, `pg_indexes`, `price_index` are
/// ordinary table names, and the first draft of the index SQLSTATE arms — a
/// bare `lower.contains("index")` placed ahead of the table rules — turned every
/// one of them from 42P01 undefined_table into 42704 undefined_object.
///
/// Asserted on the MESSAGE here (the wire half is
/// `wire_tests::wire_index_named_table_still_maps_to_undefined_table`), because
/// this is the property the engine owes the classifier.
#[test]
fn a_missing_table_whose_name_contains_index_still_reads_as_a_relation_error() {
    let db = memory_db();

    let err = db
        .query("SELECT * FROM search_index", &[])
        .expect_err("selecting from a missing table must error")
        .to_string();
    let lower = err.to_ascii_lowercase();
    assert!(
        lower.contains("table") || lower.contains("relation"),
        "a missing-table error must name a table/relation or it cannot map to 42P01: {err}"
    );
    // The quoted-index anchor must NOT fire on it: the name ends in `index`
    // immediately followed by the closing quote, with no separating space.
    assert!(
        !lower.contains("index \"") && !lower.contains("index '"),
        "the table name's trailing `index` must not look like the quoted INDEX noun: {err}"
    );
}

// ---------------------------------------------------------------------------
// 3. THE v4.20.0 REGRESSION GUARD — a same-named TABLE is never touched
// ---------------------------------------------------------------------------

/// The original data-loss bug: `DROP INDEX users` was planned as
/// `DROP TABLE users` and silently destroyed the TABLE. Now that DROP INDEX
/// does real work, the guard matters more, not less — there are two ways to
/// get this wrong and both are tested:
///
///   a) no index of that name exists → must ERROR, table untouched
///      (and `IF EXISTS` must be a no-op, still leaving the table alone);
///   b) an index AND a table share the name → the INDEX goes, the TABLE stays.
#[test]
fn a_table_sharing_the_index_name_is_never_dropped() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);

        // (a) A table whose name is not an index at all.
        db.execute("CREATE TABLE analytics (id INT PRIMARY KEY, v TEXT)")
            .unwrap();
        db.execute("INSERT INTO analytics (id, v) VALUES (1, 'keep me')")
            .unwrap();

        assert!(
            run(&db, "DROP INDEX analytics", params_family).is_err(),
            "[{fam}] DROP INDEX on a TABLE name must error, not drop the table"
        );
        run(&db, "DROP INDEX IF EXISTS analytics", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] IF EXISTS must be a no-op: {e}"));
        assert!(
            table_exists(&db, "analytics"),
            "[{fam}] *** DATA LOSS *** DROP INDEX destroyed the same-named TABLE"
        );
        assert_eq!(
            db.query("SELECT id FROM analytics", &[]).unwrap().len(),
            1,
            "[{fam}] the same-named table lost its rows"
        );

        // (b) A table and an index that genuinely share a name. The index
        //     namespace and the table namespace are separate; only the index
        //     may be removed.
        seed_docs(&db, "docs");
        db.execute("CREATE TABLE shadow (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO shadow (id) VALUES (7)").unwrap();
        db.execute("CREATE INDEX shadow ON docs (status)").unwrap();

        run(&db, "DROP INDEX shadow", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP INDEX must drop the index: {e}"));

        assert!(
            !live_index_names(&db).iter().any(|n| n == "shadow"),
            "[{fam}] the index named `shadow` survived its own drop"
        );
        assert!(
            table_exists(&db, "shadow"),
            "[{fam}] *** DATA LOSS *** DROP INDEX shadow destroyed the TABLE shadow"
        );
        assert_eq!(
            db.query("SELECT id FROM shadow", &[]).unwrap().len(),
            1,
            "[{fam}] the same-named table lost its rows"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. SAFETY — constraint-backing indexes are not droppable
// ---------------------------------------------------------------------------

/// *** THE HIGHEST-RISK CASE. *** A PRIMARY KEY / UNIQUE constraint is enforced
/// through its backing ART index. Dropping one would report success and then
/// silently stop checking inserts — no error, duplicates just start landing.
///
/// These names are genuinely reachable, which is why this is not theoretical:
/// `create_pk_index` names the PK index `<table>_pkey`, and `create_unique_index`
/// is called with the COLUMN NAME as the constraint name (`Catalog` at CREATE
/// TABLE and again at rebuild) — so a `UNIQUE` column `email` registers an ART
/// index literally called `email`. Both spellings are refused here, and the
/// assertion that actually matters is the one after: the constraints still
/// enforce.
#[test]
fn refusing_to_drop_a_constraint_index_keeps_the_constraint_enforced() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);

        db.execute("CREATE TABLE accounts (id INT PRIMARY KEY, email TEXT UNIQUE, note TEXT)")
            .unwrap();
        db.execute("INSERT INTO accounts (id, email, note) VALUES (1, 'a@x', 'first')")
            .unwrap();

        for (target, kind) in [("accounts_pkey", "PRIMARY KEY"), ("email", "UNIQUE")] {
            let err = run(&db, &format!("DROP INDEX {target}"), params_family)
                .err()
                .unwrap_or_else(|| panic!("[{fam}] *** SILENT CONSTRAINT REMOVAL *** DROP INDEX {target} succeeded"))
                .to_string();
            assert!(err.contains(target), "[{fam}] the refusal must name the index: {err}");
            assert!(
                err.contains(kind),
                "[{fam}] the refusal must say which constraint kind it backs ({kind}): {err}"
            );
        }

        // The constraints are still doing their job.
        assert!(
            db.execute("INSERT INTO accounts (id, email, note) VALUES (1, 'b@x', 'dup pk')")
                .is_err(),
            "[{fam}] PRIMARY KEY stopped being enforced"
        );
        assert!(
            db.execute("INSERT INTO accounts (id, email, note) VALUES (2, 'a@x', 'dup email')")
                .is_err(),
            "[{fam}] UNIQUE stopped being enforced"
        );
        assert_eq!(
            db.query("SELECT id FROM accounts", &[]).unwrap().len(),
            1,
            "[{fam}] a rejected insert still landed"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Per-type dispatch: hnsw and gin/gist
// ---------------------------------------------------------------------------

/// HNSW indexes live in the `VectorIndexManager`, not the ART manager, so the
/// drop has to dispatch on the persisted `index_type`. A drop that only ever
/// called `ArtManager::drop_index` would leave the vector index in place while
/// deleting its definition — the worst kind of half-drop, invisible until the
/// next restart.
#[test]
fn hnsw_index_drops() {
    let db = memory_db();

    db.execute("CREATE TABLE vecs (id INT PRIMARY KEY, emb VECTOR(3))")
        .unwrap();
    db.execute("INSERT INTO vecs (id, emb) VALUES (1, CAST('[1.0,0.0,0.0]' AS VECTOR(3)))")
        .unwrap();
    db.execute("INSERT INTO vecs (id, emb) VALUES (2, CAST('[0.0,1.0,0.0]' AS VECTOR(3)))")
        .unwrap();
    db.execute("CREATE INDEX vecs_emb_idx ON vecs USING hnsw (emb)")
        .unwrap();

    assert!(
        live_index_names(&db).iter().any(|n| n == "vecs_emb_idx"),
        "the HNSW index must exist before we drop it"
    );

    db.execute("DROP INDEX vecs_emb_idx")
        .expect("DROP INDEX on hnsw must succeed");

    assert!(
        !live_index_names(&db).iter().any(|n| n == "vecs_emb_idx"),
        "the HNSW index is still registered after DROP INDEX"
    );
    // The catalog record is gone too: a second drop must not find anything.
    assert!(
        db.execute("DROP INDEX vecs_emb_idx").is_err(),
        "the HNSW index definition survived the drop"
    );
    assert_eq!(
        db.query("SELECT id FROM vecs", &[]).unwrap().len(),
        2,
        "DROP INDEX removed rows from the table"
    );

    // And it can be built again.
    db.execute("CREATE INDEX vecs_emb_idx ON vecs USING hnsw (emb)")
        .expect("HNSW index must be recreatable after a drop");
}

/// `CREATE INDEX … USING hnsw … WITH (persistent = true)` persists the tag
/// `persistent_hnsw`, NOT `hnsw` — a FOURTH persisting branch of
/// `handle_create_index` that the drop's first draft did not mirror. It knew
/// art/btree/hash/gin/gist/hnsw/hnsw_pq only, so a persistent HNSW index fell
/// into the unknown-tag arm and was PERMANENTLY UNDROPPABLE ("unsupported
/// persisted index type 'persistent_hnsw'" — the engine calling a correct
/// catalog record corrupt), while `rebuild_vector_indexes` happily reopened it
/// at every start.
///
/// Both halves are pinned, because only one of them runs in the default build:
///   * with `vector-persist`  — the statement works and the index drops;
///   * without it             — CREATE fails LOUDLY at
///     `create_persistent_index`, so no `persistent_hnsw` record can exist in
///     the first place. (The tag is still classified — see
///     `catalog::tests::every_persisted_index_tag_is_classified`, which runs in
///     every build.)
#[test]
fn persistent_hnsw_index_drops() {
    let db = memory_db();
    db.execute("CREATE TABLE pvecs (id INT PRIMARY KEY, emb VECTOR(3))")
        .unwrap();
    db.execute("INSERT INTO pvecs (id, emb) VALUES (1, CAST('[1.0,0.0,0.0]' AS VECTOR(3)))")
        .unwrap();

    let created = db.execute("CREATE INDEX pvecs_emb_idx ON pvecs USING hnsw (emb) WITH (persistent = true)");

    if cfg!(feature = "vector-persist") {
        created.expect("a persistent HNSW index must be creatable when the feature is on");
        assert!(
            live_index_names(&db).iter().any(|n| n == "pvecs_emb_idx"),
            "the persistent HNSW index must exist before we drop it"
        );

        db.execute("DROP INDEX pvecs_emb_idx")
            .expect("a persistent HNSW index must be droppable — it was not, before the shared classifier");

        assert!(
            !live_index_names(&db).iter().any(|n| n == "pvecs_emb_idx"),
            "the persistent HNSW index is still registered after DROP INDEX"
        );
        // The catalog record went too: a second drop finds nothing.
        assert!(
            db.execute("DROP INDEX pvecs_emb_idx").is_err(),
            "the persistent HNSW definition survived the drop"
        );
        assert_eq!(
            db.query("SELECT id FROM pvecs", &[]).unwrap().len(),
            1,
            "DROP INDEX removed rows from the table"
        );
    } else {
        let err = created
            .expect_err("without `vector-persist` a persistent index must be refused, not silently downgraded")
            .to_string();
        assert!(
            err.to_ascii_lowercase().contains("vector-persist"),
            "the refusal must name the missing feature: {err}"
        );
        // Nothing was persisted, so there is nothing to drop — and the drop
        // must say so rather than pretending.
        assert!(
            db.execute("DROP INDEX pvecs_emb_idx").is_err(),
            "a CREATE that failed must not have left a droppable definition behind"
        );
    }
}

/// gin / gist indexes are DDL-only in this build: `handle_create_index` persists
/// a definition and builds NOTHING (the `@@` operator scans). The drop is
/// therefore the definition delete alone — which must still be a real delete,
/// not a shrug. Proven by the second drop failing.
#[test]
fn gin_index_drops_even_though_it_has_no_backing_structure() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);

        db.execute("CREATE TABLE posts (id INT PRIMARY KEY, body TEXT)")
            .unwrap();
        db.execute("INSERT INTO posts (id, body) VALUES (1, 'hello world')")
            .unwrap();
        db.execute("CREATE INDEX posts_body_gin ON posts USING gin (body)")
            .unwrap();

        run(&db, "DROP INDEX posts_body_gin", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP INDEX on a gin index must succeed: {e}"));

        assert!(
            run(&db, "DROP INDEX posts_body_gin", params_family).is_err(),
            "[{fam}] the gin definition survived the drop — the second drop should have failed"
        );
        run(&db, "DROP INDEX IF EXISTS posts_body_gin", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] IF EXISTS after the drop must be a no-op: {e}"));
        assert_eq!(
            db.query("SELECT id FROM posts", &[]).unwrap().len(),
            1,
            "[{fam}] DROP INDEX removed rows from the table"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Durability — the drop must not come back at the next open
// ---------------------------------------------------------------------------

/// Indexes are REBUILT AT OPEN from the persisted `meta:index:` definitions
/// (`Catalog::rebuild_all_indexes`). So deleting the definition is the entire
/// durability story: if the drop removed only the in-memory registration, the
/// index would be resurrected by the very next process to attach to the data
/// directory — a drop that reports success and quietly undoes itself.
#[test]
fn dropped_index_does_not_come_back_after_a_reopen() {
    let dir = scratch_dir("reopen");

    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        seed_docs(&db, "docs");
        db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();
        db.execute("CREATE INDEX docs_owner_idx ON docs (owner)").unwrap();
        db.execute("DROP INDEX docs_status_idx").unwrap();
    }

    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        let live = live_index_names(&db);
        assert!(
            !live.iter().any(|n| n == "docs_status_idx"),
            "the dropped index was resurrected by the index rebuild at open: {live:?}"
        );
        assert!(
            live.iter().any(|n| n == "docs_owner_idx"),
            "the drop took an UNRELATED index with it: {live:?}"
        );
        assert_eq!(
            db.query("SELECT id FROM docs WHERE status = 'open'", &[])
                .unwrap()
                .len(),
            20,
            "rows changed across the reopen"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 7. PART 1 REGRESSION — the TDE index-definition read hazard
// ---------------------------------------------------------------------------

/// *** THE PREREQUISITE BUG, PINNED. ***
///
/// `save_index_definition` writes through the ENCRYPTING `StorageEngine::put`.
/// `list_index_definitions` used to read record values off a raw RocksDB
/// iterator, which on a TDE data directory returns CIPHERTEXT. Decoding is
/// per-record resilient — an undecodable record is `warn!`ed and SKIPPED — so
/// nothing failed: `rebuild_all_indexes` simply saw ZERO user index definitions
/// and re-registered none of them. Every `CREATE INDEX` index on an encrypted
/// database disappeared at every restart, permanently, with correct query
/// results and full table scans forever.
///
/// The fix routes the read through `StorageEngine::meta_blobs_with_prefix`,
/// which fetches values via `get` — the one place decryption happens. Without
/// it, `docs_status_idx` is absent from `pg_indexes` in session 2 below.
///
/// Gated on `encryption` only because a key manager cannot be constructed
/// without it; `encryption` is in the DEFAULT feature set, so this runs in the
/// standard gate.
#[cfg(feature = "encryption")]
#[test]
fn secondary_indexes_survive_reopen_on_an_encrypted_database() {
    // A per-test env var name: `cargo test` runs these in ONE process and env
    // vars are process-global.
    const KEY_VAR: &str = "HELIOSDB_TEST_DROP_INDEX_TDE_KEY";
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

    // Session 1 — build two secondary indexes on an ENCRYPTED data directory.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config()).expect("encrypted database");
        seed_docs(&db, "docs");
        db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();
        db.execute("CREATE INDEX docs_owner_idx ON docs (owner)").unwrap();

        let live = live_index_names(&db);
        assert!(
            live.iter().any(|n| n == "docs_status_idx") && live.iter().any(|n| n == "docs_owner_idx"),
            "sanity: both indexes must exist in the creating session: {live:?}"
        );
    }

    // Session 2 — reopen. This is where the ciphertext read used to silently
    // erase every user index.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config()).expect("reopen encrypted database");
        let live = live_index_names(&db);
        assert!(
            live.iter().any(|n| n == "docs_status_idx"),
            "*** TDE INDEX LOSS *** docs_status_idx did not survive a reopen on an encrypted \
             data directory — list_index_definitions is reading ciphertext and silently \
             skipping every record: {live:?}"
        );
        assert!(
            live.iter().any(|n| n == "docs_owner_idx"),
            "*** TDE INDEX LOSS *** docs_owner_idx did not survive a reopen: {live:?}"
        );

        // Rows are unaffected either way — which is exactly why this was silent.
        assert_eq!(
            db.query("SELECT id FROM docs WHERE status = 'open'", &[])
                .unwrap()
                .len(),
            20
        );

        // And DROP INDEX works on an encrypted data dir: the drop path reads
        // the definition through the same decrypting `get`.
        db.execute("DROP INDEX docs_status_idx")
            .expect("DROP INDEX must work on an encrypted database");
        assert!(
            !live_index_names(&db).iter().any(|n| n == "docs_status_idx"),
            "DROP INDEX did not remove the index on an encrypted database"
        );
    }

    // Session 3 — the drop is durable on an encrypted data dir too.
    {
        let db = EmbeddedDatabase::with_config(encrypted_config()).expect("reopen encrypted database");
        let live = live_index_names(&db);
        assert!(
            !live.iter().any(|n| n == "docs_status_idx"),
            "the dropped index came back on the encrypted data dir: {live:?}"
        );
        assert!(
            live.iter().any(|n| n == "docs_owner_idx"),
            "the drop took an unrelated index with it: {live:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    std::env::remove_var(KEY_VAR);
}

// ---------------------------------------------------------------------------
// 8. MySQL wire spelling
// ---------------------------------------------------------------------------

/// MySQL's only legal spelling is `DROP INDEX <i> ON <t>`. sqlparser 0.53 reads
/// `DROP INDEX <i>` and then chokes on the trailing `ON`, so before v4.21.0 the
/// MySQL wire could not drop an index at all. A parse-failure fallback rewrite
/// strips the qualifier (HeliosDB's index namespace is global, so the table
/// cannot disambiguate anything).
#[test]
fn mysql_drop_index_on_table_spelling_works() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");
        db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();

        run(&db, "DROP INDEX docs_status_idx ON docs", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] the MySQL DROP INDEX spelling must work: {e}"));

        assert!(
            !live_index_names(&db).iter().any(|n| n == "docs_status_idx"),
            "[{fam}] the MySQL spelling parsed but dropped nothing"
        );
        assert_eq!(
            db.query("SELECT id FROM docs", &[]).unwrap().len(),
            40,
            "[{fam}] the MySQL spelling touched the table"
        );
    }
}

/// The `IF EXISTS` variant of the MySQL spelling keeps IF EXISTS semantics.
#[test]
fn mysql_drop_index_if_exists_on_table_is_a_no_op_when_missing() {
    let db = memory_db();
    seed_docs(&db, "docs");

    db.execute("DROP INDEX IF EXISTS nothing_here ON docs")
        .expect("IF EXISTS + the MySQL qualifier must be a no-op");
    assert!(
        db.execute("DROP INDEX nothing_here ON docs").is_err(),
        "without IF EXISTS the MySQL spelling must still report the missing index"
    );
    assert!(table_exists(&db, "docs"), "the table must be untouched");
}

/// A trailing MySQL `ALGORITHM=` / `LOCK=` option is NOT quietly discarded: the
/// rewrite declines and the original parse diagnostic stands. Accepting the
/// statement while dropping a clause the caller asked for is the silent-success
/// failure mode this repo keeps shipping.
#[test]
fn mysql_drop_index_with_trailing_options_is_rejected_loudly() {
    let db = memory_db();
    seed_docs(&db, "docs");
    db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();

    assert!(
        db.execute("DROP INDEX docs_status_idx ON docs ALGORITHM = INPLACE")
            .is_err(),
        "a trailing ALGORITHM clause must be rejected, not silently ignored"
    );
    assert!(
        live_index_names(&db).iter().any(|n| n == "docs_status_idx"),
        "the rejected statement must not have dropped anything"
    );
}

// ---------------------------------------------------------------------------
// 9. Multi-target DROP
// ---------------------------------------------------------------------------

/// `DROP INDEX a, b` plans as `DropMulti` over two `DropIndex` nodes.
///
/// The caches are WARMED first, on purpose. `plan_invalidates_sql_caches` sees
/// only the OUTER plan, and `DropMulti` was missing from its list — so the
/// comma-list spelling invalidated nothing while the single-target spelling of
/// the same statement invalidated correctly, and a `pg_indexes` read taken
/// before the drop kept being served afterwards. Reading `pg_indexes` and a
/// WHERE-on-an-indexed-column SELECT before the drop is what makes this test
/// able to fail; without the warm-up it passed either way.
#[test]
fn multiple_indexes_can_be_dropped_in_one_statement() {
    let db = memory_db();
    seed_docs(&db, "docs");
    db.execute("CREATE INDEX docs_status_idx ON docs (status)").unwrap();
    db.execute("CREATE INDEX docs_owner_idx ON docs (owner)").unwrap();

    // Warm the plan + result caches on queries whose answers the drop changes.
    let warm = live_index_names(&db);
    assert!(
        warm.iter().any(|n| n == "docs_status_idx") && warm.iter().any(|n| n == "docs_owner_idx"),
        "both indexes must be live before the drop: {warm:?}"
    );
    assert_eq!(
        db.query("SELECT id FROM docs WHERE status = 'open'", &[])
            .unwrap()
            .len(),
        20,
        "sanity: the indexed query answers 20 rows before the drop"
    );

    db.execute("DROP INDEX docs_status_idx, docs_owner_idx")
        .expect("multi-target DROP INDEX must succeed");

    // The same indexed query must still answer correctly after its index is
    // gone (it falls back to a scan) — a stale cached PLAN naming the dropped
    // index is the failure this guards.
    assert_eq!(
        db.query("SELECT id FROM docs WHERE status = 'open'", &[])
            .unwrap()
            .len(),
        20,
        "a multi-target drop must not change query RESULTS"
    );

    let live = live_index_names(&db);
    assert!(
        !live.iter().any(|n| n == "docs_status_idx") && !live.iter().any(|n| n == "docs_owner_idx"),
        "multi-target DROP INDEX left one behind: {live:?}"
    );
    assert_eq!(db.query("SELECT id FROM docs", &[]).unwrap().len(), 40);
}
