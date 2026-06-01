//! Regression for follow-up IFNE: `CREATE BRANCH IF NOT EXISTS` must parse the
//! requested branch name and be idempotent.

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

fn branch_names(db: &EmbeddedDatabase) -> Vec<String> {
    let (rows, _) = db
        .query_with_columns("SELECT branch_name FROM pg_database_branches()")
        .expect("branches");
    names_from_rows(&rows)
}

fn names_from_rows(rows: &[Tuple]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| match row.values.first() {
            Some(Value::String(name)) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn count_name(names: &[String], expected: &str) -> usize {
    names.iter().filter(|name| name.as_str() == expected).count()
}

#[test]
fn ifne_short_create_branch_parses_requested_name_and_is_idempotent() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    db.execute("CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW")
        .expect("first CREATE BRANCH IF NOT EXISTS should create feature_x");

    let names = branch_names(&db);
    assert!(
        names.iter().any(|name| name == "feature_x"),
        "CREATE BRANCH IF NOT EXISTS must create feature_x; names={names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "IF"),
        "IF NOT EXISTS must not be parsed as branch name IF; names={names:?}"
    );

    db.execute("CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW")
        .expect("second CREATE BRANCH IF NOT EXISTS should be a no-op");

    let names = branch_names(&db);
    assert_eq!(
        count_name(&names, "feature_x"),
        1,
        "idempotent create must not duplicate feature_x; names={names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "IF"),
        "idempotent create must still not create branch IF; names={names:?}"
    );
}

#[test]
fn ifne_database_branch_from_parent_is_idempotent() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    db.execute("CREATE DATABASE BRANCH IF NOT EXISTS feature_db FROM main AS OF NOW")
        .expect("first CREATE DATABASE BRANCH IF NOT EXISTS should create feature_db");
    db.execute("CREATE DATABASE BRANCH IF NOT EXISTS feature_db FROM main AS OF NOW")
        .expect("second CREATE DATABASE BRANCH IF NOT EXISTS should be a no-op");

    let names = branch_names(&db);
    assert_eq!(
        count_name(&names, "feature_db"),
        1,
        "CREATE DATABASE BRANCH IF NOT EXISTS should create exactly one feature_db; names={names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "IF"),
        "CREATE DATABASE BRANCH IF NOT EXISTS must not create branch IF; names={names:?}"
    );
}

#[test]
fn ifne_quoted_branch_name_keeps_existing_quote_stripping() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    db.execute(r#"CREATE BRANCH IF NOT EXISTS "feature-quoted" AS OF NOW"#)
        .expect("quoted branch name after IF NOT EXISTS should parse");
    db.execute(r#"CREATE BRANCH IF NOT EXISTS "feature-quoted" AS OF NOW"#)
        .expect("quoted branch IF NOT EXISTS should be idempotent");

    let names = branch_names(&db);
    assert_eq!(
        count_name(&names, "feature-quoted"),
        1,
        "quoted branch name should be stored without quotes exactly once; names={names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "IF"),
        "quoted IF NOT EXISTS must not create branch IF; names={names:?}"
    );
}

#[test]
fn ifne_duplicate_without_clause_still_errors() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    db.execute("CREATE BRANCH plain_duplicate AS OF NOW")
        .expect("first plain branch create should succeed");

    let err = db
        .execute("CREATE BRANCH plain_duplicate AS OF NOW")
        .expect_err("duplicate plain CREATE BRANCH must still error");
    assert!(
        err.to_string().contains("already exists"),
        "duplicate plain CREATE BRANCH should report already exists; err={err:?}"
    );
}

#[test]
fn ifne_many_idempotent_creates_remain_fast_and_named_correctly() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let started = std::time::Instant::now();

    for idx in 0..100 {
        let sql = format!("CREATE BRANCH IF NOT EXISTS ifne_stress_{idx:03} AS OF NOW");
        db.execute(&sql).expect("first IF NOT EXISTS create should succeed");
        db.execute(&sql).expect("second IF NOT EXISTS create should be a no-op");
    }

    let names = branch_names(&db);
    for idx in 0..100 {
        let expected = format!("ifne_stress_{idx:03}");
        assert_eq!(
            count_name(&names, &expected),
            1,
            "stress branch should exist exactly once: {expected}; names={names:?}"
        );
    }
    assert!(
        !names.iter().any(|name| name == "IF"),
        "stress IF NOT EXISTS must not create branch IF; names={names:?}"
    );

    eprintln!(
        "IFNE stress: 100 first creates + 100 idempotent no-ops completed in {:?}",
        started.elapsed()
    );
}
