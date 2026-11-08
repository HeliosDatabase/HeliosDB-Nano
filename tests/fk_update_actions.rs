//! Declared FK ON UPDATE actions, both DML families, transaction/savepoint and
//! concurrent child-write boundaries. No test accepts an arbitrary error or an orphan.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::session::{IsolationLevel, SessionId};
use heliosdb_nano::{EmbeddedDatabase, Error, Value};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Family {
    Text,
    Params,
}
const FAMILIES: [Family; 2] = [Family::Text, Family::Params];
fn run(db: &EmbeddedDatabase, family: Family, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(sql),
        Family::Params => db.execute_params(sql, &[]),
    }
}
fn update(db: &EmbeddedDatabase, family: Family, from: i32, to: i32) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute(&format!("UPDATE p SET id = {to} WHERE id = {from}")),
        Family::Params => db.execute_params(
            "UPDATE p SET id = $1 WHERE id = $2",
            &[Value::Int4(to), Value::Int4(from)],
        ),
    }
}
fn session_run(db: &EmbeddedDatabase, family: Family, session: SessionId, sql: &str) -> heliosdb_nano::Result<u64> {
    match family {
        Family::Text => db.execute_for_session(session, sql),
        Family::Params => db.execute_params_for_session(session, sql, &[]),
    }
}
fn rows(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    db.query(&format!("SELECT * FROM {table} ORDER BY id"), &[])
        .unwrap()
        .into_iter()
        .map(|row| row.values)
        .collect()
}
fn fk_error(error: &Error) {
    assert!(
        matches!(error, Error::ConstraintViolation(_)),
        "expected FK constraint error, got {error}"
    );
    assert!(
        error.to_string().to_ascii_lowercase().contains("foreign key"),
        "{error}"
    );
}
fn setup(action: &str, timing: &str) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute(&format!("CREATE TABLE c (id INT PRIMARY KEY, pid INT DEFAULT 0, CONSTRAINT c_fk FOREIGN KEY (pid) REFERENCES p(id) ON UPDATE {action} {timing})")).unwrap();
    db.execute("INSERT INTO p VALUES (0), (1), (2)").unwrap();
    db.execute("INSERT INTO c VALUES (10, 1), (11, 1), (12, 2), (13, NULL)")
        .unwrap();
    db
}
#[test]
fn declared_actions_change_only_matching_children_or_refuse_atomically() {
    for family in FAMILIES {
        for action in ["NO ACTION", "RESTRICT", "CASCADE", "SET NULL", "SET DEFAULT"] {
            let db = setup(action, "");
            let before_p = rows(&db, "p");
            let before_c = rows(&db, "c");
            assert_eq!(update(&db, family, 1, 1).unwrap(), 1, "no-op key updates are allowed");
            let result = update(&db, family, 1, 10);
            if matches!(action, "NO ACTION" | "RESTRICT") {
                fk_error(&result.unwrap_err());
                assert_eq!(rows(&db, "p"), before_p);
                assert_eq!(rows(&db, "c"), before_c);
            } else {
                assert_eq!(result.unwrap(), 1);
                let value = match action {
                    "CASCADE" => Value::Int4(10),
                    "SET NULL" => Value::Null,
                    _ => Value::Int4(0),
                };
                assert_eq!(
                    rows(&db, "c"),
                    vec![
                        vec![Value::Int4(10), value.clone()],
                        vec![Value::Int4(11), value],
                        vec![Value::Int4(12), Value::Int4(2)],
                        vec![Value::Int4(13), Value::Null]
                    ],
                    "{family:?} {action}"
                );
                assert_eq!(
                    rows(&db, "p"),
                    vec![vec![Value::Int4(0)], vec![Value::Int4(2)], vec![Value::Int4(10)]]
                );
            }
        }
    }
}
#[test]
fn composite_actions_preserve_pair_order_and_match_simple_null_exemption() {
    for family in FAMILIES {
        for action in ["NO ACTION", "RESTRICT", "CASCADE", "SET NULL", "SET DEFAULT"] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE p (a INT, b INT, PRIMARY KEY (a,b))").unwrap();
            db.execute("INSERT INTO p VALUES (0,0), (1,2), (3,4)").unwrap();
            db.execute(&format!("CREATE TABLE c (id INT PRIMARY KEY, x INT DEFAULT 0, y INT DEFAULT 0, FOREIGN KEY (y,x) REFERENCES p(b,a) ON UPDATE {action})")).unwrap();
            db.execute("INSERT INTO c VALUES (1,1,2), (2,NULL,2), (3,3,4)").unwrap();
            let before = rows(&db, "c");
            let result = run(&db, family, "UPDATE p SET a = 10, b = 20 WHERE a = 1 AND b = 2");
            if matches!(action, "NO ACTION" | "RESTRICT") {
                fk_error(&result.unwrap_err());
                assert_eq!(rows(&db, "c"), before);
            } else {
                result.unwrap();
                let (x, y) = match action {
                    "CASCADE" => (Value::Int4(10), Value::Int4(20)),
                    "SET NULL" => (Value::Null, Value::Null),
                    _ => (Value::Int4(0), Value::Int4(0)),
                };
                assert_eq!(
                    rows(&db, "c"),
                    vec![
                        vec![Value::Int4(1), x, y],
                        vec![Value::Int4(2), Value::Null, Value::Int4(2)],
                        vec![Value::Int4(3), Value::Int4(3), Value::Int4(4)]
                    ]
                );
            }
        }
    }
}
#[test]
fn generated_child_changes_still_obey_not_null_check_unique_and_fk_constraints() {
    for family in FAMILIES {
        for (action, definition, expected) in [
            ("SET NULL", "pid INT NOT NULL", "NOT NULL"),
            ("SET DEFAULT", "pid INT DEFAULT 9", "Foreign key"),
            ("CASCADE", "pid INT CHECK (pid < 5)", "CHECK"),
        ] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
            db.execute("INSERT INTO p VALUES (1)").unwrap();
            db.execute(&format!("CREATE TABLE c (id INT PRIMARY KEY, {definition}, FOREIGN KEY (pid) REFERENCES p(id) ON UPDATE {action})")).unwrap();
            db.execute("INSERT INTO c VALUES (1,1)").unwrap();
            let error = update(&db, family, 1, 10).unwrap_err();
            assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(1)]]);
            assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(1)]]);
        }
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p VALUES (0), (1), (2)").unwrap();
        db.execute(
            "CREATE TABLE c (id INT PRIMARY KEY, pid INT UNIQUE DEFAULT 0 REFERENCES p(id) ON UPDATE SET DEFAULT)",
        )
        .unwrap();
        db.execute("INSERT INTO c VALUES (1,1), (2,2)").unwrap();
        let error = run(&db, family, "UPDATE p SET id = id + 10 WHERE id > 0").unwrap_err();
        assert!(error.to_string().to_ascii_lowercase().contains("unique"), "{error}");
        assert_eq!(
            rows(&db, "p"),
            vec![vec![Value::Int4(0)], vec![Value::Int4(1)], vec![Value::Int4(2)]]
        );
        assert_eq!(
            rows(&db, "c"),
            vec![
                vec![Value::Int4(1), Value::Int4(1)],
                vec![Value::Int4(2), Value::Int4(2)]
            ]
        );
    }
}
#[test]
fn cross_type_cascade_uses_checked_child_casts_and_rolls_back_overflow() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (id BIGINT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO c VALUES (1,1)").unwrap();
        run(&db, family, "UPDATE p SET id = 10 WHERE id = 1").unwrap();
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(10));
        let error = run(&db, family, "UPDATE p SET id = 4294967297 WHERE id = 10").unwrap_err();
        assert!(error.to_string().contains("out of range"), "{error}");
        assert_eq!(rows(&db, "p"), vec![vec![Value::Int8(10)]]);
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(10));
    }
}
#[test]
fn actions_are_immediate_but_transaction_and_savepoint_rollback_restore_all_rows_and_keys() {
    for family in FAMILIES {
        for action in ["CASCADE", "SET NULL", "SET DEFAULT"] {
            let db = setup(action, "DEFERRABLE INITIALLY DEFERRED");
            let before_p = rows(&db, "p");
            let before_c = rows(&db, "c");
            run(&db, family, "BEGIN").unwrap();
            run(&db, family, "SAVEPOINT s").unwrap();
            update(&db, family, 1, 10).unwrap();
            assert_ne!(rows(&db, "c"), before_c, "actions run before commit even deferred");
            run(&db, family, "ROLLBACK TO SAVEPOINT s").unwrap();
            assert_eq!(rows(&db, "p"), before_p);
            assert_eq!(rows(&db, "c"), before_c);
            run(&db, family, "COMMIT").unwrap();
            assert_eq!(rows(&db, "c"), before_c);
            run(&db, family, "BEGIN").unwrap();
            update(&db, family, 1, 10).unwrap();
            run(&db, family, "ROLLBACK").unwrap();
            assert_eq!(rows(&db, "p"), before_p);
            assert_eq!(rows(&db, "c"), before_c);
            update(&db, family, 1, 10).unwrap();
            assert_eq!(
                db.query("SELECT id FROM p WHERE id = 10", &[]).unwrap().len(),
                1,
                "restored index keys permit the retry"
            );
        }
    }
}
#[test]
fn deferred_no_action_checks_final_children_and_restrict_never_defers() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY DEFERRED");
        run(&db, family, "BEGIN").unwrap();
        update(&db, family, 1, 10).unwrap();
        run(&db, family, "UPDATE c SET pid = 10 WHERE pid = 1").unwrap();
        run(&db, family, "COMMIT").unwrap();
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(10));
        run(&db, family, "BEGIN").unwrap();
        update(&db, family, 10, 20).unwrap();
        fk_error(&run(&db, family, "COMMIT").unwrap_err());
        assert_eq!(db.query("SELECT id FROM p WHERE id = 10", &[]).unwrap().len(), 1);
        let db = setup("RESTRICT", "DEFERRABLE INITIALLY DEFERRED");
        run(&db, family, "BEGIN").unwrap();
        fk_error(&update(&db, family, 1, 10).unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
    }
}
#[test]
fn set_constraints_timing_is_transaction_owned_and_immediate_is_not_initially_deferred() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY DEFERRED");
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SET CONSTRAINTS ALL IMMEDIATE").unwrap();
        fk_error(&update(&db, family, 1, 10).unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SAVEPOINT s").unwrap();
        update(&db, family, 1, 10).unwrap();
        run(&db, family, "ROLLBACK TO SAVEPOINT s").unwrap();
        run(&db, family, "COMMIT").unwrap();
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(1));
        run(&db, family, "BEGIN").unwrap();
        update(&db, family, 1, 10).unwrap();
        fk_error(&run(&db, family, "SET CONSTRAINTS ALL IMMEDIATE").unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
    }
}
#[test]
fn session_deferral_does_not_change_another_transactions_immediate_fk() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY IMMEDIATE");
        db.execute("CREATE TABLE p2 (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p2 VALUES (1)").unwrap();
        db.execute("CREATE TABLE c2 (id INT PRIMARY KEY, pid INT REFERENCES p2(id) DEFERRABLE INITIALLY IMMEDIATE)")
            .unwrap();
        db.execute("INSERT INTO c2 VALUES (1,1)").unwrap();
        let a = db.create_session("a", IsolationLevel::ReadCommitted).unwrap();
        let b = db.create_session("b", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(a).unwrap();
        db.begin_transaction_for_session(b).unwrap();
        session_run(&db, family, a, "SET CONSTRAINTS ALL DEFERRED").unwrap();
        session_run(&db, family, a, "UPDATE p SET id = 10 WHERE id = 1").unwrap();
        fk_error(&session_run(&db, family, b, "UPDATE p2 SET id = 10 WHERE id = 1").unwrap_err());
        db.rollback_transaction_for_session(b).unwrap();
        session_run(&db, family, a, "UPDATE c SET pid = 10 WHERE pid = 1").unwrap();
        db.commit_transaction_for_session(a).unwrap();
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(10));
        assert_eq!(rows(&db, "c2")[0][1], Value::Int4(1));
    }
}
#[test]
fn key_changing_upserts_run_the_same_actions_on_both_families() {
    for family in FAMILIES {
        for action in ["NO ACTION", "RESTRICT", "CASCADE", "SET NULL", "SET DEFAULT"] {
            let db = setup(action, "");
            let before = rows(&db, "c");
            let result = match family {
                Family::Text => db.execute("INSERT INTO p VALUES (1) ON CONFLICT(id) DO UPDATE SET id = 10"),
                Family::Params => db.execute_params(
                    "INSERT INTO p VALUES ($1) ON CONFLICT(id) DO UPDATE SET id = $2",
                    &[Value::Int4(1), Value::Int4(10)],
                ),
            };
            if matches!(action, "NO ACTION" | "RESTRICT") {
                fk_error(&result.unwrap_err());
                assert_eq!(rows(&db, "c"), before);
            } else {
                result.unwrap();
                assert_eq!(
                    rows(&db, "c")[0][1],
                    match action {
                        "CASCADE" => Value::Int4(10),
                        "SET NULL" => Value::Null,
                        _ => Value::Int4(0),
                    }
                );
            }
        }
    }
}
#[test]
fn self_references_cyclic_keys_and_multilevel_cascades_reach_a_valid_fixed_point() {
    for family in FAMILIES {
        for action in ["NO ACTION", "RESTRICT"] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute(&format!(
                "CREATE TABLE n (id INT PRIMARY KEY, parent INT REFERENCES n(id) ON UPDATE {action})"
            ))
            .unwrap();
            db.execute("INSERT INTO n VALUES (1,NULL)").unwrap();
            db.execute("UPDATE n SET parent = 1 WHERE id = 1").unwrap();
            run(&db, family, "UPDATE n SET id = 10, parent = 10 WHERE id = 1").unwrap();
            assert_eq!(rows(&db, "n"), vec![vec![Value::Int4(10), Value::Int4(10)]]);
        }
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE n (id INT PRIMARY KEY, parent INT REFERENCES n(id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO n VALUES (1,NULL), (2,1)").unwrap();
        db.execute("UPDATE n SET parent = 1 WHERE id = 1").unwrap();
        run(&db, family, "UPDATE n SET id = 10 WHERE id = 1").unwrap();
        assert_eq!(
            rows(&db, "n"),
            vec![
                vec![Value::Int4(2), Value::Int4(10)],
                vec![Value::Int4(10), Value::Int4(10)]
            ]
        );
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE a (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO a VALUES (1)").unwrap();
        db.execute("CREATE TABLE b (id INT PRIMARY KEY REFERENCES a(id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO b VALUES (1)").unwrap();
        db.execute("ALTER TABLE a ADD CONSTRAINT a_fk FOREIGN KEY (id) REFERENCES b(id) ON UPDATE CASCADE")
            .unwrap();
        run(&db, family, "UPDATE a SET id = 10 WHERE id = 1").unwrap();
        assert_eq!(rows(&db, "a"), vec![vec![Value::Int4(10)]]);
        assert_eq!(rows(&db, "b"), rows(&db, "a"));
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY REFERENCES p(id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO c VALUES (1)").unwrap();
        db.execute("CREATE TABLE g (id INT PRIMARY KEY, cid INT REFERENCES c(id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO g VALUES (1,1)").unwrap();
        update(&db, family, 1, 10).unwrap();
        assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(10)]]);
        assert_eq!(rows(&db, "g"), vec![vec![Value::Int4(1), Value::Int4(10)]]);
    }
}
#[test]
fn concurrent_old_key_child_insert_cannot_slip_past_parent_update_or_delete() {
    for family in FAMILIES {
        for delete in [false, true] {
            for action in ["RESTRICT", "CASCADE", "SET NULL"] {
                let db = EmbeddedDatabase::new_in_memory().unwrap();
                db.storage.debug_set_table_lock_timeout(Duration::from_secs(10));
                db.execute("CREATE TABLE z_parent (id INT PRIMARY KEY)").unwrap();
                db.execute("INSERT INTO z_parent VALUES (1)").unwrap();
                db.execute(&format!("CREATE TABLE a_child (id INT PRIMARY KEY, pid INT REFERENCES z_parent(id) ON UPDATE {action} ON DELETE {action})")).unwrap();
                db.execute("INSERT INTO a_child VALUES (1,1)").unwrap();
                let holder = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
                db.begin_transaction_for_session(holder).unwrap();
                db.execute_for_session(holder, "UPDATE a_child SET pid = pid WHERE id = -1")
                    .unwrap();
                let (parent, child) = std::thread::scope(|scope| {
                    let db_ref = &db;
                    let parent = scope.spawn(move || {
                        run(
                            db_ref,
                            family,
                            if delete {
                                "DELETE FROM z_parent WHERE id = 1"
                            } else {
                                "UPDATE z_parent SET id = 10 WHERE id = 1"
                            },
                        )
                    });
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while db.storage.debug_table_lock_waiters("a_child").0 == 0 {
                        assert!(
                            !parent.is_finished() && Instant::now() < deadline,
                            "parent must wait for the existing child writer"
                        );
                        std::thread::yield_now();
                    }
                    let child = scope.spawn(move || run(db_ref, family, "INSERT INTO a_child VALUES (2,1)"));
                    while db.storage.debug_table_lock_waiters("a_child").1 == 0 {
                        assert!(
                            !child.is_finished() && Instant::now() < deadline,
                            "new child waits behind the parent's exclusive request"
                        );
                        std::thread::yield_now();
                    }
                    db.commit_transaction_for_session(holder).unwrap();
                    (parent.join().unwrap(), child.join().unwrap())
                });
                if action == "RESTRICT" {
                    fk_error(&parent.unwrap_err());
                    child.unwrap();
                    assert_eq!(rows(&db, "z_parent"), vec![vec![Value::Int4(1)]]);
                } else {
                    parent.unwrap();
                    fk_error(&child.unwrap_err());
                }
                assert!(db.query("SELECT c.id FROM a_child c LEFT JOIN z_parent p ON c.pid = p.id WHERE c.pid IS NOT NULL AND p.id IS NULL", &[]).unwrap().is_empty(), "{family:?} delete={delete} action={action}: no orphan");
            }
        }
    }
}
#[test]
fn parent_delete_actions_see_children_staged_earlier_in_the_same_transaction() {
    for family in FAMILIES {
        for action in ["CASCADE", "SET NULL"] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
            db.execute("INSERT INTO p VALUES (1)").unwrap();
            db.execute(&format!(
                "CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE {action})"
            ))
            .unwrap();
            run(&db, family, "BEGIN").unwrap();
            run(&db, family, "INSERT INTO c VALUES (1,1)").unwrap();
            run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
            run(&db, family, "COMMIT").unwrap();
            assert_eq!(
                rows(&db, "c"),
                if action == "CASCADE" {
                    vec![]
                } else {
                    vec![vec![Value::Int4(1), Value::Null]]
                }
            );
        }
    }
}
#[test]
fn branch_parent_actions_change_only_the_bound_branch() {
    for family in FAMILIES {
        let db = setup("CASCADE", "");
        let main_p = rows(&db, "p");
        let main_c = rows(&db, "c");
        db.execute("CREATE DATABASE BRANCH fk_work").unwrap();
        db.execute("USE BRANCH fk_work").unwrap();
        update(&db, family, 1, 10).unwrap();
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(10));
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(rows(&db, "p"), main_p);
        assert_eq!(rows(&db, "c"), main_c);
    }
}

#[test]
fn parent_and_cascaded_rows_share_their_primary_write_group_and_logical_wal() {
    use heliosdb_nano::{
        storage::{WalEntry, WalOperation},
        Config,
    };
    use rocksdb::WriteBatchIterator;
    #[derive(Default, Debug)]
    struct Group {
        keys: Vec<String>,
        updates: Vec<String>,
    }
    impl WriteBatchIterator for Group {
        fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
            if key.starts_with(b"wal:entries:") {
                if let WalOperation::Update { table, .. } = WalEntry::deserialize(&value).unwrap().operation {
                    self.updates.push(table);
                }
            }
            self.keys.push(String::from_utf8_lossy(&key).into_owned());
        }
        fn delete(&mut self, key: Box<[u8]>) {
            self.keys.push(String::from_utf8_lossy(&key).into_owned());
        }
    }
    for family in FAMILIES {
        for strict in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = Config::default();
            config.storage.path = Some(dir.path().to_path_buf());
            config.storage.memory_only = false;
            config.storage.wal_enabled = true;
            config.storage.logical_wal_per_statement = strict;
            config.storage.wal_checkpoint_interval_entries = 0;
            config.storage.wal_checkpoint_interval_secs = 0;
            config.replication.physical.enabled = true;
            let db = EmbeddedDatabase::with_config(config).unwrap();
            db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
            db.execute("INSERT INTO p VALUES (1)").unwrap();
            db.execute("CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON UPDATE CASCADE)")
                .unwrap();
            db.execute("INSERT INTO c VALUES (1,1), (2,1)").unwrap();
            let raw = db.storage.db();
            let before = raw.latest_sequence_number();
            update(&db, family, 1, 10).unwrap();
            let end = raw.latest_sequence_number();
            let mut next = before + 1;
            let mut groups = Vec::new();
            let mut iter = raw.get_updates_since(before).unwrap();
            for item in iter.by_ref() {
                let (first, batch) = item.unwrap();
                if first > end {
                    break;
                }
                assert_eq!(first, next);
                next = first + batch.len() as u64;
                let mut group = Group::default();
                batch.iterate(&mut group);
                if group.keys.iter().any(|key| {
                    !["snapshot:", "scn_map:@", "idxsnap", "vecsnap"]
                        .iter()
                        .any(|prefix| key.starts_with(prefix))
                }) {
                    groups.push(group);
                }
            }
            iter.status().unwrap();
            assert_eq!(next, end + 1);
            assert_eq!(groups.len(), 1, "{family:?} strict={strict}: {groups:#?}");
            assert!(groups[0].keys.iter().any(|key| key.starts_with("data:p:")));
            assert!(groups[0].keys.iter().any(|key| key.starts_with("data:c:")));
            assert_eq!(
                groups[0].updates.iter().filter(|table| table.as_str() == "p").count(),
                1,
                "{groups:#?}"
            );
            assert_eq!(
                groups[0].updates.iter().filter(|table| table.as_str() == "c").count(),
                2,
                "{groups:#?}"
            );
        }
    }
}

#[test]
fn deferred_obligations_ignore_legacy_orphans_and_follow_only_current_touched_rows() {
    for family in FAMILIES {
        for seed_mode in ["off", "audit"] {
            let db = setup("NO ACTION", "DEFERRABLE INITIALLY DEFERRED");
            db.execute(&format!("SET helios.fk_validation = '{seed_mode}'"))
                .unwrap();
            db.execute("INSERT INTO c VALUES (90,404), (91,999)").unwrap();
            db.execute("SET helios.fk_validation = 'enforced'").unwrap();
            run(&db, family, "BEGIN").unwrap();
            update(&db, family, 1, 10).unwrap();
            run(&db, family, "UPDATE c SET pid = 10 WHERE pid = 1").unwrap();
            run(&db, family, "COMMIT").unwrap();
            assert_eq!(
                db.query("SELECT pid FROM c WHERE id = 90", &[]).unwrap()[0].values,
                vec![Value::Int4(404)]
            );

            // The removed row shares its invalid key with a legacy orphan.
            // A stale value probe or whole-FK validator would reject this.
            run(&db, family, "BEGIN").unwrap();
            run(&db, family, "INSERT INTO c VALUES (20,999)").unwrap();
            run(&db, family, "DELETE FROM c WHERE id = 20").unwrap();
            run(&db, family, "INSERT INTO c VALUES (21,998)").unwrap();
            run(&db, family, "UPDATE c SET pid = 10 WHERE id = 21").unwrap();
            run(&db, family, "SAVEPOINT s").unwrap();
            run(&db, family, "INSERT INTO c VALUES (22,997)").unwrap();
            run(&db, family, "ROLLBACK TO SAVEPOINT s").unwrap();
            run(&db, family, "COMMIT").unwrap();
            assert!(db
                .query("SELECT id FROM c WHERE id = 20 OR id = 22", &[])
                .unwrap()
                .is_empty());
            assert_eq!(
                db.query("SELECT pid FROM c WHERE id = 21", &[]).unwrap()[0].values,
                vec![Value::Int4(10)]
            );

            // Touching an old orphan does create an obligation for that row.
            run(&db, family, "BEGIN").unwrap();
            run(&db, family, "UPDATE c SET pid = pid WHERE id = 90").unwrap();
            fk_error(&run(&db, family, "COMMIT").unwrap_err());
            assert_eq!(
                db.query("SELECT pid FROM c WHERE id = 90", &[]).unwrap()[0].values,
                vec![Value::Int4(404)]
            );
        }
    }
}

#[test]
fn deferred_insert_select_tracks_assigned_row_ids_and_repaired_or_deleted_rows() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY DEFERRED");
        db.execute("CREATE TABLE seed (id INT PRIMARY KEY, pid INT)").unwrap();
        db.execute("INSERT INTO seed VALUES (20,999), (21,998)").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "INSERT INTO c SELECT id, pid FROM seed").unwrap();
        run(&db, family, "DELETE FROM c WHERE id = 20").unwrap();
        run(&db, family, "UPDATE c SET pid = 1 WHERE id = 21").unwrap();
        run(&db, family, "COMMIT").unwrap();
        assert_eq!(
            db.query("SELECT pid FROM c WHERE id = 21", &[]).unwrap()[0].values,
            vec![Value::Int4(1)]
        );
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "INSERT INTO c SELECT id, pid FROM seed WHERE id = 20").unwrap();
        fk_error(&run(&db, family, "COMMIT").unwrap_err());
        assert!(db.query("SELECT id FROM c WHERE id = 20", &[]).unwrap().is_empty());
    }
}

#[test]
fn parent_refusal_checks_preserve_audit_and_lock_free_nonfatal_policy() {
    use heliosdb_nano::sql::ConstraintEnforcement;
    for family in FAMILIES {
        for action in ["NO ACTION", "RESTRICT"] {
            for timing in ["", "DEFERRABLE INITIALLY DEFERRED"] {
                for lock_free in [false, true] {
                    let db = setup(action, timing);
                    if lock_free {
                        let catalog = db.storage.catalog();
                        let mut constraints = catalog.load_table_constraints("c").unwrap();
                        constraints.foreign_keys[0].enforcement = ConstraintEnforcement::LockFree;
                        catalog.save_table_constraints("c", &constraints).unwrap();
                    } else {
                        db.execute("SET helios.fk_validation = 'audit'").unwrap();
                    }
                    update(&db, family, 1, 10).unwrap();
                    assert_eq!(db.query("SELECT id FROM p WHERE id = 10", &[]).unwrap().len(), 1);
                    assert_eq!(
                        rows(&db, "c")[0][1],
                        Value::Int4(1),
                        "refusal policy logs instead of changing child"
                    );
                    let log = db
                        .query("SELECT constraint_name, referenced_values FROM pg_log_violations", &[])
                        .unwrap();
                    assert_eq!(log.len(), 1, "{family:?} {action} {timing} lock_free={lock_free}");
                    assert_eq!(
                        log[0].values,
                        vec![Value::String("c_fk".into()), Value::String("1".into())]
                    );
                }
            }
        }
    }
}

#[test]
fn named_constraint_timing_and_immediate_validation_do_not_affect_other_names() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY IMMEDIATE");
        db.execute("CREATE TABLE p2 (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p2 VALUES (1)").unwrap();
        db.execute("CREATE TABLE c2 (id INT PRIMARY KEY, pid INT, CONSTRAINT other_fk FOREIGN KEY (pid) REFERENCES p2(id) DEFERRABLE INITIALLY IMMEDIATE)").unwrap();
        db.execute("INSERT INTO c2 VALUES (1,1)").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk DEFERRED").unwrap();
        update(&db, family, 1, 10).unwrap();
        fk_error(&run(&db, family, "UPDATE p2 SET id = 10 WHERE id = 1").unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
        assert_eq!(rows(&db, "p2"), vec![vec![Value::Int4(1)]]);

        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SET CONSTRAINTS ALL DEFERRED").unwrap();
        update(&db, family, 1, 10).unwrap();
        run(&db, family, "UPDATE p2 SET id = 10 WHERE id = 1").unwrap();
        run(&db, family, "UPDATE c SET pid = 10 WHERE pid = 1").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk IMMEDIATE").unwrap();
        run(&db, family, "UPDATE c2 SET pid = 10 WHERE pid = 1").unwrap();
        run(&db, family, "COMMIT").unwrap();
        assert_eq!(rows(&db, "c2"), vec![vec![Value::Int4(1), Value::Int4(10)]]);

        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk DEFERRED").unwrap();
        run(&db, family, "SAVEPOINT s").unwrap();
        run(&db, family, "SET CONSTRAINTS other_fk DEFERRED").unwrap();
        run(&db, family, "ROLLBACK TO SAVEPOINT s").unwrap();
        fk_error(&run(&db, family, "UPDATE p2 SET id = 20 WHERE id = 10").unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk DEFERRED").unwrap();
        run(&db, family, "SET CONSTRAINTS ALL IMMEDIATE").unwrap();
        fk_error(&update(&db, family, 10, 20).unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
    }
}

#[test]
fn named_constraint_selection_refuses_unknown_qualified_and_nondeferrable_names_explicitly() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "");
        for (statement, marker) in [
            (
                "SET CONSTRAINTS missing_fk DEFERRED",
                "does not identify a supported foreign key",
            ),
            (
                "SET CONSTRAINTS public.c_fk DEFERRED",
                "unqualified foreign-key names only",
            ),
            ("SET CONSTRAINTS c_fk DEFERRED", "is not deferrable"),
        ] {
            let error = run(&db, family, statement).unwrap_err();
            assert!(error.to_string().contains(marker), "{error}");
        }
        fk_error(&update(&db, family, 1, 10).unwrap_err());
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(1));
    }
}
