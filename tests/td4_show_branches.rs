//! Token Dashboard outstanding item #4: `SHOW BRANCHES` returned a single
//! empty-name row over the PostgreSQL path even after successful branch
//! creation. These embedded regressions prove fresh branch metadata is
//! enumerated by name; protocol coverage lives in the PostgreSQL handler tests.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

fn first_column_names(rows: &[heliosdb_nano::Tuple]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn show_branches_lists_main_and_created_branch() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE BRANCH 'td_probe' AS OF NOW")?;

    let rows = db.query("SHOW BRANCHES", &[])?;
    let names = first_column_names(&rows);

    assert!(
        names.iter().any(|n| n == "main"),
        "expected the main branch listed, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "td_probe"),
        "expected the created branch listed, got {names:?}"
    );
    assert!(
        names.iter().all(|n| !n.is_empty()),
        "no branch name should be empty, got {names:?}"
    );
    Ok(())
}

#[test]
fn show_branches_lists_multiple_fresh_data_dir_branches_after_writes() -> Result<()> {
    let data_dir = tempfile::tempdir().expect("fresh data dir");
    let db = EmbeddedDatabase::new(data_dir.path())?;

    db.execute("CREATE TABLE t(x INT)")?;
    db.execute("INSERT INTO t VALUES(1),(2),(3)")?;
    db.execute("CREATE BRANCH 'alpha' AS OF NOW")?;
    db.execute("CREATE BRANCH 'beta' AS OF NOW")?;

    let (rows, columns) = db.query_with_columns("SHOW BRANCHES")?;
    assert!(
        columns.iter().any(|column| column == "branch_name"),
        "SHOW BRANCHES should expose branch_name; columns={columns:?}"
    );

    let names = first_column_names(&rows);
    for expected in ["main", "alpha", "beta"] {
        assert!(
            names.iter().any(|name| name == expected),
            "expected {expected} branch listed, got names={names:?}; rows={rows:?}"
        );
    }
    assert!(
        names.iter().all(|name| !name.is_empty()),
        "no branch name should be empty, got names={names:?}; rows={rows:?}"
    );

    Ok(())
}
