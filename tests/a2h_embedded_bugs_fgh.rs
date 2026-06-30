//! Regression coverage for the a2h embedded-engine bugs F, G, H — surfaced by
//! a2h (any2heliosdb) dogfooding HeliosDB-Nano as its manifest store.
//!
//! F: UPDATE/DELETE (and a latent SELECT) whose WHERE constrains only a leading
//!    PREFIX of a COMPOSITE PRIMARY KEY matched 0 rows, because the single-value
//!    PK-index fast paths probed the composite index with a one-value key.
//! G: execute_many (and multi-row VALUES / COPY) falsely rejected composite-
//!    DISTINCT keys as a UNIQUE violation, because intra-batch dedup keyed on
//!    each PK column individually instead of the full composite key.
//! H: `<col> IS [NOT] JSON` (SQL:2016 / Oracle) failed to parse (sqlparser 0.53
//!    has no IS JSON support), blocking an Oracle->HeliosDB migrate.

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// BUG F — composite-PK prefix UPDATE / DELETE / SELECT
// ---------------------------------------------------------------------------

#[test]
fn bug_f_composite_pk_prefix_update_delete() {
    let db = EmbeddedDatabase::new_in_memory().expect("create db");
    db.execute(
        "CREATE TABLE c (run_id TEXT, chunk_id TEXT, state TEXT, PRIMARY KEY(run_id, chunk_id))",
    )
    .unwrap();
    db.execute("INSERT INTO c (run_id, chunk_id, state) VALUES ('r1','c0','pending')")
        .unwrap();
    db.execute("INSERT INTO c (run_id, chunk_id, state) VALUES ('r1','c1','pending')")
        .unwrap();
    db.execute("INSERT INTO c (run_id, chunk_id, state) VALUES ('r2','c0','pending')")
        .unwrap();

    // Prefix SELECT already matched the 2 rows (the reference behaviour).
    let rows = db.query("SELECT * FROM c WHERE run_id = 'r1'", &[]).unwrap();
    assert_eq!(rows.len(), 2, "prefix SELECT must see 2 rows");

    // Prefix UPDATE must change exactly the 2 rows with run_id = 'r1'.
    let updated = db.execute("UPDATE c SET state = 'x' WHERE run_id = 'r1'").unwrap();
    assert_eq!(updated, 2, "prefix UPDATE must change 2 rows (BUG F)");
    assert_eq!(
        db.query("SELECT * FROM c WHERE state = 'x'", &[]).unwrap().len(),
        2
    );

    // Prefix DELETE must remove exactly the 2 rows with run_id = 'r1'.
    let deleted = db.execute("DELETE FROM c WHERE run_id = 'r1'").unwrap();
    assert_eq!(deleted, 2, "prefix DELETE must remove 2 rows (BUG F)");
    assert_eq!(
        db.query("SELECT * FROM c", &[]).unwrap().len(),
        1,
        "only the r2 row remains"
    );

    // Parameterized ($1) prefix DELETE shares fast_pk_expr_from_selection.
    db.execute("INSERT INTO c (run_id, chunk_id, state) VALUES ('r1','c0','pending')")
        .unwrap();
    db.execute("INSERT INTO c (run_id, chunk_id, state) VALUES ('r1','c1','pending')")
        .unwrap();
    let pdel = db
        .execute_params("DELETE FROM c WHERE run_id = $1", &[Value::String("r1".into())])
        .unwrap();
    assert_eq!(pdel, 2, "parameterized prefix DELETE must remove 2 rows (BUG F)");

    // Full composite-PK DELETE must still work.
    let d2 = db
        .execute("DELETE FROM c WHERE run_id = 'r2' AND chunk_id = 'c0'")
        .unwrap();
    assert_eq!(d2, 1, "full composite-PK DELETE removes 1 row");
    assert!(db.query("SELECT * FROM c", &[]).unwrap().is_empty());
}

/// The fix must NOT disturb the single-column-PK fast paths (the hot OLTP shape
/// and what the point-lookup benchmarks target): point SELECT/UPDATE/DELETE on a
/// sole PK column still resolve via the fast path and affect exactly one row.
#[test]
fn bug_f_single_column_pk_fast_path_preserved() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE k (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO k VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO k VALUES (2, 'b')").unwrap();

    assert_eq!(db.query("SELECT v FROM k WHERE id = 1", &[]).unwrap().len(), 1);
    assert_eq!(db.execute("UPDATE k SET v = 'z' WHERE id = 1").unwrap(), 1);
    assert_eq!(db.execute("DELETE FROM k WHERE id = 2").unwrap(), 1);
    assert_eq!(db.query("SELECT * FROM k", &[]).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// BUG G — execute_many of composite-distinct keys
// ---------------------------------------------------------------------------

#[test]
fn bug_g_execute_many_composite_distinct_keys() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE c (run_id TEXT, chunk_id TEXT, PRIMARY KEY(run_id, chunk_id))")
        .unwrap();

    // Three DISTINCT composite keys that pairwise share one column value:
    // (r1,c0)&(r1,c1) share run_id; (r1,c0)&(r2,c0) share chunk_id.
    let rows = vec![
        vec![Value::String("r1".into()), Value::String("c0".into())],
        vec![Value::String("r1".into()), Value::String("c1".into())],
        vec![Value::String("r2".into()), Value::String("c0".into())],
    ];
    let n = db
        .execute_many_params("INSERT INTO c (run_id, chunk_id) VALUES ($1, $2)", &rows)
        .expect("distinct composite keys must all insert (BUG G)");
    assert_eq!(n, 3, "all three distinct composite keys insert");
    assert_eq!(db.query("SELECT * FROM c", &[]).unwrap().len(), 3);

    // NEGATIVE: a genuine duplicate composite key in the batch is still rejected,
    // and must not leave partial rows behind.
    db.execute("CREATE TABLE c2 (run_id TEXT, chunk_id TEXT, PRIMARY KEY(run_id, chunk_id))")
        .unwrap();
    let dup = vec![
        vec![Value::String("r1".into()), Value::String("c0".into())],
        vec![Value::String("r2".into()), Value::String("c9".into())],
        vec![Value::String("r1".into()), Value::String("c0".into())], // exact composite dup
    ];
    assert!(
        db.execute_many_params("INSERT INTO c2 (run_id, chunk_id) VALUES ($1, $2)", &dup)
            .is_err(),
        "a true duplicate composite key must still be rejected"
    );
    assert_eq!(
        db.query("SELECT * FROM c2", &[]).unwrap().len(),
        0,
        "a failed batch must not write partial rows"
    );

    // Single-column PK intra-batch dedup must still catch true duplicates.
    db.execute("CREATE TABLE s (id INT PRIMARY KEY)").unwrap();
    let sdup = vec![vec![Value::Int8(1)], vec![Value::Int8(1)]];
    assert!(
        db.execute_many_params("INSERT INTO s (id) VALUES ($1)", &sdup)
            .is_err(),
        "duplicate single-column PK in a batch must be rejected"
    );
}

// ---------------------------------------------------------------------------
// BUG H — `<col> IS [NOT] JSON`
// ---------------------------------------------------------------------------

#[test]
fn bug_h_is_json_check_constraint_parses_and_is_permissive() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    // The exact statement from the a2h migrate must now parse + execute.
    db.execute(
        "CREATE TABLE u (id INT PRIMARY KEY, mfa VARCHAR(1000), \
         CONSTRAINT mfa_is_json CHECK (mfa is json))",
    )
    .unwrap();

    // Valid JSON passes; NULL passes too (IS JSON treats NULL as satisfied —
    // the migrate must never reject a NULL row).
    db.execute("INSERT INTO u VALUES (1, '{}')").unwrap();
    db.execute("INSERT INTO u VALUES (2, NULL)").unwrap();
    assert_eq!(db.query("SELECT id FROM u", &[]).unwrap().len(), 2);
}

#[test]
fn bug_h_is_json_bare_expression_semantics() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE j (id INT, doc TEXT)").unwrap();
    db.execute("INSERT INTO j VALUES (1, '{}')").unwrap(); // valid JSON
    db.execute("INSERT INTO j VALUES (2, 'not json')").unwrap(); // invalid
    db.execute("INSERT INTO j VALUES (3, NULL)").unwrap(); // null

    let r = db
        .query("SELECT id, doc IS JSON AS v FROM j ORDER BY id", &[])
        .unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].values[1], Value::Boolean(true), "'{{}}' IS JSON => true");
    assert_eq!(r[1].values[1], Value::Boolean(false), "'not json' IS JSON => false");
    assert_eq!(r[2].values[1], Value::Null, "NULL IS JSON => NULL");

    // IS NOT JSON is the negation (NULL stays NULL).
    let rn = db
        .query("SELECT id, doc IS NOT JSON AS v FROM j ORDER BY id", &[])
        .unwrap();
    assert_eq!(rn[0].values[1], Value::Boolean(false), "valid -> NOT JSON false");
    assert_eq!(rn[1].values[1], Value::Boolean(true), "invalid -> NOT JSON true");
    assert_eq!(rn[2].values[1], Value::Null, "NULL IS NOT JSON => NULL");
}

/// A string literal that merely contains the words "is json" must be left
/// untouched by the pre-parser rewrite (quote-awareness guard).
#[test]
fn bug_h_is_json_does_not_corrupt_string_literals() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    let s = db.query("SELECT 'this is json text' AS s", &[]).unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].values[0], Value::String("this is json text".to_string()));
}
