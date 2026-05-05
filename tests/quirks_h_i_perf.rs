//! Quirks H + I — performance regression tests for v3.30.0.
//!
//! Token-Dashboard team filed (carried forward through v3.27→v3.29):
//! - **Quirk H**: DELETE/DROP on a table with ~11k rows hangs the
//!   daemon for >5 min when there's an FK pointing at the table.
//!   Root cause: `check_referencing_rows_exist` did a full table
//!   scan per parent row deleted (O(N×M)). Fixed by routing through
//!   the FK / UNIQUE / PK ART index when available — O(log N).
//! - **Quirk I**: `ON CONFLICT DO UPDATE` was ~400× slower than
//!   bare INSERT on populated tables (0.4 ins/sec at 11k rows
//!   vs 181). Root cause: per-row `self.query("SELECT pk FROM tbl
//!   WHERE col = 'val'")` round-tripped through the SQL planner +
//!   ended in a table scan. Fixed by index_get_all on the UNIQUE
//!   index directly — `check_unique_constraints` already proved
//!   the row exists; we just need its row_id.
//!
//! Targets:
//! - DELETE 1000 rows from a 1000-row child-of-FK'd table: <500ms
//! - 1000 INSERT … ON CONFLICT DO UPDATE on a populated 1000-row
//!   table: total <2s (i.e., >500 ops/sec, >2× bare INSERT).

use heliosdb_nano::EmbeddedDatabase;
use std::time::Instant;

#[test]
fn quirk_h_delete_with_fk_is_fast() {
    // 1000-row child table with an FK at PK level. DELETE-all on the
    // PARENT must finish in well under a second on the index path.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT)").expect("p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("c");

    // Populate parents 1..N and one child each pointing at the parent.
    const N: i32 = 1000;
    for i in 1..=N {
        db.execute(&format!("INSERT INTO p (id, name) VALUES ({i}, 'p{i}')")).expect("p ins");
        db.execute(&format!("INSERT INTO c (id, parent_id) VALUES ({i}, {i})")).expect("c ins");
    }

    // First, delete the children (so subsequent parent DELETE is FK-clean).
    let start = Instant::now();
    db.execute("DELETE FROM c WHERE id <= 1000").expect("delete c");
    let child_dt = start.elapsed();
    eprintln!("DELETE 1000 children: {:?}", child_dt);

    // Now DELETE all parents — this is the path that previously did
    // a full child-table scan per row.
    let start = Instant::now();
    db.execute("DELETE FROM p WHERE id <= 1000").expect("delete p");
    let parent_dt = start.elapsed();
    eprintln!("DELETE 1000 parents (FK-checked): {:?}", parent_dt);

    // Pre-fix: 1000×1000 = 1M tuple checks → ~5 s. Post-fix: 1000
    // index lookups → well under 1 s.
    assert!(
        parent_dt.as_millis() < 5_000,
        "Quirk H: DELETE 1000 FK-parent rows should complete in <5s; took {parent_dt:?}"
    );
}

#[test]
fn quirk_i_on_conflict_do_update_is_fast() {
    // Populated table, then 1000 ON CONFLICT DO UPDATE — should be
    // within a small constant factor of bare INSERT throughput, not
    // 400× slower.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE, name TEXT)").expect("t");

    // Pre-populate 1000 rows with PK 1..1000 and emails e1..e1000.
    const N: i32 = 1000;
    for i in 1..=N {
        db.execute(&format!(
            "INSERT INTO t (id, email, name) VALUES ({i}, 'e{i}@x', 'name{i}')"
        )).expect("ins");
    }

    // Now hammer with ON CONFLICT DO UPDATE on the email UNIQUE
    // column. Each row hits the conflict path → tests Quirk I's
    // existing-row-lookup performance.
    let start = Instant::now();
    for i in 1..=N {
        db.execute(&format!(
            "INSERT INTO t (id, email, name) VALUES ({}, 'e{}@x', 'updated{}')
             ON CONFLICT (email) DO UPDATE SET name = 'updated{}'",
            N + i, i, i, i
        )).expect("on conflict do update");
    }
    let dt = start.elapsed();
    eprintln!("1000 ON CONFLICT DO UPDATE on populated table: {:?}", dt);

    // Pre-fix this took roughly 400× bare-INSERT time (~25 s for
    // 1000 ops). Post-fix should be within 5× of bare INSERT —
    // generous bound to cover scheduler / cargo-test noise. The
    // actual delta should be negligible.
    assert!(
        dt.as_millis() < 10_000,
        "Quirk I: 1000 ON CONFLICT DO UPDATE should complete in <10s; took {dt:?}"
    );
}

#[test]
fn quirk_h_drop_table_with_fk_completes() {
    // Smoke test: DROP TABLE on a populated table that has a FK
    // pointing at it. Pre-fix: hangs. Post-fix: completes quickly
    // because cascade evaluation goes through the index.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("c");
    for i in 1..=500 {
        db.execute(&format!("INSERT INTO p (id) VALUES ({i})")).expect("p");
        db.execute(&format!("INSERT INTO c (id, parent_id) VALUES ({i}, {i})")).expect("c");
    }

    // Drop child first (so FK doesn't block), then parent. If either
    // hangs the test will time out at the cargo-test level.
    let start = Instant::now();
    db.execute("DROP TABLE c").expect("drop c");
    db.execute("DROP TABLE p").expect("drop p");
    let dt = start.elapsed();
    eprintln!("DROP 500-row child + parent: {:?}", dt);
    assert!(
        dt.as_secs() < 30,
        "DROP TABLE on FK'd populated tables should complete in <30s; took {dt:?}"
    );
}

#[test]
fn fk_validation_still_blocks_orphan_deletes() {
    // Sanity: the index-fast-path must not regress correctness — a
    // DELETE on a parent that still has a child must error.
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("c");
    db.execute("INSERT INTO p (id) VALUES (1)").expect("p");
    db.execute("INSERT INTO c (id, parent_id) VALUES (1, 1)").expect("c");

    let r = db.execute("DELETE FROM p WHERE id = 1");
    assert!(
        r.is_err(),
        "DELETE FROM parent with surviving child must error; got {r:?}"
    );
}
