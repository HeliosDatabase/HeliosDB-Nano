//! Every way a statement can read a table is filtered: one case per plan
//! shape, each through the text family and the parameter family, as a role
//! that may see only its own rows.

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
        Value::Null => -1,
        other => panic!("not an integer: {other:?}"),
    }
}

fn session(db: &EmbeddedDatabase, user: &str) -> SessionId {
    db.create_session(user, IsolationLevel::ReadCommitted).unwrap()
}

/// First column of every row, through both statement families.
fn ids(db: &EmbeddedDatabase, s: SessionId, sql: &str) -> Vec<i64> {
    let (text_rows, _) = db
        .query_with_columns_for_session(s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let text: Vec<i64> = text_rows.iter().map(|r| int(&r.values[0])).collect();
    let params: Vec<i64> = db
        .query_params_for_session(s, sql, &[])
        .unwrap_or_else(|e| panic!("{sql} (params): {e}"))
        .iter()
        .map(|r| int(&r.values[0]))
        .collect();
    assert_eq!(text, params, "the two statement families disagree on `{sql}`");
    text
}

fn all(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| int(&r.values[0]))
        .collect()
}

/// docs: 1, 2 alice; 3, 4 bob; 5 carol. tags: one per doc, unprotected.
/// alice and bob see their own docs.
fn db() -> (EmbeddedDatabase, SessionId, SessionId) {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    for role in ["alice", "bob"] {
        db.execute(&format!("CREATE ROLE {role} LOGIN")).unwrap();
    }
    db.execute("CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, dept TEXT, parent INT)")
        .unwrap();
    db.execute(
        "INSERT INTO docs VALUES (1, 'alice', 'eng', NULL), (2, 'alice', 'ops', 1), (3, 'bob', 'eng', 1), \
         (4, 'bob', 'ops', 3), (5, 'carol', 'eng', 2)",
    )
    .unwrap();
    db.execute("CREATE INDEX docs_dept ON docs (dept)").unwrap();
    db.execute("CREATE TABLE tags (doc_id INT PRIMARY KEY, tag TEXT)").unwrap();
    db.execute("INSERT INTO tags VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')")
        .unwrap();
    db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    db.execute("CREATE POLICY own ON docs USING (owner = current_user)").unwrap();
    let alice = session(&db, "alice");
    let bob = session(&db, "bob");
    (db, alice, bob)
}

#[test]
fn scans_filters_projections_and_fast_paths() {
    let (db, alice, bob) = db();
    for (sql, expected) in [
        ("SELECT id FROM docs ORDER BY id", vec![1, 2]),
        ("SELECT * FROM docs ORDER BY id", vec![1, 2]),
        ("SELECT id FROM docs WHERE dept = 'eng' ORDER BY id", vec![1]),
        // A projection without the policy's column.
        ("SELECT id, dept FROM docs ORDER BY id", vec![1, 2]),
        // Primary-key point and range reads, the paths that skip the planner.
        ("SELECT id FROM docs WHERE id = 1", vec![1]),
        ("SELECT id FROM docs WHERE id = 3", vec![]),
        ("SELECT id FROM docs WHERE id IN (2, 3, 4)", vec![2]),
        ("SELECT id FROM docs WHERE id BETWEEN 2 AND 5 ORDER BY id", vec![2]),
        ("SELECT id FROM docs WHERE id > 1 ORDER BY id LIMIT 10", vec![2]),
        ("SELECT count(*) FROM docs", vec![2]),
        ("SELECT count(*) FROM docs WHERE dept = 'eng'", vec![1]),
        ("SELECT max(id) FROM docs", vec![2]),
        ("SELECT DISTINCT parent FROM docs WHERE parent IS NOT NULL ORDER BY 1", vec![1]),
        ("SELECT count(*) FROM docs GROUP BY dept ORDER BY 1", vec![1, 1]),
        ("SELECT id FROM docs ORDER BY id DESC LIMIT 1", vec![2]),
        ("SELECT id FROM docs ORDER BY id OFFSET 1", vec![2]),
        ("SELECT row_number() OVER (ORDER BY id) FROM docs", vec![1, 2]),
    ] {
        assert_eq!(ids(&db, alice, sql), expected, "{sql}");
    }
    assert_eq!(ids(&db, bob, "SELECT id FROM docs WHERE id = 3"), vec![3]);
    assert_eq!(ids(&db, bob, "SELECT id FROM docs WHERE id = 1"), Vec::<i64>::new());
    assert_eq!(ids(&db, bob, "SELECT count(*) FROM docs"), vec![2]);
    // A bound parameter on the primary key.
    for (id, expected) in [(1, vec![1]), (3, vec![])] {
        let rows = db
            .query_params_for_session(alice, "SELECT id FROM docs WHERE id = $1", &[Value::Int4(id)])
            .unwrap();
        assert_eq!(rows.iter().map(|r| int(&r.values[0])).collect::<Vec<_>>(), expected);
    }
}

#[test]
fn joins() {
    let (db, alice, _) = db();
    for (sql, expected) in [
        ("SELECT d.id FROM docs d JOIN tags t ON t.doc_id = d.id ORDER BY d.id", vec![1, 2]),
        ("SELECT t.doc_id FROM tags t JOIN docs d ON t.doc_id = d.id ORDER BY 1", vec![1, 2]),
        // The outer side is unprotected: its rows stay, the protected side is NULL.
        (
            "SELECT count(d.id) FROM tags t LEFT JOIN docs d ON t.doc_id = d.id",
            vec![2],
        ),
        ("SELECT count(*) FROM tags t LEFT JOIN docs d ON t.doc_id = d.id", vec![5]),
        // The table read twice: both reads are filtered.
        (
            "SELECT c.id FROM docs c JOIN docs p ON c.parent = p.id ORDER BY c.id",
            vec![2],
        ),
        ("SELECT count(*) FROM docs a, docs b", vec![4]),
        ("SELECT d.id FROM docs d, tags t WHERE t.doc_id = d.id AND t.tag > 'a' ORDER BY 1", vec![2]),
    ] {
        assert_eq!(ids(&db, alice, sql), expected, "{sql}");
    }
}

#[test]
fn subqueries_in_every_clause() {
    let (db, alice, _) = db();
    for (sql, expected) in [
        ("SELECT doc_id FROM tags WHERE doc_id IN (SELECT id FROM docs) ORDER BY 1", vec![1, 2]),
        ("SELECT doc_id FROM tags WHERE doc_id NOT IN (SELECT id FROM docs) ORDER BY 1", vec![3, 4, 5]),
        (
            "SELECT doc_id FROM tags t WHERE EXISTS (SELECT 1 FROM docs d WHERE d.id = t.doc_id) ORDER BY 1",
            vec![1, 2],
        ),
        ("SELECT (SELECT count(*) FROM docs)", vec![2]),
        ("SELECT doc_id FROM tags WHERE doc_id = (SELECT max(id) FROM docs)", vec![2]),
        ("SELECT s.id FROM (SELECT id FROM docs) AS s ORDER BY 1", vec![1, 2]),
        ("SELECT count(*) FROM (SELECT id FROM docs WHERE dept = 'eng') AS s", vec![1]),
        (
            "SELECT doc_id FROM tags GROUP BY doc_id HAVING doc_id IN (SELECT id FROM docs) ORDER BY 1",
            vec![1, 2],
        ),
        (
            "SELECT CASE WHEN doc_id IN (SELECT id FROM docs) THEN 1 ELSE 0 END FROM tags ORDER BY doc_id",
            vec![1, 1, 0, 0, 0],
        ),
    ] {
        assert_eq!(ids(&db, alice, sql), expected, "{sql}");
    }
}

#[test]
fn common_table_expressions() {
    let (db, alice, _) = db();
    for (sql, expected) in [
        ("WITH mine AS (SELECT id FROM docs) SELECT id FROM mine ORDER BY id", vec![1, 2]),
        (
            "WITH mine AS (SELECT id FROM docs), n AS (SELECT count(*) AS c FROM mine) SELECT c FROM n",
            vec![2],
        ),
        (
            "WITH RECURSIVE tree AS (SELECT id, parent FROM docs WHERE parent IS NULL \
             UNION ALL SELECT d.id, d.parent FROM docs d JOIN tree t ON d.parent = t.id) \
             SELECT id FROM tree ORDER BY id",
            vec![1, 2],
        ),
    ] {
        assert_eq!(ids(&db, alice, sql), expected, "{sql}");
    }
}

#[test]
fn set_operations() {
    let (db, alice, _) = db();
    for (sql, expected) in [
        ("SELECT id FROM docs UNION ALL SELECT id FROM docs ORDER BY 1", vec![1, 1, 2, 2]),
        ("SELECT id FROM docs UNION SELECT doc_id FROM tags WHERE doc_id = 5 ORDER BY 1", vec![1, 2, 5]),
        ("SELECT doc_id FROM tags INTERSECT SELECT id FROM docs ORDER BY 1", vec![1, 2]),
        ("SELECT doc_id FROM tags EXCEPT SELECT id FROM docs ORDER BY 1", vec![3, 4, 5]),
    ] {
        assert_eq!(ids(&db, alice, sql), expected, "{sql}");
    }
}

#[test]
fn insert_select_reads_only_visible_rows() {
    let (db, alice, bob) = db();
    db.execute("CREATE TABLE copy_text (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE copy_params (id INT PRIMARY KEY)").unwrap();
    assert_eq!(
        db.execute_for_session(alice, "INSERT INTO copy_text SELECT id FROM docs")
            .unwrap(),
        2
    );
    assert_eq!(
        db.execute_params_for_session(bob, "INSERT INTO copy_params SELECT id FROM docs", &[])
            .unwrap(),
        2
    );
    assert_eq!(all(&db, "SELECT id FROM copy_text ORDER BY id"), vec![1, 2]);
    assert_eq!(all(&db, "SELECT id FROM copy_params ORDER BY id"), vec![3, 4]);
    // CREATE TABLE AS likewise.
    db.execute_for_session(alice, "CREATE TABLE snapshot_of_docs AS SELECT id FROM docs")
        .unwrap();
    assert_eq!(all(&db, "SELECT id FROM snapshot_of_docs ORDER BY id"), vec![1, 2]);
}

#[test]
fn update_and_delete_predicates_and_sources() {
    let (db, alice, bob) = db();
    // A subquery over the protected table in the WHERE clause of a write to
    // another table.
    assert_eq!(
        db.execute_for_session(alice, "UPDATE tags SET tag = 'mine' WHERE doc_id IN (SELECT id FROM docs)")
            .unwrap(),
        2
    );
    assert_eq!(
        db.execute_params_for_session(bob, "DELETE FROM tags WHERE doc_id IN (SELECT id FROM docs)", &[])
            .unwrap(),
        2
    );
    assert_eq!(all(&db, "SELECT doc_id FROM tags ORDER BY 1"), vec![1, 2, 5]);
    // A subquery in SET.
    db.execute_for_session(alice, "UPDATE tags SET tag = (SELECT count(*)::text FROM docs) WHERE doc_id = 5")
        .unwrap();
    let tag = db.query("SELECT tag FROM tags WHERE doc_id = 5", &[]).unwrap();
    assert_eq!(tag[0].values[0], Value::String("2".to_string()));
    // The target itself: a primary-key write to an invisible row changes nothing.
    assert_eq!(db.execute_for_session(alice, "UPDATE docs SET dept = 'x' WHERE id = 3").unwrap(), 0);
    assert_eq!(
        db.execute_params_for_session(alice, "UPDATE docs SET dept = 'x' WHERE id = $1", &[Value::Int4(3)])
            .unwrap(),
        0
    );
    assert_eq!(db.execute_for_session(alice, "DELETE FROM docs WHERE id = 3").unwrap(), 0);
    assert_eq!(
        db.execute_params_for_session(alice, "DELETE FROM docs WHERE id = $1", &[Value::Int4(4)])
            .unwrap(),
        0
    );
    assert_eq!(db.execute_for_session(alice, "DELETE FROM docs WHERE id IN (2, 3, 4)").unwrap(), 1);
    assert_eq!(all(&db, "SELECT id FROM docs ORDER BY id"), vec![1, 3, 4, 5]);
    assert_eq!(all(&db, "SELECT id FROM docs WHERE dept = 'x'"), Vec::<i64>::new());
}

#[test]
fn returning_shows_only_rows_the_statement_could_touch() {
    let (db, alice, bob) = db();
    let (count, rows) = db
        .execute_returning_for_session(alice, "UPDATE docs SET dept = 'r' RETURNING id")
        .unwrap();
    assert_eq!(count, 2);
    let mut seen: Vec<i64> = rows.iter().map(|r| int(&r.values[0])).collect();
    seen.sort_unstable();
    assert_eq!(seen, vec![1, 2]);
    let (count, rows) = db
        .execute_params_returning_for_session(bob, "DELETE FROM docs WHERE id > $1 RETURNING id", &[Value::Int4(0)])
        .unwrap();
    assert_eq!(count, 2);
    let mut seen: Vec<i64> = rows.iter().map(|r| int(&r.values[0])).collect();
    seen.sort_unstable();
    assert_eq!(seen, vec![3, 4]);
    assert_eq!(all(&db, "SELECT id FROM docs ORDER BY id"), vec![1, 2, 5]);
}

#[test]
fn on_conflict_do_update_cannot_reach_an_invisible_row() {
    let (db, alice, _) = db();
    // Row 3 is bob's. alice's upsert onto it must not change it.
    let result = db.execute_for_session(
        alice,
        "INSERT INTO docs VALUES (3, 'alice', 'eng', NULL) ON CONFLICT (id) DO UPDATE SET owner = 'alice'",
    );
    let owner = db.query("SELECT owner FROM docs WHERE id = 3", &[]).unwrap();
    assert_eq!(owner[0].values[0], Value::String("bob".to_string()), "{result:?}");
    let result = db.execute_params_for_session(
        alice,
        "INSERT INTO docs VALUES (4, 'alice', 'eng', NULL) ON CONFLICT (id) DO UPDATE SET owner = 'alice'",
        &[],
    );
    let owner = db.query("SELECT owner FROM docs WHERE id = 4", &[]).unwrap();
    assert_eq!(owner[0].values[0], Value::String("bob".to_string()), "{result:?}");
}

#[test]
fn prepared_statements_are_filtered_when_they_run() {
    let (db, alice, bob) = db();
    db.execute_for_session(alice, "PREPARE by_id AS SELECT id FROM docs WHERE id = $1")
        .unwrap();
    let (rows, _) = db.query_with_columns_for_session(alice, "EXECUTE by_id(1)").unwrap();
    assert_eq!(rows.len(), 1);
    let (rows, _) = db.query_with_columns_for_session(alice, "EXECUTE by_id(3)").unwrap();
    assert_eq!(rows.len(), 0);
    db.execute_for_session(bob, "PREPARE by_id AS SELECT id FROM docs WHERE id = $1")
        .unwrap();
    let (rows, _) = db.query_with_columns_for_session(bob, "EXECUTE by_id(3)").unwrap();
    assert_eq!(rows.len(), 1);
    let (rows, _) = db.query_with_columns_for_session(bob, "EXECUTE by_id(1)").unwrap();
    assert_eq!(rows.len(), 0);
}

#[test]
fn a_read_inside_a_transaction_is_filtered() {
    let (db, alice, _) = db();
    db.begin_transaction_for_session(alice).unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM docs ORDER BY id"), vec![1, 2]);
    assert_eq!(ids(&db, alice, "SELECT id FROM docs WHERE id = 3"), Vec::<i64>::new());
    db.execute_for_session(alice, "INSERT INTO docs VALUES (10, 'alice', 'eng', NULL)")
        .unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM docs ORDER BY id"), vec![1, 2, 10]);
    db.commit_transaction_for_session(alice).unwrap();
    assert_eq!(ids(&db, alice, "SELECT count(*) FROM docs"), vec![3]);
}

#[test]
fn foreign_key_checks_are_not_subject_to_policies() {
    let (db, alice, _) = db();
    db.execute("CREATE TABLE refs (id INT PRIMARY KEY, doc_id INT REFERENCES docs(id))")
        .unwrap();
    // Row 3 exists but alice cannot see it: the reference is still valid …
    db.execute_for_session(alice, "INSERT INTO refs VALUES (1, 3)").unwrap();
    db.execute_params_for_session(alice, "INSERT INTO refs VALUES (2, 4)", &[])
        .unwrap();
    // … and a row that does not exist is still refused.
    assert!(db.execute_for_session(alice, "INSERT INTO refs VALUES (3, 99)").is_err());
    // A referenced row cannot be deleted from under a reference, visible or not.
    db.execute_for_session(alice, "INSERT INTO refs VALUES (4, 1)").unwrap();
    assert!(db.execute_for_session(alice, "DELETE FROM docs WHERE id = 1").is_err());
}

#[test]
fn time_travel_reads_are_filtered() {
    let (db, alice, _) = db();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string();
    std::thread::sleep(std::time::Duration::from_millis(20));
    db.execute("INSERT INTO docs VALUES (20, 'alice', 'eng', NULL), (21, 'bob', 'eng', NULL)")
        .unwrap();
    let sql = format!("SELECT id FROM docs AS OF TIMESTAMP '{now}' ORDER BY id");
    assert_eq!(ids(&db, alice, &sql), vec![1, 2]);
    assert_eq!(ids(&db, alice, "SELECT id FROM docs ORDER BY id"), vec![1, 2, 20]);
}

#[test]
fn a_branch_read_is_filtered() {
    let (db, alice, _) = db();
    db.execute("CREATE BRANCH scratch AS OF NOW").unwrap();
    db.execute_for_session(alice, "USE BRANCH scratch").unwrap();
    assert_eq!(ids(&db, alice, "SELECT id FROM docs ORDER BY id"), vec![1, 2]);
    assert_eq!(ids(&db, alice, "SELECT id FROM docs WHERE id = 3"), Vec::<i64>::new());
    assert_eq!(ids(&db, alice, "SELECT count(*) FROM docs"), vec![2]);
    // A write on the branch is checked like any other.
    let e = db
        .execute_for_session(alice, "INSERT INTO docs VALUES (30, 'bob', 'eng', NULL)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("new row violates row-level security policy"), "{e}");
    assert_eq!(db.execute_for_session(alice, "UPDATE docs SET dept = 'b'").unwrap(), 2);
    db.execute_for_session(alice, "USE BRANCH main").unwrap();
}

#[test]
fn explain_does_not_show_unfiltered_rows() {
    let (db, alice, _) = db();
    let (rows, _) = db
        .query_with_columns_for_session(alice, "EXPLAIN ANALYZE SELECT id FROM docs")
        .unwrap();
    let text: String = rows.iter().map(|r| format!("{:?}\n", r.values)).collect();
    assert!(!text.contains("rows=5"), "{text}");
}
