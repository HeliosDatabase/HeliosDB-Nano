//! `AS OF` / `VERSIONS BETWEEN` are found by tokens, not by searching the SQL
//! text (plan brief v05.3). A column alias such as `AS offer_id`, the words
//! "as of" inside a string, and non-ASCII text used to break ordinary reads
//! and DML, or crash the process. Every statement here is run on the text
//! family (`query`/`execute`) and on the params family
//! (`query_params`/`execute_params`), and must give what the same statement
//! without the words gives.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::{EmbeddedDatabase, Value};

fn db() -> (EmbeddedDatabase, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    db.execute("CREATE TABLE acc (id INT PRIMARY KEY, bal INT)").unwrap();
    db.execute("INSERT INTO acc VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    db.execute("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    db.execute("INSERT INTO notes VALUES (1, 'a'), (2, 'x as of y'), (3, 'c')")
        .unwrap();
    (db, dir)
}

fn render(rows: &[heliosdb_nano::Tuple]) -> Vec<String> {
    rows.iter().map(|r| format!("{:?}", r.values)).collect()
}

/// Rows from both families, which must agree.
fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<String> {
    let a = render(&db.query(sql, &[]).unwrap_or_else(|e| panic!("query {sql}: {e}")));
    let b = render(
        &db.query_params(sql, &[])
            .unwrap_or_else(|e| panic!("query_params {sql}: {e}")),
    );
    assert_eq!(a, b, "{sql}: families disagree");
    a
}

#[test]
fn aliases_beginning_with_of_read_like_any_alias() {
    let (db, _d) = db();
    let plain = rows(&db, "SELECT id AS x FROM acc WHERE id = 1");
    assert_eq!(plain.len(), 1);
    for alias in ["offer_id", "offset_bal", "of_id", "OFFICIAL", "of", "\"as of\""] {
        let got = rows(&db, &format!("SELECT id AS {alias} FROM acc WHERE id = 1"));
        assert_eq!(got, plain, "alias {alias}");
    }
    // placeholder through the params family
    let r = db
        .query_params("SELECT id AS offer_id FROM acc WHERE id = $1", &[Value::Int4(1)])
        .unwrap();
    assert_eq!(render(&r), plain);
}

#[test]
fn as_of_text_in_literals_is_data() {
    let (db, _d) = db();
    assert_eq!(
        rows(&db, "SELECT id, 'as of' AS label FROM notes WHERE id = 1").len(),
        1
    );
    assert_eq!(
        rows(&db, "SELECT id, 'x' AS \"as of\" FROM notes WHERE id = 1").len(),
        1
    );
    assert_eq!(
        rows(&db, "SELECT id FROM notes WHERE body = 'x as of y' OR id = 3").len(),
        2
    );
    // DML
    for run in 0..2 {
        let sql = "INSERT INTO notes SELECT id + 10, 'copied as of now' FROM notes WHERE id = 1";
        let sql_params = "INSERT INTO notes SELECT id + 20, 'copied as of now' FROM notes WHERE id = 1";
        if run == 0 {
            assert_eq!(db.execute(sql).unwrap(), 1);
        } else {
            assert_eq!(db.execute_params(sql_params, &[]).unwrap(), 1);
        }
    }
    assert_eq!(
        rows(&db, "SELECT id FROM notes WHERE body = 'copied as of now'").len(),
        2
    );
    assert_eq!(
        db.execute("DELETE FROM notes WHERE body = 'copied as of now'").unwrap(),
        2
    );
    assert_eq!(
        db.execute_params("DELETE FROM notes WHERE body = 'x as of y'", &[])
            .unwrap(),
        1
    );
}

#[test]
fn non_ascii_text_before_a_clause_does_not_panic() {
    let (db, _d) = db();
    for ch in ["ŉ", "ǰ", "ß", "İ"] {
        let sql = format!("SELECT '{}' AS x, COUNT(*) FROM notes AS OF NOW", ch.repeat(30));
        let got = rows(&db, &sql);
        assert_eq!(got.len(), 1, "{ch}");
        let plain = rows(&db, &format!("SELECT '{}' AS x, COUNT(*) FROM notes", ch.repeat(30)));
        assert_eq!(got, plain, "{ch}");
    }
}

#[test]
fn real_clauses_still_work_after_alias_in_subselect_and_cte() {
    let (db, _d) = db();
    let plain = rows(&db, "SELECT id FROM acc ORDER BY id");
    assert_eq!(rows(&db, "SELECT id FROM acc AS OF NOW ORDER BY id"), plain);
    assert_eq!(rows(&db, "SELECT a.id FROM acc AS OF NOW a ORDER BY a.id"), plain);
    assert_eq!(rows(&db, "SELECT a.id FROM acc a AS OF NOW ORDER BY a.id"), plain);
    assert_eq!(
        rows(&db, "SELECT s.id FROM (SELECT id FROM acc AS OF NOW) s ORDER BY s.id"),
        plain
    );
    assert_eq!(
        rows(
            &db,
            "WITH c AS (SELECT id FROM acc AS OF NOW) SELECT id FROM c ORDER BY id"
        ),
        plain
    );
}

#[test]
fn join_clause_binds_to_the_table_it_follows() {
    let (db, _d) = db();
    // acc has 3 rows now; give it a historical state through a snapshot first.
    let ts_before = {
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        t
    };
    db.execute("INSERT INTO acc VALUES (4, 40)").unwrap();
    db.execute("INSERT INTO notes VALUES (4, 'd')").unwrap();
    // the clause sits on the RIGHT table only: acc is read as of now (4 rows),
    // notes as of the earlier time (3 rows).
    let sql =
        format!("SELECT a.id, n.id FROM acc a JOIN notes n AS OF TIMESTAMP '{ts_before}' ON a.id = n.id ORDER BY a.id");
    assert_eq!(rows(&db, &sql).len(), 3);
    // and on the left only: acc historical (3), notes current (4) -> 3 matches
    let sql =
        format!("SELECT a.id, n.id FROM acc AS OF TIMESTAMP '{ts_before}' a JOIN notes n ON a.id = n.id ORDER BY a.id");
    assert_eq!(rows(&db, &sql).len(), 3);
    // on neither: 4 matches
    assert_eq!(
        rows(&db, "SELECT a.id, n.id FROM acc a JOIN notes n ON a.id = n.id").len(),
        4
    );
    // clause on acc must not leak onto notes: acc historical, joined to notes LEFT
    let sql = format!(
        "SELECT n.id FROM notes n LEFT JOIN acc AS OF TIMESTAMP '{ts_before}' a ON a.id = n.id WHERE a.id IS NULL"
    );
    assert_eq!(rows(&db, &sql).len(), 1);
}

#[test]
fn line_commented_clause_is_ignored() {
    let (db, _d) = db();
    let sql = "SELECT id FROM acc -- AS OF TIMESTAMP '2000-01-01 00:00:00'\n ORDER BY id";
    assert_eq!(rows(&db, sql).len(), 3);
    let sql = "SELECT id FROM acc /* AS OF TIMESTAMP '2000-01-01 00:00:00' */ ORDER BY id";
    assert_eq!(rows(&db, sql).len(), 3);
}

#[test]
fn missing_timestamp_literal_is_an_error_not_a_silent_read() {
    let (db, _d) = db();
    assert!(db.query("SELECT id FROM acc AS OF TIMESTAMP", &[]).is_err());
    assert!(db.query_params("SELECT id FROM acc AS OF TIMESTAMP", &[]).is_err());
}
