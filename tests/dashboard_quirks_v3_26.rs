//! Regression tests for the Token-Dashboard cutover quirks reported
//! against v3.26.0 in `/home/app/websites/token-dashboard/HELIOSDB_*.md`.
//!
//! Quirk A — autocommit visibility (the cutover blocker): INSERT followed
//! immediately by SELECT in the same connection sees zero rows until an
//! explicit COMMIT fires. With autocommit=False, BEGIN errors with
//! "Transaction already active".
//!
//! Quirk B — CTE with parameter binding silently returns zero rows.
//! Same logical query in flat WHERE form returns rows; rewrap in a
//! `WITH foo AS (…) SELECT … FROM foo WHERE x >= $1` and you get 0.
//!
//! Quirk D — SQL-standard `SUBSTRING (s FROM x FOR y)` errors with
//! "Expression not yet supported: Substring { special: true }".
//! The function form `SUBSTR(s, x, y)` works.

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------- Quirk A: autocommit visibility ----------------------------------

#[test]
fn embedded_insert_then_select_sees_the_row() {
    // Baseline: at the embedded-API level, INSERT-then-SELECT in
    // implicit-tx mode must surface the inserted row. If THIS fails,
    // Quirk A's root cause is in the storage / fast-path layer, not in
    // the PG-wire handler.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id INT, name TEXT)").expect("create");
    db.execute("INSERT INTO t VALUES (1, 'a')").expect("insert");
    let rows = db.query("SELECT COUNT(*) FROM t", &[]).expect("select count");
    assert_eq!(rows.len(), 1);
    let count = match rows[0].values.first() {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("expected integer count, got {other:?}"),
    };
    assert_eq!(count, 1, "INSERT followed by SELECT must see the inserted row");
}

#[test]
fn embedded_insert_then_select_data_round_trips() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id INT, name TEXT)").expect("create");
    db.execute("INSERT INTO t VALUES (42, 'hello')").expect("insert");
    let rows = db.query("SELECT id, name FROM t WHERE id = 42", &[]).expect("select");
    assert_eq!(rows.len(), 1, "row inserted in same session must be visible");
}

#[test]
fn execute_params_insert_then_query_sees_the_row() {
    // The PG-wire extended-query path now routes INSERT through
    // `db.execute_params`. That path didn't wrap the write in an
    // implicit transaction in v3.26.0/.1, which the dashboard team's
    // Quirk A diagnosis points at: pg8000's autocommit=True path
    // bypasses BEGIN/COMMIT, so the executor sees no transaction
    // wrapper and the write either doesn't commit or doesn't show up
    // on the next read.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id INT, name TEXT)").expect("create");
    let inserted = db
        .execute_params(
            "INSERT INTO t (id, name) VALUES ($1, $2)",
            &[Value::Int4(1), Value::String("a".to_string())],
        )
        .expect("insert via params");
    assert_eq!(inserted, 1, "INSERT should report 1 row");

    // The user reports SELECT after INSERT sees 0 rows on the wire.
    // Mirror that here at the embedded API.
    let rows = db
        .query_with_columns("SELECT COUNT(*) FROM t")
        .expect("count after insert");
    let count = match rows.0[0].values.first() {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        other => panic!("expected integer, got {other:?}"),
    };
    assert_eq!(
        count, 1,
        "Quirk A: INSERT via execute_params then SELECT must see 1 row"
    );
}

// ---------- Quirk C: SHOW BRANCHES empty after CREATE BRANCH ----------------

#[test]
fn create_branch_then_show_branches_lists_it() {
    // Dashboard repro: CREATE BRANCH 'verify-branch' AS OF NOW returns OK
    // 0, but SHOW BRANCHES afterwards is empty. v3.26.1 fixed
    // SHOW BRANCHES routing in the embedded query path; this checks
    // whether CREATE BRANCH actually persists branches that the
    // pg_database_branches() / SHOW BRANCHES surfaces can see.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE DATABASE BRANCH verify_branch FROM main AS OF NOW")
        .expect("create branch");

    let (rows, _cols) = db
        .query_with_columns("SHOW BRANCHES")
        .expect("show branches");
    assert!(rows.len() >= 2, "SHOW BRANCHES must list main + verify_branch, got {}", rows.len());

    let any_named = rows.iter().any(|r| {
        r.values
            .iter()
            .any(|v| matches!(v, Value::String(s) if s == "verify_branch"))
    });
    assert!(any_named, "Quirk C: verify_branch must be enumerable; rows={rows:?}");

    // pg_database_branches() must agree.
    let (rows2, _cols2) = db
        .query_with_columns("SELECT * FROM pg_database_branches()")
        .expect("pg_database_branches");
    let any_named2 = rows2.iter().any(|r| {
        r.values
            .iter()
            .any(|v| matches!(v, Value::String(s) if s == "verify_branch"))
    });
    assert!(any_named2, "pg_database_branches() must list verify_branch; rows={rows2:?}");
}

#[test]
fn create_branch_with_quoted_name_strips_quotes() {
    // The dashboard team's actual call:
    //   CREATE BRANCH 'verify-branch' AS OF NOW
    // The SQL-standard for branch names should accept either bare
    // identifiers or single-quoted strings. v3.26.0 stored the quotes
    // as part of the name (so the branch was unfindable under the
    // user's intended bare name).
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE BRANCH 'verify-branch' AS OF NOW").expect("create");

    let (rows, _) = db
        .query_with_columns("SELECT * FROM pg_database_branches()")
        .expect("pg_database_branches");
    let names: Vec<&str> = rows.iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        names.iter().any(|n| *n == "verify-branch"),
        "branch should be stored as `verify-branch` (no quotes); got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains('\'')),
        "no branch name should retain its surrounding single quotes; got {names:?}"
    );
}

// ---------- Quirk B: CTE with parameter binding -----------------------------

#[test]
fn cte_with_parameter_in_outer_where_returns_rows() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE messages (id INT, billable INT)").expect("create");
    for (i, b) in [(1, 100), (2, 50), (3, 200), (4, 150)] {
        db.execute(&format!("INSERT INTO messages VALUES ({i}, {b})")).expect("insert");
    }

    // Flat WHERE form: works today.
    let flat = db
        .query_with_columns("SELECT id, billable FROM messages WHERE billable >= 100")
        .expect("flat query");
    assert_eq!(flat.0.len(), 3, "flat WHERE must return 3 rows >= 100");

    // CTE form, literal threshold (no parameter): should also work.
    let cte_lit = db
        .query_with_columns(
            "WITH spend AS (SELECT id, billable FROM messages) \
             SELECT id, billable FROM spend WHERE billable >= 100",
        )
        .expect("cte literal query");
    assert_eq!(cte_lit.0.len(), 3, "CTE with literal threshold must return 3 rows");

    // CTE form WITH parameter binding (the dashboard's Quirk B repro):
    // WITH spend AS (...) SELECT ... FROM spend WHERE billable >= $1
    let cte_param = db.query_params(
        "WITH spend AS (SELECT id, billable FROM messages) \
         SELECT id, billable FROM spend WHERE billable >= $1",
        &[Value::Int8(100)],
    );
    let rows = cte_param.expect("cte parameterised query");
    assert_eq!(
        rows.len(),
        3,
        "Quirk B: CTE with $1 in outer WHERE must return same rows as flat form"
    );

    // Sanity: same threshold via the flat form via query_params should
    // return the same 3 rows.
    let flat_param = db
        .query_params(
            "SELECT id, billable FROM messages WHERE billable >= $1",
            &[Value::Int8(100)],
        )
        .expect("flat parameterised query");
    assert_eq!(flat_param.len(), 3);
}

#[test]
fn cte_with_computed_column_alias_and_parameter_returns_rows() {
    // Closer match to the dashboard's actual repro: the CTE projects a
    // computed column with COALESCE + arithmetic + AS alias, and the
    // outer SELECT references that alias in a parameterised WHERE.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE messages (uuid TEXT, type TEXT, input_tokens INT, output_tokens INT)").expect("create");
    db.execute("INSERT INTO messages VALUES ('a', 'assistant', 80, 20)").expect("ins a"); // 100
    db.execute("INSERT INTO messages VALUES ('b', 'assistant', 200, 50)").expect("ins b"); // 250
    db.execute("INSERT INTO messages VALUES ('c', 'user', 999, 999)").expect("ins c"); // user — filtered out
    db.execute("INSERT INTO messages VALUES ('d', 'assistant', 30, 10)").expect("ins d"); // 40 — below threshold

    let sql = "WITH spend AS (\
        SELECT uuid, COALESCE(input_tokens,0)+COALESCE(output_tokens,0) AS billable \
        FROM messages WHERE type='assistant') \
      SELECT uuid, billable FROM spend WHERE billable >= $1";

    let rows = db.query_params(sql, &[Value::Int8(100)]).expect("cte+computed+$1");
    assert_eq!(
        rows.len(),
        2,
        "CTE with computed alias + parameterised outer WHERE must surface 2 rows"
    );
}

#[test]
fn cte_with_post_substitution_sql_returns_rows() {
    // Mirror the PG-wire extended-query path: the dashboard's pg8000
    // sends Parse with `$1` then Bind with the value; the handler runs
    // `substitute_parameters` (in `prepared.rs`) to splice the value
    // into the SQL textually, then calls `db.query()` on the resulting
    // string. Verify that path also surfaces the rows.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE messages (uuid TEXT, type TEXT, input_tokens INT, output_tokens INT)").expect("create");
    db.execute("INSERT INTO messages VALUES ('a', 'assistant', 80, 20)").expect("ins a");
    db.execute("INSERT INTO messages VALUES ('b', 'assistant', 200, 50)").expect("ins b");

    // What the PG-wire path would actually run after substitution.
    let post_sub = "WITH spend AS (\
        SELECT uuid, COALESCE(input_tokens,0)+COALESCE(output_tokens,0) AS billable \
        FROM messages WHERE type='assistant') \
      SELECT uuid, billable FROM spend WHERE billable >= 100";

    let rows = db.query_with_columns(post_sub).expect("post-substitution CTE query");
    assert_eq!(
        rows.0.len(),
        2,
        "post-substitution CTE must return same 2 rows as the parameterised form"
    );
}

// ---------- Quirk D: SUBSTRING (FROM x FOR y) special form ------------------

#[test]
fn substring_special_form_from_for_works() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (s TEXT)").expect("create");
    db.execute("INSERT INTO t VALUES ('helloworld')").expect("insert");

    // Function form (works today).
    let r1 = db
        .query("SELECT SUBSTR(s, 1, 5) FROM t", &[])
        .expect("substr function form");
    assert_eq!(r1.len(), 1);
    let v1 = match r1[0].values.first() {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(v1, "hello");

    // Special form: SUBSTRING (s FROM x FOR y). Currently errors with
    // "Expression not yet supported: Substring { special: true }".
    let r2 = db.query("SELECT SUBSTRING(s FROM 1 FOR 5) FROM t", &[]);
    let rows = r2.expect("Quirk D: SUBSTRING (FROM FOR) must work");
    let v2 = match rows[0].values.first() {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(v2, "hello");
}

#[test]
fn substring_special_form_from_only_works() {
    // PG also supports `SUBSTRING(s FROM x)` (no FOR) — open-ended slice.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (s TEXT)").expect("create");
    db.execute("INSERT INTO t VALUES ('helloworld')").expect("insert");

    let r = db.query("SELECT SUBSTRING(s FROM 6) FROM t", &[]).expect("substring from");
    let v = match r[0].values.first() {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(v, "world");
}
