//! Probe for HELIOSDB_GAPS_markon.md A6: EXISTS / NOT EXISTS, IN / NOT IN
//! (subquery), and the LEFT JOIN … IS NULL anti-join. Establishes the current
//! behavior on this branch so the fix targets exactly the broken shapes.

use heliosdb_nano::{EmbeddedDatabase, Result, Tuple, Value};

fn ids(rows: &[Tuple]) -> Vec<i32> {
    let mut v: Vec<i32> = rows
        .iter()
        .map(|r| match r.values.first() {
            Some(Value::Int4(x)) => *x,
            Some(Value::Int8(x)) => *x as i32,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn seed(db: &EmbeddedDatabase) -> Result<()> {
    db.execute("CREATE TABLE parent (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO parent VALUES (1,'a'),(2,'b'),(3,'c')")?;
    db.execute("CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, v INT)")?;
    // child 13 references a non-existent parent (99).
    db.execute("INSERT INTO child VALUES (10,1,100),(11,1,200),(12,2,300),(13,99,400)")?;
    Ok(())
}

#[test]
fn exists_correlated() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let rows = db.query(
        "SELECT id FROM parent p WHERE EXISTS (SELECT 1 FROM child c WHERE c.parent_id = p.id) ORDER BY id",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![1, 2]);
    Ok(())
}

#[test]
fn not_exists_correlated() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let rows = db.query(
        "SELECT id FROM parent p WHERE NOT EXISTS (SELECT 1 FROM child c WHERE c.parent_id = p.id) ORDER BY id",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![3]);
    Ok(())
}

#[test]
fn not_in_subquery() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let rows = db.query(
        "SELECT id FROM child WHERE parent_id NOT IN (SELECT id FROM parent) ORDER BY id",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![13]);
    Ok(())
}

#[test]
fn in_subquery() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let rows = db.query(
        "SELECT id FROM child WHERE parent_id IN (SELECT id FROM parent) ORDER BY id",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![10, 11, 12]);
    Ok(())
}

#[test]
fn nested_in_subquery() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let rows = db.query(
        "SELECT id FROM child WHERE parent_id IN (SELECT id FROM parent WHERE id IN (1,2)) ORDER BY id",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![10, 11, 12]);
    Ok(())
}

#[test]
fn left_join_is_null_antijoin() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    seed(&db)?;
    let rows = db.query(
        "SELECT c.id FROM child c LEFT JOIN parent p ON c.parent_id = p.id WHERE p.id IS NULL ORDER BY c.id",
        &[],
    )?;
    assert_eq!(ids(&rows), vec![13]);
    Ok(())
}
