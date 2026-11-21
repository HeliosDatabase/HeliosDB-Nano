#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::many_single_char_names)]
//! `MERGE INTO`, the SQL-standard statement: every WHEN arm, conditions, the
//! second-change error, atomicity, RETURNING, constraints and foreign keys,
//! defaults and serial columns, every source kind, bound parameters, a
//! transaction block and a branch — on both statement families.

use heliosdb_nano::{EmbeddedDatabase, Error, StatementOutcome, Tuple, Value};

fn db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

fn rows(db: &EmbeddedDatabase, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .into_iter()
        .map(|t: Tuple| t.values)
        .collect()
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "NULL".to_string(),
        other => other.to_string(),
    }
}

/// `id:val` of every row of `table`, ordered by id.
fn snapshot(db: &EmbeddedDatabase, table: &str) -> Vec<String> {
    rows(db, &format!("SELECT id, val FROM {table} ORDER BY id"))
        .iter()
        .map(|r| format!("{}:{}", int(&r[0]), text(&r[1])))
        .collect()
}

fn sqlstate(e: &Error) -> String {
    e.raised_sqlstate().unwrap_or("-").to_string()
}

/// A target `t(id PK, val)` with rows 1:a 2:b 3:c and a source `s(id, val)`
/// with rows 2:B 3:C 4:D.
fn target_and_source() -> EmbeddedDatabase {
    let db = db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (id INT, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
    db.execute("INSERT INTO s VALUES (2, 'B'), (3, 'C'), (4, 'D')").unwrap();
    db
}

#[test]
fn matched_rows_are_updated_and_unmatched_rows_inserted() {
    let db = target_and_source();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)",
        )
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:B", "3:C", "4:D"]);
}

#[test]
fn the_params_family_gives_the_same_result() {
    let db = target_and_source();
    let n = db
        .execute_params(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)",
            &[],
        )
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:B", "3:C", "4:D"]);
}

#[test]
fn aliases_name_the_two_sides() {
    let db = target_and_source();
    db.execute(
        "MERGE INTO t AS tgt USING s AS src ON tgt.id = src.id \
         WHEN MATCHED THEN UPDATE SET val = tgt.val || src.val \
         WHEN NOT MATCHED THEN INSERT VALUES (src.id, src.val)",
    )
    .unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:bB", "3:cC", "4:D"]);
}

#[test]
fn matched_delete_and_do_nothing() {
    let db = target_and_source();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED AND s.val = 'B' THEN DELETE \
             WHEN MATCHED THEN DO NOTHING \
             WHEN NOT MATCHED THEN DO NOTHING",
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(snapshot(&db, "t"), ["1:a", "3:c"]);
}

#[test]
fn the_first_clause_whose_condition_holds_acts() {
    let db = target_and_source();
    db.execute(
        "MERGE INTO t USING s ON t.id = s.id \
         WHEN MATCHED AND s.id = 2 THEN UPDATE SET val = 'first' \
         WHEN MATCHED AND s.id IN (2, 3) THEN UPDATE SET val = 'second' \
         WHEN MATCHED THEN UPDATE SET val = 'third' \
         WHEN NOT MATCHED AND s.id > 100 THEN INSERT VALUES (s.id, 'big') \
         WHEN NOT MATCHED THEN INSERT VALUES (s.id, 'small')",
    )
    .unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:first", "3:second", "4:small"]);
}

#[test]
fn a_condition_that_holds_for_no_clause_leaves_the_row() {
    let db = target_and_source();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED AND s.val = 'nope' THEN DELETE \
             WHEN NOT MATCHED AND s.val = 'nope' THEN INSERT VALUES (s.id, s.val)",
        )
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:c"]);
}

#[test]
fn not_matched_by_source_acts_on_target_rows_without_a_source_row() {
    let db = target_and_source();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED BY TARGET THEN INSERT VALUES (s.id, s.val) \
             WHEN NOT MATCHED BY SOURCE AND t.id < 0 THEN DO NOTHING \
             WHEN NOT MATCHED BY SOURCE THEN DELETE",
        )
        .unwrap();
    assert_eq!(n, 4);
    assert_eq!(snapshot(&db, "t"), ["2:B", "3:C", "4:D"]);

    let n = db
        .execute(
            "MERGE INTO t USING (SELECT 2 AS id) AS only ON t.id = only.id \
             WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = 'orphan'",
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(snapshot(&db, "t"), ["2:B", "3:orphan", "4:orphan"]);
}

#[test]
fn a_second_change_of_one_target_row_is_error_21000_and_changes_nothing() {
    let db = target_and_source();
    db.execute("INSERT INTO s VALUES (2, 'again')").unwrap();
    for family in 0..2 {
        let sql = "MERGE INTO t USING s ON t.id = s.id \
                   WHEN MATCHED THEN UPDATE SET val = s.val \
                   WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)";
        let err = if family == 0 {
            db.execute(sql).unwrap_err()
        } else {
            db.execute_params(sql, &[]).unwrap_err()
        };
        assert_eq!(sqlstate(&err), "21000", "{err}");
        assert!(err.to_string().contains("cannot affect row a second time"), "{err}");
        assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:c"], "family {family}");
    }
}

#[test]
fn a_row_matched_twice_but_changed_once_is_not_an_error() {
    let db = target_and_source();
    db.execute("INSERT INTO s VALUES (2, 'skip')").unwrap();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED AND s.val = 'skip' THEN DO NOTHING \
             WHEN MATCHED THEN UPDATE SET val = s.val",
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:B", "3:C"]);
}

#[test]
fn a_late_failure_undoes_the_whole_statement() {
    let db = db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT, CHECK (val <> 'bad'))")
        .unwrap();
    db.execute("CREATE TABLE s (id INT, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'A'), (5, 'E'), (2, 'bad')").unwrap();
    let sql = "MERGE INTO t USING s ON t.id = s.id \
               WHEN MATCHED THEN UPDATE SET val = s.val \
               WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)";
    assert!(db.execute(sql).is_err());
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b"]);
    assert!(db.execute_params(sql, &[]).is_err());
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b"]);
}

#[test]
fn a_failed_merge_in_a_block_leaves_the_block_s_earlier_work_alone() {
    let db = db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (id INT, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO s VALUES (7, 'x'), (1, 'A'), (1, 'AA')").unwrap();
    let txn = db.begin_transaction().unwrap();
    txn.execute("INSERT INTO t VALUES (9, 'mine')").unwrap();
    let err = txn
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)",
        )
        .unwrap_err();
    assert!(err.to_string().contains("second time"), "{err}");
    let seen: Vec<i64> = txn
        .query("SELECT id FROM t ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|r| int(&r.values[0]))
        .collect();
    assert_eq!(seen, [1, 9], "the MERGE's insert of 7 and its update are undone");
    txn.rollback().unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a"]);
}

#[test]
fn rollback_and_commit_of_a_block() {
    let db = target_and_source();
    let sql = "MERGE INTO t USING s ON t.id = s.id \
               WHEN MATCHED THEN UPDATE SET val = s.val \
               WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)";
    db.execute("BEGIN").unwrap();
    assert_eq!(db.execute(sql).unwrap(), 3);
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:B", "3:C", "4:D"]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:c"]);

    db.execute("BEGIN").unwrap();
    assert_eq!(db.execute_params(sql, &[]).unwrap(), 3);
    db.execute("COMMIT").unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:B", "3:C", "4:D"]);
}

#[test]
fn a_merge_sees_the_rows_its_own_transaction_wrote() {
    let db = target_and_source();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (4, 'staged')").unwrap();
    db.execute("DELETE FROM t WHERE id = 2").unwrap();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = t.val || '+' || s.val \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, 'new')",
        )
        .unwrap();
    assert_eq!(n, 3);
    db.execute("COMMIT").unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:new", "3:c+C", "4:staged+D"]);
}

#[test]
fn a_row_the_statement_inserts_is_not_matched_by_a_later_source_row() {
    let db = db();
    db.execute("CREATE TABLE t (id INT, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (id INT, val TEXT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'x'), (1, 'y')").unwrap();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = 'matched' \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)",
        )
        .unwrap();
    assert_eq!(n, 2);
    let mut vals: Vec<String> = rows(&db, "SELECT val FROM t").iter().map(|r| text(&r[0])).collect();
    vals.sort();
    assert_eq!(vals, ["x", "y"]);
}

#[test]
fn returning_gives_the_action_and_both_sides() {
    let db = target_and_source();
    let sql = "MERGE INTO t USING s ON t.id = s.id \
               WHEN MATCHED AND s.id = 2 THEN DELETE \
               WHEN MATCHED THEN UPDATE SET val = s.val \
               WHEN NOT MATCHED THEN INSERT VALUES (s.id, lower(s.val)) \
               RETURNING merge_action() AS act, t.id, t.val AS now, s.val AS src, t.id * 10 AS tenfold";
    let (n, out) = db.execute_returning(sql).unwrap();
    assert_eq!(n, 3);
    let mut got: Vec<String> = out
        .iter()
        .map(|r| {
            format!(
                "{}|{}|{}|{}|{}",
                text(&r.values[0]),
                int(&r.values[1]),
                text(&r.values[2]),
                text(&r.values[3]),
                int(&r.values[4])
            )
        })
        .collect();
    got.sort();
    assert_eq!(got, ["DELETE|2|b|B|20", "INSERT|4|d|D|40", "UPDATE|3|C|C|30"]);
    assert_eq!(snapshot(&db, "t"), ["1:a", "3:C", "4:d"]);
}

#[test]
fn returning_star_is_the_source_columns_then_the_target_columns() {
    let db = db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (sid INT, sval TEXT)").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'one')").unwrap();
    let sql = "MERGE INTO t USING s ON t.id = s.sid \
               WHEN NOT MATCHED THEN INSERT VALUES (s.sid, upper(s.sval)) RETURNING *";
    let outcome = db.run_statement(sql).unwrap();
    match outcome {
        StatementOutcome::Rows { columns, rows } => {
            assert_eq!(columns, ["sid", "sval", "id", "val"]);
            assert_eq!(rows.len(), 1);
            assert_eq!(int(&rows[0].values[0]), 1);
            assert_eq!(text(&rows[0].values[1]), "one");
            assert_eq!(int(&rows[0].values[2]), 1);
            assert_eq!(text(&rows[0].values[3]), "ONE");
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // The query entry point returns the same rows.
    db.execute("DELETE FROM t").unwrap();
    let out = db
        .query(
            "MERGE INTO t USING s ON t.id = s.sid \
             WHEN NOT MATCHED THEN INSERT VALUES (s.sid, s.sval) RETURNING t.*, merge_action()",
            &[],
        )
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(text(&out[0].values[1]), "one");
    assert_eq!(text(&out[0].values[2]), "INSERT");
}

#[test]
fn a_statement_without_returning_reports_a_count() {
    let db = target_and_source();
    let outcome = db
        .run_statement("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE")
        .unwrap();
    assert_eq!(outcome, StatementOutcome::Affected(2));
}

#[test]
fn constraints_hold_as_for_insert_and_update() {
    let db = db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, code TEXT UNIQUE, n INT NOT NULL, CHECK (n >= 0))")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a', 1), (2, 'b', 2)").unwrap();
    // UNIQUE through UPDATE.
    let err = db
        .execute(
            "MERGE INTO t USING (SELECT 2 AS id) AS s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET code = 'a'",
        )
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("unique"), "{err}");
    // PRIMARY KEY through INSERT (the ON condition deliberately misses).
    let err = db
        .execute(
            "MERGE INTO t USING (SELECT 1 AS id) AS s ON t.id = s.id + 100 \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, 'z', 0)",
        )
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("duplicate") || err.to_string().to_lowercase().contains("unique"),
        "{err}"
    );
    // NOT NULL and CHECK.
    assert!(db
        .execute(
            "MERGE INTO t USING (SELECT 1 AS id) AS s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = NULL"
        )
        .is_err());
    assert!(db
        .execute("MERGE INTO t USING (SELECT 1 AS id) AS s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = -1")
        .is_err());
    let got: Vec<String> = rows(&db, "SELECT id, code, n FROM t ORDER BY id")
        .iter()
        .map(|r| format!("{}:{}:{}", int(&r[0]), text(&r[1]), int(&r[2])))
        .collect();
    assert_eq!(got, ["1:a:1", "2:b:2"]);
}

#[test]
fn foreign_keys_hold_in_both_directions() {
    let db = db();
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id), val TEXT)")
        .unwrap();
    db.execute("INSERT INTO parent VALUES (1, 'p1'), (2, 'p2')").unwrap();
    db.execute("INSERT INTO child VALUES (10, 1, 'c')").unwrap();
    // A child row for a parent that does not exist.
    let err = db
        .execute(
            "MERGE INTO child USING (SELECT 11 AS id, 99 AS pid) AS s ON child.id = s.id \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.pid, 'orphan')",
        )
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("foreign key"), "{err}");
    // Deleting a parent that is referenced.
    let err = db
        .execute("MERGE INTO parent USING (SELECT 1 AS id) AS s ON parent.id = s.id WHEN MATCHED THEN DELETE")
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("foreign key"), "{err}");
    // The unreferenced parent can go, and a valid child can come.
    assert_eq!(
        db.execute("MERGE INTO parent USING (SELECT 2 AS id) AS s ON parent.id = s.id WHEN MATCHED THEN DELETE")
            .unwrap(),
        1
    );
    assert_eq!(
        db.execute(
            "MERGE INTO child USING (SELECT 12 AS id, 1 AS pid) AS s ON child.id = s.id \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.pid, 'ok')"
        )
        .unwrap(),
        1
    );
    assert_eq!(snapshot(&db, "parent"), ["1:p1"]);
    assert_eq!(snapshot(&db, "child"), ["10:c", "12:ok"]);
}

#[test]
fn defaults_and_serial_columns_are_filled() {
    let db = db();
    db.execute("CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT, state TEXT DEFAULT 'new', n INT DEFAULT 7)")
        .unwrap();
    db.execute("CREATE TABLE s (name TEXT)").unwrap();
    db.execute("INSERT INTO s VALUES ('x'), ('y')").unwrap();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.name = s.name \
             WHEN NOT MATCHED THEN INSERT (name, n) VALUES (s.name, DEFAULT)",
        )
        .unwrap();
    assert_eq!(n, 2);
    let got = rows(&db, "SELECT id, name, state, n FROM t ORDER BY name");
    assert_eq!(got.len(), 2);
    assert_eq!(text(&got[0][1]), "x");
    assert_eq!(text(&got[0][2]), "new");
    assert_eq!(int(&got[0][3]), 7);
    assert_ne!(int(&got[0][0]), int(&got[1][0]), "serial ids differ");
    assert!(int(&got[0][0]) > 0 && int(&got[1][0]) > 0);

    // SET … = DEFAULT restores the declared default.
    db.execute("UPDATE t SET state = 'used', n = 1").unwrap();
    db.execute(
        "MERGE INTO t USING s ON t.name = s.name \
         WHEN MATCHED AND s.name = 'x' THEN UPDATE SET state = DEFAULT, n = DEFAULT",
    )
    .unwrap();
    let got = rows(&db, "SELECT name, state, n FROM t ORDER BY name");
    assert_eq!((text(&got[0][1]), int(&got[0][2])), ("new".to_string(), 7));
    assert_eq!((text(&got[1][1]), int(&got[1][2])), ("used".to_string(), 1));

    // INSERT DEFAULT VALUES.
    let n = db
        .execute(
            "MERGE INTO t USING (SELECT 'zz' AS name) AS q ON t.name = q.name \
             WHEN NOT MATCHED THEN INSERT DEFAULT VALUES",
        )
        .unwrap();
    assert_eq!(n, 1);
    let got = rows(&db, "SELECT name, state, n FROM t WHERE name IS NULL");
    assert_eq!(got.len(), 1);
    assert_eq!((text(&got[0][1]), int(&got[0][2])), ("new".to_string(), 7));
}

#[test]
fn the_source_may_be_a_subquery_values_or_a_cte() {
    let db = target_and_source();
    db.execute(
        "MERGE INTO t USING (SELECT id, upper(val) || '!' AS val FROM s WHERE id <> 3) AS q ON t.id = q.id \
         WHEN MATCHED THEN UPDATE SET val = q.val \
         WHEN NOT MATCHED THEN INSERT VALUES (q.id, q.val)",
    )
    .unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:B!", "3:c", "4:D!"]);

    db.execute(
        "MERGE INTO t USING (VALUES (1, 'one'), (5, 'five')) AS v(id, val) ON t.id = v.id \
         WHEN MATCHED THEN UPDATE SET val = v.val \
         WHEN NOT MATCHED THEN INSERT VALUES (v.id, v.val)",
    )
    .unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:one", "2:B!", "3:c", "4:D!", "5:five"]);

    db.execute(
        "WITH picked AS (SELECT id, val FROM s WHERE id = 3) \
         MERGE INTO t USING picked ON t.id = picked.id \
         WHEN MATCHED THEN UPDATE SET val = 'cte:' || picked.val",
    )
    .unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:one", "2:B!", "3:cte:C", "4:D!", "5:five"]);
}

#[test]
fn bound_parameters_are_values_everywhere_they_appear() {
    let db = target_and_source();
    let sql = "MERGE INTO t USING (SELECT id, val FROM s WHERE id >= $1) AS q ON t.id = q.id \
               WHEN MATCHED AND q.val <> $2 THEN UPDATE SET val = q.val || $3 \
               WHEN NOT MATCHED THEN INSERT VALUES (q.id, $4) \
               RETURNING merge_action(), t.val, $3 AS suffix";
    let params = [
        Value::Int4(3),
        Value::String("nope".to_string()),
        Value::String("'; DROP TABLE t; --".to_string()),
        Value::String("ins".to_string()),
    ];
    let (n, out) = db.execute_params_returning(sql, &params).unwrap();
    assert_eq!(n, 2);
    assert_eq!(out.len(), 2);
    assert!(out
        .iter()
        .all(|r| text(&r.values[2]) == "'; DROP TABLE t; --"));
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:C'; DROP TABLE t; --", "4:ins"]);

    let n = db
        .execute_params(
            "MERGE INTO t USING s ON t.id = s.id AND t.id = $1 WHEN MATCHED THEN DELETE",
            &[Value::Int4(2)],
        )
        .unwrap();
    assert_eq!(n, 1);
    let out = db
        .query_params(
            "MERGE INTO t USING s ON t.id = s.id AND t.id = $1 WHEN MATCHED THEN DELETE RETURNING t.id",
            &[Value::Int4(3)],
        )
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(int(&out[0].values[0]), 3);
    assert_eq!(snapshot(&db, "t"), ["1:a", "4:ins"]);
}

#[test]
fn a_target_without_a_primary_key_and_with_identical_rows() {
    let db = db();
    db.execute("CREATE TABLE t (id INT, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (id INT, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'same'), (1, 'same'), (2, 'other'), (NULL, 'null')")
        .unwrap();
    db.execute("INSERT INTO s VALUES (1, 'S1'), (3, 'S3'), (NULL, 'SN')").unwrap();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)",
        )
        .unwrap();
    // Both identical rows are matched by the one source row (two updates);
    // 3 and the NULL-keyed source row (NULL equals nothing) are inserted.
    assert_eq!(n, 4);
    let mut got: Vec<String> = rows(&db, "SELECT id, val FROM t")
        .iter()
        .map(|r| format!("{}:{}", text(&r[0]), text(&r[1])))
        .collect();
    got.sort();
    assert_eq!(got, ["1:S1", "1:S1", "2:other", "3:S3", "NULL:SN", "NULL:null"]);

    // Deleting one of two identical rows' twin by a non-key condition.
    let n = db
        .execute("MERGE INTO t USING (SELECT 'S1' AS val) AS q ON t.val = q.val WHEN MATCHED THEN DELETE")
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(rows(&db, "SELECT count(*) FROM t WHERE val = 'S1'")[0][0], Value::Int8(0));
}

#[test]
fn a_non_equality_condition_joins_row_by_row() {
    let db = target_and_source();
    let n = db
        .execute(
            "MERGE INTO t USING (SELECT 2 AS lo) AS q ON t.id > q.lo \
             WHEN MATCHED THEN UPDATE SET val = 'gt'",
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:gt"]);
}

#[test]
fn keys_of_different_integer_widths_and_text_keys_match() {
    let db = db();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, code TEXT, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (id SMALLINT, code TEXT, val TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'k1', 'a'), (2, 'k2', 'b')").unwrap();
    db.execute("INSERT INTO s VALUES (1, 'k1', 'A'), (2, 'kX', 'B')").unwrap();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id AND s.code = t.code \
             WHEN MATCHED THEN UPDATE SET val = s.val",
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(snapshot(&db, "t"), ["1:A", "2:b"]);
}

#[test]
fn a_view_is_refused_as_target() {
    let db = target_and_source();
    db.execute("CREATE VIEW tv AS SELECT id, val FROM t").unwrap();
    let err = db
        .execute("MERGE INTO tv USING s ON tv.id = s.id WHEN MATCHED THEN DELETE")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42809", "{err}");
    assert!(err.to_string().contains("cannot execute MERGE on relation"), "{err}");
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:c"]);
    let err = db
        .execute("MERGE INTO no_such_table USING s ON no_such_table.id = s.id WHEN MATCHED THEN DELETE")
        .unwrap_err();
    assert!(err.to_string().contains("no_such_table"), "{err}");
}

#[test]
fn names_that_cannot_be_resolved_are_errors_not_guesses() {
    let db = target_and_source();
    // `id` and `val` exist on both sides.
    let err = db
        .execute("MERGE INTO t USING s ON id = 1 WHEN MATCHED THEN DELETE")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42702", "{err}");
    // A NOT MATCHED clause has no target row.
    let err = db
        .execute("MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id, t.val)")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42P01", "{err}");
    // A NOT MATCHED BY SOURCE clause has no source row.
    let err = db
        .execute("MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = s.val")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42P01", "{err}");
    let err = db
        .execute("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET nope = 1")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42703", "{err}");
    let err = db
        .execute("MERGE INTO t USING s ON t.id = s.nope WHEN MATCHED THEN DELETE")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42703", "{err}");
    // A subquery in a condition is refused, not answered wrongly.
    let err = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED AND t.id = (SELECT max(id) FROM s) THEN DELETE",
        )
        .unwrap_err();
    assert_eq!(sqlstate(&err), "0A000", "{err}");
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:c"]);
}

#[test]
fn an_index_follows_the_changes() {
    let db = target_and_source();
    db.execute("CREATE INDEX t_val ON t (val)").unwrap();
    db.execute(
        "MERGE INTO t USING s ON t.id = s.id \
         WHEN MATCHED AND s.id = 2 THEN DELETE \
         WHEN MATCHED THEN UPDATE SET val = s.val \
         WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)",
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT id FROM t WHERE val = 'C'").len(), 1);
    assert_eq!(rows(&db, "SELECT id FROM t WHERE val = 'c'").len(), 0);
    assert_eq!(rows(&db, "SELECT id FROM t WHERE val = 'b'").len(), 0);
    assert_eq!(rows(&db, "SELECT id FROM t WHERE val = 'D'").len(), 1);
}

#[test]
fn a_merge_on_a_branch_stays_on_the_branch() {
    let db = target_and_source();
    db.execute("CREATE BRANCH dev FROM main AS OF NOW").unwrap();
    db.execute("USE BRANCH dev").unwrap();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val) \
             WHEN NOT MATCHED BY SOURCE THEN DELETE",
        )
        .unwrap();
    assert_eq!(n, 4);
    assert_eq!(snapshot(&db, "t"), ["2:B", "3:C", "4:D"]);
    db.execute("USE BRANCH main").unwrap();
    assert_eq!(snapshot(&db, "t"), ["1:a", "2:b", "3:c"]);
}

#[test]
fn merge_branch_is_still_its_own_statement() {
    let db = target_and_source();
    let err = db.execute("MERGE BRANCH no_such_branch INTO main").unwrap_err();
    let text = err.to_string().to_lowercase();
    assert!(text.contains("branch"), "{err}");
    assert!(!text.contains("when clause"), "{err}");
}

#[test]
fn syntax_errors_are_reported() {
    let db = target_and_source();
    let err = db.execute("MERGE INTO t USING s ON t.id = s.id").unwrap_err();
    assert_eq!(sqlstate(&err), "42601", "{err}");
    let err = db
        .execute("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN INSERT VALUES (1, 'x')")
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42601", "{err}");
}

#[test]
fn a_keyword_inside_a_literal_is_data() {
    let db = target_and_source();
    db.execute(
        "MERGE INTO t USING s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET val = 'WHEN MATCHED THEN DO NOTHING RETURNING x' \
         WHEN NOT MATCHED THEN INSERT VALUES (s.id, CASE WHEN s.id > 3 THEN 'THEN' ELSE 'ELSE' END)",
    )
    .unwrap();
    assert_eq!(
        snapshot(&db, "t"),
        [
            "1:a",
            "2:WHEN MATCHED THEN DO NOTHING RETURNING x",
            "3:WHEN MATCHED THEN DO NOTHING RETURNING x",
            "4:THEN"
        ]
    );
}

#[test]
fn many_rows_merge_in_reasonable_time() {
    let db = db();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("CREATE TABLE s (id INT, val TEXT)").unwrap();
    for chunk in 0..10 {
        let values: Vec<String> = (0..200)
            .map(|i| {
                let id = chunk * 200 + i;
                format!("({id}, 'v{id}')")
            })
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(","))).unwrap();
        let values: Vec<String> = (0..200)
            .map(|i| {
                let id = chunk * 200 + i + 1000;
                format!("({id}, 'n{id}')")
            })
            .collect();
        db.execute(&format!("INSERT INTO s VALUES {}", values.join(","))).unwrap();
    }
    let started = std::time::Instant::now();
    let n = db
        .execute(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.val)",
        )
        .unwrap();
    assert_eq!(n, 2000);
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::Int8(3000));
    assert_eq!(rows(&db, "SELECT count(*) FROM t WHERE val LIKE 'n%'")[0][0], Value::Int8(2000));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "2000 source rows against 2000 target rows took {:?}",
        started.elapsed()
    );
}
