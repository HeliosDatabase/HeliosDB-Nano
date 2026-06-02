//! Token Dashboard outstanding item #4: `SHOW BRANCHES` returned a single
//! empty-name row even after a successful `CREATE BRANCH`. This proves the
//! embedded path lists the default branch and a freshly-created branch with
//! real names. (A live instance showing one blank row indicates a branch
//! registry persisted by an older binary — re-test on a fresh data dir.)

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn show_branches_lists_main_and_created_branch() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE BRANCH 'td_probe' AS OF NOW")?;

    let rows = db.query("SHOW BRANCHES", &[])?;
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

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
