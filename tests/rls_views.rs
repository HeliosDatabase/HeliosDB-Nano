//! Policies hold through views and materialized views.
//!
//! A view is read with the reader's rights. A materialized view's stored rows
//! are filtered by its base tables' policies when that is provably the same
//! as filtering the base rows first, and refused (42501) otherwise: never an
//! empty result, never the unfiltered rows.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]

use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Value};

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(v) => i64::from(*v),
        Value::Int4(v) => i64::from(*v),
        Value::Int8(v) => *v,
        other => panic!("not an integer: {other:?}"),
    }
}

fn session(db: &EmbeddedDatabase, user: &str) -> SessionId {
    db.create_session(user, IsolationLevel::ReadCommitted).unwrap()
}

/// First column, sorted, through both statement families.
fn ids(db: &EmbeddedDatabase, s: SessionId, sql: &str) -> Vec<i64> {
    let (text_rows, _) = db
        .query_with_columns_for_session(s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut text: Vec<i64> = text_rows.iter().map(|r| int(&r.values[0])).collect();
    let mut params: Vec<i64> = db
        .query_params_for_session(s, sql, &[])
        .unwrap_or_else(|e| panic!("{sql} (params): {e}"))
        .iter()
        .map(|r| int(&r.values[0]))
        .collect();
    text.sort_unstable();
    params.sort_unstable();
    assert_eq!(text, params, "the two statement families disagree on `{sql}`");
    text
}

/// The error of a read, which both statement families must raise.
fn refusal(db: &EmbeddedDatabase, s: SessionId, sql: &str) -> String {
    let text = db
        .query_with_columns_for_session(s, sql)
        .expect_err(sql)
        .to_string();
    let params = db.query_params_for_session(s, sql, &[]).expect_err(sql).to_string();
    assert_eq!(text, params, "the two statement families disagree on `{sql}`");
    text
}

/// docs: 1, 2 alice (eng, ops); 3, 4 bob (eng, ops); 5 carol (eng).
/// tags: one per doc. alice and bob see their own docs.
fn db() -> (EmbeddedDatabase, SessionId, SessionId) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for role in ["alice", "bob"] {
        db.execute(&format!("CREATE ROLE {role} LOGIN")).unwrap();
    }
    db.execute("CREATE ROLE auditor BYPASSRLS LOGIN").unwrap();
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, dept TEXT)").unwrap();
    db.execute(
        "INSERT INTO docs VALUES (1, 'alice', 'eng'), (2, 'alice', 'ops'), (3, 'bob', 'eng'), \
         (4, 'bob', 'ops'), (5, 'carol', 'eng')",
    )
    .unwrap();
    db.execute("CREATE TABLE tags (doc_id INT PRIMARY KEY, tag TEXT)").unwrap();
    db.execute("INSERT INTO tags VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')")
        .unwrap();
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    (db, alice, bob)
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[test]
fn a_view_is_read_with_the_readers_rights() {
    let (db, alice, bob) = db();
    db.execute("CREATE VIEW all_docs AS SELECT id, dept FROM docs").unwrap();
    db.execute("CREATE VIEW eng_docs AS SELECT id FROM all_docs WHERE dept = 'eng'").unwrap();
    db.execute("CREATE VIEW tagged AS SELECT d.id, t.tag FROM docs d JOIN tags t ON t.doc_id = d.id")
        .unwrap();
    db.execute("CREATE VIEW doc_count AS SELECT count(*) AS n FROM docs").unwrap();
    // The view does not carry the policy's column, and is still filtered.
    assert_eq!(ids(&db, alice, "SELECT id FROM all_docs"), vec![1, 2]);
    assert_eq!(ids(&db, bob, "SELECT id FROM all_docs"), vec![3, 4]);
    assert_eq!(ids(&db, alice, "SELECT id FROM eng_docs"), vec![1]);
    assert_eq!(ids(&db, bob, "SELECT id FROM tagged"), vec![3, 4]);
    assert_eq!(ids(&db, alice, "SELECT n FROM doc_count"), vec![2]);
    assert_eq!(ids(&db, alice, "SELECT v.id FROM all_docs v JOIN tags t ON t.doc_id = v.id"), vec![1, 2]);
    assert_eq!(
        ids(&db, alice, "SELECT doc_id FROM tags WHERE doc_id IN (SELECT id FROM all_docs)"),
        vec![1, 2]
    );
    // An exempt role and the embedded handle read everything.
    let auditor = session(&db, "auditor");
    assert_eq!(ids(&db, auditor, "SELECT id FROM all_docs"), vec![1, 2, 3, 4, 5]);
    assert_eq!(db.query("SELECT id FROM all_docs", &[]).unwrap().len(), 5);
}

#[test]
fn security_invoker_is_the_name_of_that_behaviour() {
    let (db, alice, _) = db();
    db.execute("CREATE VIEW v_on WITH (security_invoker = true) AS SELECT id FROM docs")
        .unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM v_on"), vec![1, 2]);
    // The owner's-rights form is refused, not accepted and ignored.
    let e = db
        .execute("CREATE VIEW v_off WITH (security_invoker = false) AS SELECT id FROM docs")
        .unwrap_err()
        .to_string();
    assert!(e.contains("security_invoker = false is not supported with row-level security"), "{e}");
    assert!(db.query("SELECT * FROM v_off", &[]).is_err());
    let e = db
        .execute("CREATE VIEW v_bad WITH (security_invoker = maybe) AS SELECT id FROM docs")
        .unwrap_err()
        .to_string();
    assert!(e.contains("security_invoker"), "{e}");
}

// ---------------------------------------------------------------------------
// Materialized views: accepted shapes
// ---------------------------------------------------------------------------

#[test]
fn a_materialized_view_that_keeps_the_policy_column_is_filtered() {
    let (db, alice, bob) = db();
    db.execute("CREATE MATERIALIZED VIEW mv_plain AS SELECT id, owner FROM docs").unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv_filtered AS SELECT id, owner, dept FROM docs WHERE dept = 'eng'")
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW mv_star AS SELECT * FROM docs").unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM mv_plain"), vec![1, 2]);
    assert_eq!(ids(&db, bob, "SELECT id FROM mv_plain"), vec![3, 4]);
    assert_eq!(ids(&db, alice, "SELECT id FROM mv_filtered"), vec![1]);
    assert_eq!(ids(&db, bob, "SELECT id FROM mv_star"), vec![3, 4]);
    // Through a projection that leaves the column out, a point read, a count
    // and a join.
    assert_eq!(ids(&db, alice, "SELECT id FROM mv_plain WHERE id = 3"), Vec::<i64>::new());
    assert_eq!(ids(&db, alice, "SELECT count(*) FROM mv_plain"), vec![2]);
    assert_eq!(
        ids(&db, alice, "SELECT m.id FROM mv_plain m JOIN tags t ON t.doc_id = m.id"),
        vec![1, 2]
    );
    // The table that holds the view's rows, read by its own name, is the view.
    assert_eq!(ids(&db, alice, "SELECT id FROM __mv_mv_plain"), vec![1, 2]);
    // Exempt readers get the stored rows.
    let auditor = session(&db, "auditor");
    assert_eq!(ids(&db, auditor, "SELECT id FROM mv_plain"), vec![1, 2, 3, 4, 5]);
    assert_eq!(db.query("SELECT id FROM mv_plain", &[]).unwrap().len(), 5);
}

#[test]
fn a_grouped_materialized_view_is_filtered_when_the_column_is_a_grouping_key() {
    let (db, alice, bob) = db();
    db.execute("CREATE MATERIALIZED VIEW per_owner AS SELECT owner, count(*) AS n FROM docs GROUP BY owner")
        .unwrap();
    assert_eq!(ids(&db, alice, "SELECT n FROM per_owner"), vec![2]);
    assert_eq!(ids(&db, bob, "SELECT n FROM per_owner"), vec![2]);
    assert_eq!(db.query("SELECT n FROM per_owner", &[]).unwrap().len(), 3);
    db.execute(
        "CREATE MATERIALIZED VIEW per_owner_dept AS SELECT owner, dept, count(*) AS n FROM docs GROUP BY owner, dept",
    )
    .unwrap();
    assert_eq!(ids(&db, alice, "SELECT n FROM per_owner_dept"), vec![1, 1]);
}

#[test]
fn a_materialized_view_over_an_inner_join_is_filtered() {
    let (db, alice, _) = db();
    db.execute(
        "CREATE MATERIALIZED VIEW tagged AS SELECT d.id, d.owner, t.tag FROM docs d JOIN tags t ON t.doc_id = d.id",
    )
    .unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM tagged"), vec![1, 2]);
}

#[test]
fn a_materialized_view_over_a_materialized_view_is_filtered() {
    let (db, alice, _) = db();
    db.execute("CREATE MATERIALIZED VIEW level1 AS SELECT id, owner, dept FROM docs").unwrap();
    db.execute("CREATE MATERIALIZED VIEW level2 AS SELECT id, owner FROM level1 WHERE dept = 'eng'")
        .unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM level2"), vec![1]);
    db.execute("CREATE MATERIALIZED VIEW level2_lost AS SELECT id FROM level1").unwrap();
    let e = refusal(&db, alice, "SELECT id FROM level2_lost");
    assert!(e.contains("level2_lost") && e.contains("\"owner\""), "{e}");
}

#[test]
fn default_deny_and_role_specific_policies_reach_a_materialized_view() {
    let (db, alice, bob) = db();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, owner, dept FROM docs").unwrap();
    db.execute("DROP POLICY own ON docs").unwrap();
    // Enabled, no policy: nothing, as on the table.
    assert_eq!(ids(&db, alice, "SELECT id FROM mv"), Vec::<i64>::new());
    db.execute("CREATE POLICY eng_for_alice ON docs TO alice USING (dept = 'eng')").unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM mv"), vec![1, 3, 5]);
    assert_eq!(ids(&db, bob, "SELECT id FROM mv"), Vec::<i64>::new());
    // Not enabled: stored rows, unfiltered.
    db.execute("ALTER TABLE docs DISABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(ids(&db, bob, "SELECT id FROM mv"), vec![1, 2, 3, 4, 5]);
}

#[test]
fn the_owner_of_the_base_table_reads_the_materialized_view_unfiltered() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE ROLE alice LOGIN").unwrap();
    db.execute("CREATE ROLE bob LOGIN").unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    db.execute_for_session(alice, "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT)")
        .unwrap();
    db.execute_for_session(alice, "INSERT INTO docs VALUES (1, 'alice'), (2, 'bob')")
        .unwrap();
    db.execute_for_session(alice, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.execute_for_session(alice, "CREATE POLICY own ON docs USING (owner = current_user)")
        .unwrap();
    db.execute("CREATE MATERIALIZED VIEW lost AS SELECT id FROM docs").unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM lost"), vec![1, 2]);
    assert!(refusal(&db, bob, "SELECT id FROM lost").contains("row-level security"));
}

// ---------------------------------------------------------------------------
// Materialized views: refused shapes
// ---------------------------------------------------------------------------

/// Create `definition` as materialized view `mv` and return what a subject
/// role is told when it reads it.
fn refused(definition: &str) -> String {
    let (db, alice, _) = db();
    db.execute(&format!("CREATE MATERIALIZED VIEW mv AS {definition}"))
        .unwrap_or_else(|e| panic!("{definition}: {e}"));
    let e = refusal(&db, alice, "SELECT * FROM mv");
    // Always the same frame: the view, the table, the policy.
    assert!(
        e.contains("cannot read materialized view \"mv\" as alice")
            && e.contains("row-level security policy \"own\" of table \"docs\""),
        "{definition}: {e}"
    );
    // Never an empty result or the unfiltered rows, on any shape of read.
    for sql in ["SELECT count(*) FROM mv", "SELECT 1 FROM mv LIMIT 1"] {
        assert!(db.query_with_columns_for_session(alice, sql).is_err(), "{definition}: {sql}");
        assert!(db.query_params_for_session(alice, sql, &[]).is_err(), "{definition}: {sql}");
    }
    // An exempt role reads it normally.
    let auditor = session(&db, "auditor");
    db.query_with_columns_for_session(auditor, "SELECT * FROM mv").unwrap();
    db.query("SELECT * FROM mv", &[]).unwrap();
    e
}

#[test]
fn refused_the_column_is_not_in_the_output() {
    let e = refused("SELECT id, dept FROM docs");
    assert!(e.contains("column \"owner\" is not preserved: it is not in the view's output"), "{e}");
}

#[test]
fn refused_the_column_is_renamed() {
    let e = refused("SELECT id, owner AS who FROM docs");
    assert!(
        e.contains("column \"owner\" is not preserved: the view's output has it under the name \"who\""),
        "{e}"
    );
}

#[test]
fn refused_the_column_is_computed() {
    let e = refused("SELECT id, upper(owner) AS owner FROM docs");
    assert!(e.contains("column \"owner\" is not preserved"), "{e}");
}

#[test]
fn refused_an_aggregate_that_does_not_group_by_the_column() {
    let e = refused("SELECT dept, count(*) AS n FROM docs GROUP BY dept");
    assert!(e.contains("column \"owner\" is not preserved"), "{e}");
}

#[test]
fn refused_an_aggregate_over_all_rows() {
    let e = refused("SELECT count(*) AS n FROM docs");
    assert!(e.contains("\"owner\"") || e.contains("no GROUP BY"), "{e}");
}

#[test]
fn refused_a_limit() {
    let e = refused("SELECT id, owner FROM docs ORDER BY id LIMIT 3");
    assert!(e.contains("LIMIT"), "{e}");
}

#[test]
fn refused_a_union_that_removes_duplicates() {
    let e = refused("SELECT id, owner FROM docs UNION SELECT id, owner FROM docs");
    assert!(e.contains("UNION"), "{e}");
}

#[test]
fn refused_a_union_all_branch_that_does_not_read_the_table() {
    let e = refused("SELECT id, owner FROM docs UNION ALL SELECT doc_id, tag FROM tags");
    assert!(e.contains("UNION ALL branch"), "{e}");
}

#[test]
fn refused_an_outer_join_that_can_null_the_table() {
    let e = refused("SELECT t.doc_id, d.owner FROM tags t LEFT JOIN docs d ON d.id = t.doc_id");
    assert!(e.contains("outer join"), "{e}");
}

#[test]
fn refused_the_table_read_twice() {
    let e = refused("SELECT a.id, a.owner FROM docs a JOIN docs b ON a.id = b.id");
    assert!(e.contains("more than once") || e.contains("\"owner\""), "{e}");
}

#[test]
fn refused_a_subquery_over_the_table() {
    let e = refused("SELECT doc_id, tag FROM tags WHERE doc_id IN (SELECT id FROM docs)");
    assert!(e.contains("subquery of the view reads the table") || e.contains("\"owner\""), "{e}");
}

#[test]
fn a_union_all_of_branches_that_each_keep_the_column_is_accepted() {
    let (db, alice, _) = db();
    db.execute(
        "CREATE MATERIALIZED VIEW both_depts AS \
         SELECT id, owner FROM docs WHERE dept = 'eng' UNION ALL SELECT id, owner FROM docs WHERE dept = 'ops'",
    )
    .unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM both_depts"), vec![1, 2]);
}

#[test]
fn a_refresh_keeps_the_stored_rows_complete_and_the_reads_filtered() {
    let (db, alice, bob) = db();
    db.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, owner FROM docs").unwrap();
    db.execute("INSERT INTO docs VALUES (6, 'alice', 'eng'), (7, 'bob', 'eng')").unwrap();
    // Refreshed by a subject role: the stored rows are still all rows.
    db.execute_for_session(alice, "REFRESH MATERIALIZED VIEW mv").unwrap();
    assert_eq!(db.query("SELECT id FROM mv", &[]).unwrap().len(), 7);
    assert_eq!(ids(&db, alice, "SELECT id FROM mv"), vec![1, 2, 6]);
    assert_eq!(ids(&db, bob, "SELECT id FROM mv"), vec![3, 4, 7]);
}
