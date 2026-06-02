//! TEMPORARY diagnostic — not a real regression test. Prints how a stored
//! VECTOR reads back and what the distance expression evaluates to, so we can
//! tell a sort bug from a vector-read bug. Delete after diagnosis.

use heliosdb_nano::{EmbeddedDatabase, Result, Value};

#[test]
fn zzdiag_vector_readback_and_distance() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE d (id INT PRIMARY KEY, vec VECTOR(3))")?;
    db.execute("INSERT INTO d VALUES (3, '[3.0,0.0,0.0]')")?;
    db.execute("INSERT INTO d VALUES (1, '[1.0,0.0,0.0]')")?;
    db.execute("INSERT INTO d VALUES (4, '[4.0,0.0,0.0]')")?;
    db.execute("INSERT INTO d VALUES (2, '[2.0,0.0,0.0]')")?;

    eprintln!("--- raw scan: id, vec::text ---");
    let raw = db.query("SELECT id, vec::text FROM d", &[])?;
    for r in &raw {
        eprintln!("{:?}", r.values);
    }

    eprintln!("--- literal distance: id, vec <-> '[0,0,0]' ---");
    let lit = db.query("SELECT id, vec <-> '[0.0,0.0,0.0]' AS dd FROM d", &[])?;
    for r in &lit {
        eprintln!("{:?}", r.values);
    }

    eprintln!("--- param distance: id, vec <-> $1 ---");
    let par = db.query_params(
        "SELECT id, vec <-> $1 AS dd FROM d",
        &[Value::Vector(vec![0.0, 0.0, 0.0])],
    )?;
    for r in &par {
        eprintln!("{:?}", r.values);
    }

    eprintln!("--- A) SELECT id ... ORDER BY vec<->lit (expr, vec NOT projected) ---");
    let a = db.query("SELECT id FROM d ORDER BY vec <-> '[0.0,0.0,0.0]'", &[])?;
    eprintln!("{:?}", a.iter().map(|r| r.values.first().cloned()).collect::<Vec<_>>());

    eprintln!("--- B) SELECT id, dist ... ORDER BY dist (alias, dist projected) ---");
    let b = db.query(
        "SELECT id, vec <-> '[0.0,0.0,0.0]' AS dist FROM d ORDER BY dist",
        &[],
    )?;
    eprintln!("{:?}", b.iter().map(|r| r.values.first().cloned()).collect::<Vec<_>>());

    eprintln!("--- C) SELECT id, dist ... ORDER BY vec<->lit (expr, dist also projected) ---");
    let c = db.query(
        "SELECT id, vec <-> '[0.0,0.0,0.0]' AS dist FROM d ORDER BY vec <-> '[0.0,0.0,0.0]'",
        &[],
    )?;
    eprintln!("{:?}", c.iter().map(|r| r.values.first().cloned()).collect::<Vec<_>>());

    eprintln!("--- D) EXPLAIN of A ---");
    let e = db.query("EXPLAIN SELECT id FROM d ORDER BY vec <-> '[0.0,0.0,0.0]'", &[])?;
    for r in &e {
        eprintln!("{:?}", r.values);
    }
    Ok(())
}
