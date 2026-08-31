//! GH#16 — `CREATE INDEX` without an explicit index name.
//!
//! # The report
//!
//! ```sql
//! CREATE INDEX ON items USING hnsw (embedding vector_cosine_ops);
//! ```
//!
//! That is pgvector's README spelling — the first statement anyone running a
//! vector store types. On v4.21.0 it failed outright:
//!
//! ```text
//! ERROR: Query execution error: Index name is required
//! ```
//!
//! PostgreSQL makes the name OPTIONAL and derives one from the table and the
//! indexed columns (`ChooseRelationName`): `{table}_{col}_idx`, or
//! `{table}_{col1}_{col2}_idx` for a multi-column index, uniquified with `1`,
//! `2`, … when that name is already taken.
//!
//! # What these tests pin
//!
//! 1. The name is generated for BOTH index families — the ART/btree branch and
//!    the vector/HNSW branch of `executor::ddl::handle_create_index`. Generating
//!    it in only one is the half-fix shape this repo keeps shipping, and it is
//!    exactly the half a pgvector user would hit.
//! 2. The generated name is a FIRST-CLASS name: visible in `pg_indexes`,
//!    persisted like an explicit one (so it survives a reopen), and DROPPABLE by
//!    `DROP INDEX`.
//! 3. It never lands in `art_manager`'s CONSTRAINT namespace (`{table}_pkey`,
//!    `{table}_{cols}_key`, `{table}_{cols}_fkey`) — a name that did would be
//!    refused by `DROP INDEX`'s constraint guard, leaving the user with an index
//!    they could create but never remove.
//! 4. `IF NOT EXISTS` with no name is REFUSED, as PostgreSQL refuses it. A
//!    generated name is unique by construction, so the clause could never
//!    suppress anything; accepting it would be a silently-ignored clause.
//!
//! # Both executor families, always
//!
//!   * text family   — `db.execute()`        → `execute_in_transaction_inner`
//!   * params family — `db.execute_params()` → `execute_plan_with_params_inner`
//!                     (the PG extended protocol: psycopg, JDBC, sqlx,
//!                      node-postgres; plus REST/BaaS)
//!
//! Both reach the SAME `Statement::CreateIndex` planner arm, which is where the
//! name is generated — so every test below runs on both.

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

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nano_genidx_{tag}_{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Index names currently visible through `pg_indexes` — the LIVE ART / vector
/// registrations, not the catalog records. This is the surface a user (and
/// every introspection tool) discovers a generated name through, so it is also
/// how the tests learn the name.
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

fn has_index(db: &EmbeddedDatabase, name: &str) -> bool {
    live_index_names(db).iter().any(|n| n == name)
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

fn seed_items(db: &EmbeddedDatabase, table: &str) {
    db.execute(&format!(
        "CREATE TABLE {table} (id INT PRIMARY KEY, embedding VECTOR(3))"
    ))
    .unwrap();
    for (i, literal) in ["[1.0,0.0,0.0]", "[0.0,1.0,0.0]", "[0.0,0.0,1.0]"].iter().enumerate() {
        db.execute(&format!(
            "INSERT INTO {table} (id, embedding) VALUES ({i}, CAST('{literal}' AS VECTOR(3)))"
        ))
        .unwrap();
    }
}

// ---------------------------------------------------------------------------
// 1. The ART / btree branch
// ---------------------------------------------------------------------------

/// `CREATE INDEX ON docs (status)` — no name, no `USING`, so this lands on the
/// ART branch of `handle_create_index`. PostgreSQL calls the result
/// `docs_status_idx`; so must we, and it must be visible in `pg_indexes`.
#[test]
fn unnamed_index_on_a_plain_column_is_named_table_column_idx() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        run(&db, "CREATE INDEX ON docs (status)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] unnamed CREATE INDEX must succeed (GH#16): {e}"));

        assert!(
            has_index(&db, "docs_status_idx"),
            "[{fam}] the generated name docs_status_idx is not in pg_indexes: {:?}",
            live_index_names(&db)
        );

        // The index must actually be usable, not merely named: same rows as
        // before it existed.
        assert_eq!(
            db.query("SELECT id FROM docs WHERE status = 'open'", &[])
                .unwrap()
                .len(),
            20,
            "[{fam}] the generated index changed query RESULTS"
        );
    }
}

/// Multi-column: PostgreSQL joins every indexed column into the name.
/// (Only the leading column is actually indexed by this engine, but the NAME
/// must still describe the statement the user wrote — otherwise
/// `CREATE INDEX ON docs (status, owner)` and `CREATE INDEX ON docs (status)`
/// would fight over one name.)
#[test]
fn unnamed_multi_column_index_names_every_column() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        run(&db, "CREATE INDEX ON docs (status, owner)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] unnamed multi-column CREATE INDEX must succeed: {e}"));

        assert!(
            has_index(&db, "docs_status_owner_idx"),
            "[{fam}] expected docs_status_owner_idx: {:?}",
            live_index_names(&db)
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The vector / HNSW branch — the reported statement
// ---------------------------------------------------------------------------

/// *** THE REPORTED STATEMENT. ***
///
/// pgvector's README, verbatim apart from the metric: no index name, `USING
/// hnsw`, an operator class on the column. This is what the reporter ran in
/// their first five minutes.
///
/// It exercises the OTHER branch of `handle_create_index` (the
/// `VectorIndexManager`, not the ART manager). A fix that generated a name for
/// only the ART branch would leave this exact statement still broken.
#[test]
fn unnamed_hnsw_index_on_a_vector_column_is_named_and_registered() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_items(&db, "items");

        run(
            &db,
            "CREATE INDEX ON items USING hnsw (embedding vector_cosine_ops)",
            params_family,
        )
        .unwrap_or_else(|e| panic!("[{fam}] the reported pgvector statement must succeed (GH#16): {e}"));

        assert!(
            has_index(&db, "items_embedding_idx"),
            "[{fam}] expected the generated HNSW index items_embedding_idx: {:?}",
            live_index_names(&db)
        );
    }
}

/// The generated HNSW name is droppable — i.e. it was PERSISTED by
/// `persist_index_definition` exactly like an explicit one. `DROP INDEX`
/// dispatches on that persisted definition, so a generated name that was
/// registered but not persisted would produce an index nobody could ever
/// remove (and that would vanish at the next restart).
#[test]
fn a_generated_hnsw_name_can_be_dropped() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_items(&db, "items");

        run(
            &db,
            "CREATE INDEX ON items USING hnsw (embedding vector_cosine_ops)",
            params_family,
        )
        .unwrap();
        assert!(has_index(&db, "items_embedding_idx"), "[{fam}] sanity: index created");

        run(&db, "DROP INDEX items_embedding_idx", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP INDEX of a GENERATED name must succeed: {e}"));

        assert!(
            !has_index(&db, "items_embedding_idx"),
            "[{fam}] the generated HNSW index is still registered after DROP INDEX: {:?}",
            live_index_names(&db)
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Collision handling
// ---------------------------------------------------------------------------

/// Two unnamed indexes on the SAME column must not fight over one name.
/// PostgreSQL appends `1`, `2`, …; so does this.
#[test]
fn two_unnamed_indexes_on_the_same_column_get_distinct_names() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        run(&db, "CREATE INDEX ON docs (status)", params_family).unwrap();
        run(&db, "CREATE INDEX ON docs (status)", params_family).unwrap_or_else(|e| {
            panic!("[{fam}] a SECOND unnamed index on the same column must get a fresh name, not collide: {e}")
        });

        let live = live_index_names(&db);
        assert!(
            live.iter().any(|n| n == "docs_status_idx"),
            "[{fam}] expected docs_status_idx: {live:?}"
        );
        assert!(
            live.iter().any(|n| n == "docs_status_idx1"),
            "[{fam}] expected the uniquified docs_status_idx1: {live:?}"
        );
    }
}

/// An EXPLICIT index already sitting on the generated name is respected too:
/// the generator does not overwrite it, it steps around it.
#[test]
fn a_generated_name_steps_around_an_explicit_index_of_that_name() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        run(&db, "CREATE INDEX docs_owner_idx ON docs (owner)", params_family).unwrap();
        run(&db, "CREATE INDEX ON docs (owner)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] must step around the explicit docs_owner_idx: {e}"));

        let live = live_index_names(&db);
        assert!(
            live.iter().any(|n| n == "docs_owner_idx") && live.iter().any(|n| n == "docs_owner_idx1"),
            "[{fam}] expected both docs_owner_idx and docs_owner_idx1: {live:?}"
        );
    }
}

/// The generated name must stay OUT of `art_manager`'s constraint namespace.
/// A constraint index (`{table}_pkey`, `{table}_{cols}_key`,
/// `{table}_{cols}_fkey`) is what ENFORCES a PRIMARY KEY / UNIQUE / FOREIGN
/// KEY, and `DROP INDEX` refuses to drop one — so a generated name that landed
/// there would be an index the user could create and never remove, or worse,
/// a name shadowing a live constraint.
#[test]
fn a_generated_name_never_lands_in_the_constraint_namespace() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        db.execute("CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, city TEXT)")
            .unwrap();
        db.execute("INSERT INTO users (id, email, city) VALUES (1, 'a@x', 'lisbon')")
            .unwrap();
        db.execute("INSERT INTO users (id, email, city) VALUES (2, 'b@x', 'porto')")
            .unwrap();

        let before = live_index_names(&db);

        run(&db, "CREATE INDEX ON users (email)", params_family)
            .unwrap_or_else(|e| panic!("[{fam}] unnamed index on a UNIQUE column must succeed: {e}"));

        let after = live_index_names(&db);
        let generated: Vec<&String> = after.iter().filter(|n| !before.contains(*n)).collect();
        assert_eq!(
            generated.len(),
            1,
            "[{fam}] exactly one new index expected; before={before:?} after={after:?}"
        );
        let generated = generated[0];
        assert_eq!(
            generated, "users_email_idx",
            "[{fam}] unexpected generated name: {generated}"
        );
        assert!(
            !generated.ends_with("_pkey") && !generated.ends_with("_key") && !generated.ends_with("_fkey"),
            "[{fam}] the generated name {generated} is in the CONSTRAINT namespace"
        );

        // And the proof that it is not treated as one: DROP INDEX accepts it
        // (the constraint guard would have refused), while the PK index it must
        // not have disturbed is still there.
        run(&db, &format!("DROP INDEX {generated}"), params_family)
            .unwrap_or_else(|e| panic!("[{fam}] DROP INDEX {generated} was refused — constraint guard hit: {e}"));
        assert!(
            live_index_names(&db).iter().any(|n| n == "users_pkey"),
            "[{fam}] dropping the generated index took the PRIMARY KEY index with it: {:?}",
            live_index_names(&db)
        );
        assert_eq!(
            db.query("SELECT id FROM users", &[]).unwrap().len(),
            2,
            "[{fam}] rows changed"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. IF NOT EXISTS
// ---------------------------------------------------------------------------

/// `CREATE INDEX IF NOT EXISTS ON docs (status)` is REFUSED, exactly as
/// PostgreSQL refuses it ("IF NOT EXISTS requires that you name the index").
///
/// The clause asks "is the index called X already there?" — with no X there is
/// nothing to ask, and a generated name is unique by construction, so the
/// clause could never suppress anything. Accepting the statement would leave a
/// silently-ignored clause in the user's migration. The error may come from the
/// parser (sqlparser requires a name once it has consumed IF NOT EXISTS) or
/// from the planner's own guard; what matters is that it is never silently
/// accepted.
#[test]
fn if_not_exists_without_a_name_is_refused() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        let result = run(&db, "CREATE INDEX IF NOT EXISTS ON docs (status)", params_family);
        assert!(
            result.is_err(),
            "[{fam}] CREATE INDEX IF NOT EXISTS with no index name must be refused, not silently accepted"
        );
        assert!(
            live_index_names(&db).iter().all(|n| !n.starts_with("docs_status")),
            "[{fam}] a refused statement still created an index: {:?}",
            live_index_names(&db)
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Durability — a generated name is persisted like an explicit one
// ---------------------------------------------------------------------------

/// `Catalog::rebuild_all_indexes` re-registers user secondary indexes at open
/// from the persisted `meta:index:` records. If the generated name were only
/// registered in memory, the index would silently vanish at the next restart —
/// and `DROP INDEX` (which dispatches on the persisted definition) would then
/// report that it does not exist.
#[test]
fn a_generated_name_survives_a_reopen_and_is_still_droppable() {
    let dir = scratch_dir("reopen");

    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        seed_docs(&db, "docs");
        db.execute("CREATE INDEX ON docs (owner)").unwrap();
        assert!(
            has_index(&db, "docs_owner_idx"),
            "sanity: the generated index exists in the creating session"
        );
    }

    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        assert!(
            has_index(&db, "docs_owner_idx"),
            "the generated index did not survive a reopen — it was never persisted: {:?}",
            live_index_names(&db)
        );
        db.execute("DROP INDEX docs_owner_idx")
            .expect("a generated name must still be droppable after a reopen");
        assert!(
            !has_index(&db, "docs_owner_idx"),
            "the drop did nothing: {:?}",
            live_index_names(&db)
        );
    }

    {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        assert!(
            !has_index(&db, "docs_owner_idx"),
            "the dropped generated index was resurrected at open: {:?}",
            live_index_names(&db)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 6. The explicit-name path is unchanged
// ---------------------------------------------------------------------------

/// Regression guard: naming the index still does exactly what it did, including
/// through a schema qualifier (which collapses to the bare index name).
#[test]
fn an_explicit_index_name_is_still_used_verbatim() {
    for params_family in [false, true] {
        let db = memory_db();
        let fam = family_name(params_family);
        seed_docs(&db, "docs");

        run(&db, "CREATE INDEX my_status_idx ON docs (status)", params_family).unwrap();
        let live = live_index_names(&db);
        assert!(
            live.iter().any(|n| n == "my_status_idx"),
            "[{fam}] explicit name lost: {live:?}"
        );
        assert!(
            !live.iter().any(|n| n == "docs_status_idx"),
            "[{fam}] an explicit name must NOT also generate one: {live:?}"
        );
    }
}
