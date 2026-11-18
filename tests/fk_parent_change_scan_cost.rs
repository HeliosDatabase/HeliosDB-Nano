//! What a change to a table in a foreign-key relationship reads, and what it decides.
//!
//! Since foreign-key actions follow parent changes (4.42.0 development), a
//! parent `UPDATE` or `DELETE` asked, for every changed row and every foreign
//! key pointing at it, "which child rows carry the old key" by reading the whole
//! child table, read it again for the `NO ACTION` check, and found the rows an
//! `UPDATE` touched by reading their whole table. Deleting a hundred parent rows
//! read the child table a few hundred times (27.6 s where 4.41.1 took 61 ms on a
//! code-graph cleanup), and a one-row `UPDATE` of any table with a foreign key
//! cost a scan of that table.
//!
//! A command now reads a child table once and keeps it until it stages a write
//! to that table; the final checks read each relationship once; touched rows are
//! read by their id. The first half of this file pins the cost as a ratio between
//! two sizes (no wall-clock limit: the ratio is what the defect changes). The
//! second half pins the answers that depend on a command seeing its own staged
//! actions, which is what the kept rows must not break.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};
use std::time::{Duration, Instant};

fn int(value: &Value) -> i64 {
    match value {
        Value::Int2(n) => i64::from(*n),
        Value::Int4(n) => i64::from(*n),
        Value::Int8(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn count(db: &EmbeddedDatabase, table: &str) -> usize {
    db.query(&format!("SELECT id FROM {table}"), &[]).unwrap().len()
}

fn ids(db: &EmbeddedDatabase, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap()
        .iter()
        .map(|row| int(&row.values[0]))
        .collect()
}

/// A parent table and a child table of `rows` rows each. The children reference
/// the first half of the parents, so the second half of the parents has no child.
fn parent_and_child(db: &EmbeddedDatabase, suffix: &str, rows: i64) {
    db.execute(&format!("CREATE TABLE par{suffix} (id INT PRIMARY KEY, v TEXT)"))
        .unwrap();
    db.execute(&format!(
        "CREATE TABLE chi{suffix} (id INT PRIMARY KEY, pid INT REFERENCES par{suffix}(id), v TEXT)"
    ))
    .unwrap();
    db.execute(&format!("CREATE INDEX chi{suffix}_pid ON chi{suffix}(pid)"))
        .unwrap();
    db.execute(&format!(
        "INSERT INTO par{suffix} SELECT g, 'p' FROM generate_series(1, {rows}) g"
    ))
    .unwrap();
    db.execute(&format!(
        "INSERT INTO chi{suffix} SELECT g, (g % {half}) + 1, 'c' FROM generate_series(1, {rows}) g",
        half = rows / 2
    ))
    .unwrap();
}

/// The fastest of three runs: what the statement costs when nothing disturbs it.
fn fastest(mut run: impl FnMut(usize) -> Duration) -> Duration {
    (0..3).map(&mut run).min().unwrap()
}

#[test]
fn a_one_row_update_of_a_child_table_does_not_read_the_table() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent_and_child(&db, "_small", 500);
    parent_and_child(&db, "_large", 10_000);
    let updates = |table: &str, round: usize| {
        let started = Instant::now();
        for id in 1..=100 {
            let changed = db
                .execute(&format!("UPDATE {table} SET v = 'r{round}' WHERE id = {id}"))
                .unwrap();
            assert_eq!(changed, 1);
        }
        started.elapsed()
    };
    let small = fastest(|round| updates("chi_small", round));
    let large = fastest(|round| updates("chi_large", round));
    // Twenty times the rows. Reading the table per statement made this about
    // twenty times the time; by id it is the same statement.
    assert!(
        large < small * 5 + Duration::from_millis(20),
        "100 one-row updates: {small:?} on 500 rows, {large:?} on 10,000 rows"
    );
    // The foreign-key column itself: still checked, still by id.
    assert_eq!(db.execute("UPDATE chi_large SET pid = 2 WHERE id = 1").unwrap(), 1);
    let refused = db
        .execute("UPDATE chi_large SET pid = 999999 WHERE id = 1")
        .expect_err("no such parent");
    assert!(refused.to_string().contains("Foreign key"), "{refused}");
}

#[test]
fn deleting_many_parents_reads_the_child_table_once() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    parent_and_child(&db, "", 8_000);
    // Parents 4001..=8000 have no child. Delete 10 of them, then 300 of them.
    let delete = |low: i64, high: i64| {
        let started = Instant::now();
        let removed = db
            .execute(&format!("DELETE FROM par WHERE id > {low} AND id <= {high}"))
            .unwrap();
        assert_eq!(removed as i64, high - low);
        started.elapsed()
    };
    let few = [delete(7_990, 8_000), delete(7_980, 7_990), delete(7_970, 7_980)]
        .into_iter()
        .min()
        .unwrap();
    let many = [delete(7_600, 7_900), delete(7_300, 7_600), delete(7_000, 7_300)]
        .into_iter()
        .min()
        .unwrap();
    // Thirty times the parents. Reading the child table per parent made this
    // about thirty times the time.
    assert!(
        many < few * 8 + Duration::from_millis(50),
        "delete of 10 childless parents {few:?}, of 300 {many:?}"
    );
    assert_eq!(count(&db, "chi"), 8_000);
    // A referenced parent is still refused, and nothing of the statement stays.
    let refused = db
        .execute("DELETE FROM par WHERE id <= 3")
        .expect_err("children reference these parents");
    assert!(refused.to_string().contains("Foreign key"), "{refused}");
    assert_eq!(ids(&db, "SELECT id FROM par WHERE id <= 3 ORDER BY id"), [1, 2, 3]);
}

#[test]
fn a_cascade_followed_by_a_check_reads_what_the_cascade_staged() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("CREATE TABLE g (id INT PRIMARY KEY, cid INT REFERENCES c(id))")
        .unwrap();
    db.execute("INSERT INTO p VALUES (1), (2), (3)").unwrap();
    db.execute("INSERT INTO c VALUES (10, 1), (11, 1), (20, 2), (30, 3)")
        .unwrap();
    db.execute("INSERT INTO g VALUES (100, 20)").unwrap();
    // Parents 1 and 3 in one statement: their children go, nothing references them.
    assert_eq!(db.execute("DELETE FROM p WHERE id IN (1, 3)").unwrap(), 2);
    assert_eq!(ids(&db, "SELECT id FROM c ORDER BY id"), [20]);
    // Parent 2: the cascade would remove child 20, which `g` still references.
    let refused = db
        .execute("DELETE FROM p WHERE id = 2")
        .expect_err("g references the child");
    assert!(refused.to_string().contains("Foreign key"), "{refused}");
    assert_eq!(ids(&db, "SELECT id FROM p ORDER BY id"), [2]);
    assert_eq!(ids(&db, "SELECT id FROM c ORDER BY id"), [20]);
    // Once the grandchild is gone the same statement goes through.
    db.execute("DELETE FROM g").unwrap();
    assert_eq!(db.execute("DELETE FROM p WHERE id = 2").unwrap(), 1);
    assert_eq!(count(&db, "c"), 0);
}

#[test]
fn a_two_level_cascade_removes_every_level_for_every_parent() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("CREATE TABLE g (id INT PRIMARY KEY, cid INT REFERENCES c(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("INSERT INTO p SELECT g FROM generate_series(1, 40) g")
        .unwrap();
    db.execute("INSERT INTO c SELECT g, ((g - 1) % 40) + 1 FROM generate_series(1, 120) g")
        .unwrap();
    db.execute("INSERT INTO g SELECT g, ((g - 1) % 120) + 1 FROM generate_series(1, 360) g")
        .unwrap();
    // Thirty parents in one statement: three children each, three grandchildren each.
    assert_eq!(db.execute("DELETE FROM p WHERE id <= 30").unwrap(), 30);
    assert_eq!(count(&db, "p"), 10);
    assert_eq!(count(&db, "c"), 30);
    assert_eq!(count(&db, "g"), 90);
    assert!(ids(&db, "SELECT pid FROM c ORDER BY pid").iter().all(|pid| *pid > 30));
}

#[test]
fn two_foreign_keys_of_one_child_are_both_set_to_null() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE person (id INT PRIMARY KEY)").unwrap();
    db.execute(
        "CREATE TABLE message (id INT PRIMARY KEY, \
         sender INT REFERENCES person(id) ON DELETE SET NULL, \
         receiver INT REFERENCES person(id) ON DELETE SET NULL)",
    )
    .unwrap();
    db.execute("INSERT INTO person VALUES (1), (2), (3)").unwrap();
    db.execute("INSERT INTO message VALUES (1, 1, 2), (2, 2, 1), (3, 1, 1), (4, 3, 3)")
        .unwrap();
    // Both people of messages 1 to 3 go in one statement. The second action on a
    // message must start from what the first one staged.
    assert_eq!(db.execute("DELETE FROM person WHERE id IN (1, 2)").unwrap(), 2);
    let rows = db
        .query("SELECT id, sender, receiver FROM message ORDER BY id", &[])
        .unwrap();
    let cell = |value: &Value| match value {
        Value::Null => None,
        other => Some(int(other)),
    };
    let seen: Vec<(i64, Option<i64>, Option<i64>)> = rows
        .iter()
        .map(|row| (int(&row.values[0]), cell(&row.values[1]), cell(&row.values[2])))
        .collect();
    assert_eq!(
        seen,
        [(1, None, None), (2, None, None), (3, None, None), (4, Some(3), Some(3))]
    );
}

#[test]
fn a_tree_is_removed_from_its_root_and_a_parent_key_update_follows_its_children() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE node (id INT PRIMARY KEY, parent INT REFERENCES node(id) ON DELETE CASCADE)")
        .unwrap();
    db.execute("INSERT INTO node VALUES (1, NULL)").unwrap();
    db.execute("INSERT INTO node SELECT g, g / 2 FROM generate_series(2, 63) g")
        .unwrap();
    db.execute("INSERT INTO node VALUES (100, NULL), (101, 100)").unwrap();
    // The subtree under node 2 is every node whose path to the root passes it: 31 nodes.
    assert_eq!(db.execute("DELETE FROM node WHERE id = 2").unwrap(), 1);
    assert_eq!(count(&db, "node"), 65 - 31);
    assert_eq!(db.execute("DELETE FROM node WHERE id = 1").unwrap(), 1);
    assert_eq!(ids(&db, "SELECT id FROM node ORDER BY id"), [100, 101]);

    db.execute("CREATE TABLE owner (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE item (id INT PRIMARY KEY, owner_id INT REFERENCES owner(id) ON UPDATE CASCADE)")
        .unwrap();
    db.execute("INSERT INTO owner VALUES (1), (2), (3)").unwrap();
    db.execute("INSERT INTO item VALUES (1, 1), (2, 1), (3, 2), (4, 3)")
        .unwrap();
    // Every parent key moves in one statement; each child follows its own parent.
    assert_eq!(db.execute("UPDATE owner SET id = id + 10").unwrap(), 3);
    assert_eq!(ids(&db, "SELECT owner_id FROM item ORDER BY id"), [11, 11, 12, 13]);
}
