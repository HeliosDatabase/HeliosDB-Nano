//! Regression tests for two gaps surfaced by Any2HeliosDB migrations and fixed
//! after v3.58.3:
//!   * parenthesized / nested joins (`(a JOIN b …) JOIN c …`) in SELECT and
//!     CREATE VIEW — a2h's view-body translation emits left-deep nested joins
//!     for multi-table views (Pagila/sakila `*_list`), previously rejected with
//!     "Unsupported table expression: NestedJoin".
//!   * `ALTER TABLE … DROP CONSTRAINT [IF EXISTS]` — a2h's chunked loader drops
//!     FKs before a range-delete + reload pass, previously rejected with
//!     "Unsupported ALTER TABLE operation: DropConstraint".

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn nested_join_parenthesized_in_select_and_view() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE a (id INT4 PRIMARY KEY, b_id INT4)")?;
    db.execute("CREATE TABLE b (id INT4 PRIMARY KEY, c_id INT4)")?;
    db.execute("CREATE TABLE c (id INT4 PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO a VALUES (1, 10)")?;
    db.execute("INSERT INTO b VALUES (10, 100)")?;
    db.execute("INSERT INTO c VALUES (100, 'leaf')")?;

    // Left-deep parenthesized joins — the exact shape a2h emits for *_list views.
    let rows = db.query(
        "SELECT c.name FROM ((a JOIN b ON a.b_id = b.id) JOIN c ON b.c_id = c.id)",
        &[],
    )?;
    assert_eq!(rows.len(), 1, "nested join should return the single joined row");
    assert_eq!(rows[0].values[0], Value::String("leaf".to_string()));

    // And the same shape inside a CREATE VIEW (the migration path).
    db.execute(
        "CREATE VIEW abc AS SELECT c.name AS nm \
         FROM ((a JOIN b ON a.b_id = b.id) JOIN c ON b.c_id = c.id)",
    )?;
    let vrows = db.query("SELECT nm FROM abc", &[])?;
    assert_eq!(vrows.len(), 1);
    assert_eq!(vrows[0].values[0], Value::String("leaf".to_string()));
    Ok(())
}

#[test]
fn alter_table_drop_constraint_fk() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE parent (id INT4 PRIMARY KEY)")?;
    db.execute("CREATE TABLE child (id INT4 PRIMARY KEY, pid INT4)")?;
    db.execute("INSERT INTO parent VALUES (1)")?;
    db.execute(
        "ALTER TABLE child ADD CONSTRAINT child_pid_fk \
         FOREIGN KEY (pid) REFERENCES parent (id)",
    )?;

    // FK is enforced: an orphan child row is rejected.
    assert!(
        db.execute("INSERT INTO child VALUES (1, 999)").is_err(),
        "FK should reject an orphan row while the constraint exists"
    );

    // Drop the named FK (a2h's pre-reload step), then the orphan is allowed —
    // proving the constraint was actually removed (and its cache cleared).
    db.execute("ALTER TABLE child DROP CONSTRAINT child_pid_fk")?;
    db.execute("INSERT INTO child VALUES (1, 999)")?;

    // IF EXISTS on a now-missing constraint is a no-op.
    db.execute("ALTER TABLE child DROP CONSTRAINT IF EXISTS child_pid_fk")?;

    // Without IF EXISTS, dropping a missing constraint errors (PostgreSQL-like).
    assert!(db.execute("ALTER TABLE child DROP CONSTRAINT nope").is_err());
    Ok(())
}

/// GROUP_CONCAT(x) with a single argument defaults the separator to ',' (MySQL /
/// Pagila custom-aggregate semantics). Pagila's `film_list` etc. use the 1-arg
/// form; STRING_AGG still requires an explicit delimiter (PostgreSQL).
#[test]
fn group_concat_one_arg_defaults_comma() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE g (grp INT4, name TEXT)")?;
    db.execute("INSERT INTO g VALUES (1, 'a'), (1, 'b'), (1, 'c')")?;

    let parts = |db: &EmbeddedDatabase, sql: &str, sep: char| -> Result<Vec<String>> {
        let rows = db.query(sql, &[])?;
        let s = match &rows[0].values[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected String, got {:?}", other),
        };
        let mut p: Vec<String> = s.split(sep).map(|x| x.to_string()).collect();
        p.sort();
        Ok(p)
    };

    // 1-arg group_concat → comma-joined.
    assert_eq!(
        parts(&db, "SELECT group_concat(name) FROM g", ',')?,
        vec!["a", "b", "c"]
    );
    // 2-arg group_concat → explicit delimiter honored.
    assert_eq!(
        parts(&db, "SELECT group_concat(name, '|') FROM g", '|')?,
        vec!["a", "b", "c"]
    );
    // STRING_AGG keeps requiring the explicit delimiter.
    assert!(db.query("SELECT string_agg(name) FROM g", &[]).is_err());
    Ok(())
}
