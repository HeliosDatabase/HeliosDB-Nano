//! Regression for checklist item T4: `CREATE BRANCH <name> AS OF NOW`
//! must store the user-visible branch name so `SHOW BRANCHES` does not
//! surface an empty-name metadata row.

use heliosdb_nano::{EmbeddedDatabase, Tuple, Value};

fn branch_names(rows: &[Tuple]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| match row.values.first() {
            Some(Value::String(name)) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn t4_short_create_branch_as_of_now_preserves_name_in_show_branches() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    db.execute("CREATE BRANCH t4_short AS OF NOW")
        .expect("create short-form branch");

    let (rows, columns) = db.query_with_columns("SHOW BRANCHES").expect("show branches");
    assert!(
        columns.iter().any(|column| column == "branch_name" || column == "name"),
        "SHOW BRANCHES should expose a name column; columns={columns:?}"
    );

    let names = branch_names(&rows);
    assert!(
        names.iter().any(|name| name == "t4_short"),
        "SHOW BRANCHES must list t4_short; names={names:?}; rows={rows:?}"
    );
    assert!(
        !names.iter().any(|name| name.is_empty()),
        "SHOW BRANCHES must not return an empty branch-name row; names={names:?}; rows={rows:?}"
    );
}

#[test]
fn t4_short_create_branch_as_of_now_preserves_name_in_pg_function() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    db.execute("CREATE BRANCH t4_pg_func AS OF NOW")
        .expect("create short-form branch");

    let (rows, columns) = db
        .query_with_columns("SELECT * FROM pg_database_branches()")
        .expect("pg_database_branches");
    assert!(
        columns.iter().any(|column| column == "branch_name" || column == "name"),
        "pg_database_branches should expose a name column; columns={columns:?}"
    );

    let names = branch_names(&rows);
    assert!(
        names.iter().any(|name| name == "t4_pg_func"),
        "pg_database_branches() must list t4_pg_func; names={names:?}; rows={rows:?}"
    );
    assert!(
        !names.iter().any(|name| name.is_empty()),
        "pg_database_branches() must not return an empty branch-name row; names={names:?}; rows={rows:?}"
    );
}

#[test]
fn t4_database_branch_as_of_now_still_preserves_name() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");

    db.execute("CREATE DATABASE BRANCH t4_database FROM main AS OF NOW")
        .expect("create database branch");

    let (rows, _) = db.query_with_columns("SHOW BRANCHES").expect("show branches");
    let names = branch_names(&rows);
    assert!(
        names.iter().any(|name| name == "t4_database"),
        "SHOW BRANCHES must list t4_database; names={names:?}; rows={rows:?}"
    );
    assert!(
        !names.iter().any(|name| name.is_empty()),
        "SHOW BRANCHES must not return an empty branch-name row; names={names:?}; rows={rows:?}"
    );
}

#[test]
fn t4_short_create_branch_as_of_timestamp_preserves_name() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.storage
        .snapshot_manager()
        .register_snapshot(100)
        .expect("register snapshot");

    db.execute(&format!(
        "CREATE BRANCH t4_timestamp AS OF TIMESTAMP '{}'",
        chrono::Utc::now()
            .checked_add_signed(chrono::Duration::days(1))
            .expect("valid timestamp")
            .format("%Y-%m-%d %H:%M:%S")
    ))
    .expect("create short-form branch at timestamp");

    let (rows, _) = db.query_with_columns("SHOW BRANCHES").expect("show branches");
    let names = branch_names(&rows);
    assert!(
        names.iter().any(|name| name == "t4_timestamp"),
        "SHOW BRANCHES must list t4_timestamp; names={names:?}; rows={rows:?}"
    );
    assert!(
        !names.iter().any(|name| name.is_empty()),
        "SHOW BRANCHES must not return an empty branch-name row; names={names:?}; rows={rows:?}"
    );
}

#[test]
fn t4_branch_name_listing_stress_has_no_empty_names() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    let started = std::time::Instant::now();

    for idx in 0..50 {
        db.execute(&format!("CREATE BRANCH t4_stress_{idx:02} AS OF NOW"))
            .expect("create stress branch");
    }

    let (rows, _) = db.query_with_columns("SHOW BRANCHES").expect("show branches");
    let names = branch_names(&rows);
    for idx in 0..50 {
        let expected = format!("t4_stress_{idx:02}");
        assert!(
            names.iter().any(|name| name == &expected),
            "SHOW BRANCHES must list {expected}; names={names:?}; rows={rows:?}"
        );
    }
    assert!(
        !names.iter().any(|name| name.is_empty()),
        "SHOW BRANCHES must not return an empty branch-name row; names={names:?}; rows={rows:?}"
    );

    eprintln!(
        "T4 stress: created 50 short-form AS OF NOW branches and listed {} active branches in {:?}",
        names.len(),
        started.elapsed()
    );
}
