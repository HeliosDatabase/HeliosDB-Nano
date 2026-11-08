//! Recursive DELETE actions share UPDATE's staged action graph and constraints.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::{EmbeddedDatabase, Error, Value};
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
fn rows(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    db.query(&format!("SELECT * FROM {table} ORDER BY id"), &[])
        .unwrap()
        .into_iter()
        .map(|row| row.values)
        .collect()
}
fn fk_error(error: Error) {
    assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
    assert!(
        error.to_string().to_ascii_lowercase().contains("foreign key"),
        "{error}"
    );
}
fn setup(action: &str, timing: &str) -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO p VALUES (0),(1),(2)").unwrap();
    db.execute(&format!("CREATE TABLE c (id INT PRIMARY KEY, pid INT DEFAULT 0, CONSTRAINT c_fk FOREIGN KEY(pid) REFERENCES p(id) ON DELETE {action} {timing})")).unwrap();
    db.execute("INSERT INTO c VALUES (10,1),(11,1),(12,2),(13,NULL)")
        .unwrap();
    db
}
#[test]
fn all_delete_actions_preserve_unrelated_children_or_refuse_atomically() {
    for family in FAMILIES {
        for action in ["NO ACTION", "RESTRICT", "CASCADE", "SET NULL", "SET DEFAULT"] {
            let db = setup(action, "");
            let before_p = rows(&db, "p");
            let before_c = rows(&db, "c");
            let result = run(&db, family, "DELETE FROM p WHERE id = 1");
            if matches!(action, "NO ACTION" | "RESTRICT") {
                fk_error(result.unwrap_err());
                assert_eq!(rows(&db, "p"), before_p);
                assert_eq!(rows(&db, "c"), before_c);
            } else {
                assert_eq!(result.unwrap(), 1);
                assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(0)], vec![Value::Int4(2)]]);
                let mut expected = vec![
                    vec![Value::Int4(12), Value::Int4(2)],
                    vec![Value::Int4(13), Value::Null],
                ];
                if action != "CASCADE" {
                    let val = if action == "SET NULL" {
                        Value::Null
                    } else {
                        Value::Int4(0)
                    };
                    expected.insert(0, vec![Value::Int4(11), val.clone()]);
                    expected.insert(0, vec![Value::Int4(10), val]);
                }
                assert_eq!(rows(&db, "c"), expected, "{family:?} {action}");
            }
        }
    }
}
#[test]
fn cascade_reaches_grandchildren_and_rolls_back_every_level_and_index_key() {
    for family in FAMILIES {
        let db = setup("CASCADE", "");
        db.execute("CREATE TABLE g (id INT PRIMARY KEY, cid INT REFERENCES c(id) ON DELETE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO g VALUES (1,10),(2,11),(3,12)").unwrap();
        let before = (rows(&db, "p"), rows(&db, "c"), rows(&db, "g"));
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SAVEPOINT s").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        assert_eq!(rows(&db, "g"), vec![vec![Value::Int4(3), Value::Int4(12)]]);
        run(&db, family, "ROLLBACK TO SAVEPOINT s").unwrap();
        assert_eq!((rows(&db, "p"), rows(&db, "c"), rows(&db, "g")), before);
        run(&db, family, "COMMIT").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        run(&db, family, "ROLLBACK").unwrap();
        assert_eq!((rows(&db, "p"), rows(&db, "c"), rows(&db, "g")), before);
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        assert!(db.query("SELECT id FROM c WHERE id = 10", &[]).unwrap().is_empty());
        assert!(db.query("SELECT id FROM g WHERE id = 1", &[]).unwrap().is_empty());
        run(&db, family, "INSERT INTO p VALUES (1)").unwrap();
        run(&db, family, "INSERT INTO c VALUES (10,1)").unwrap();
        run(&db, family, "INSERT INTO g VALUES (1,10)").unwrap();
        assert_eq!(db.query("SELECT id FROM g WHERE id = 1", &[]).unwrap().len(), 1);
    }
}
#[test]
fn grandchild_restrict_refuses_entire_cascade_without_losing_child_indexes() {
    for family in FAMILIES {
        let db = setup("CASCADE", "");
        db.execute("CREATE TABLE g (id INT PRIMARY KEY, cid INT REFERENCES c(id) ON DELETE RESTRICT)")
            .unwrap();
        db.execute("INSERT INTO g VALUES (1,10)").unwrap();
        let before = (rows(&db, "p"), rows(&db, "c"), rows(&db, "g"));
        fk_error(run(&db, family, "DELETE FROM p WHERE id = 1").unwrap_err());
        assert_eq!((rows(&db, "p"), rows(&db, "c"), rows(&db, "g")), before);
        assert_eq!(db.query("SELECT id FROM c WHERE id = 10", &[]).unwrap().len(), 1);
    }
}
#[test]
fn null_and_default_actions_validate_all_child_constraints_atomically() {
    for family in FAMILIES {
        for (action, definition, marker) in [
            ("SET NULL", "pid INT NOT NULL", "NOT NULL"),
            ("SET DEFAULT", "pid INT DEFAULT 999", "Foreign key"),
            ("SET DEFAULT", "pid INT DEFAULT 0 CHECK (pid > 0)", "CHECK"),
        ] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
            db.execute("INSERT INTO p VALUES (0),(1)").unwrap();
            db.execute(&format!("CREATE TABLE c (id INT PRIMARY KEY, {definition}, FOREIGN KEY(pid) REFERENCES p(id) ON DELETE {action})")).unwrap();
            db.execute("INSERT INTO c VALUES (1,1)").unwrap();
            let error = run(&db, family, "DELETE FROM p WHERE id = 1").unwrap_err();
            assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
            assert!(error.to_string().contains(marker), "{error}");
            assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(0)], vec![Value::Int4(1)]]);
            assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(1)]]);
        }
    }
}
#[test]
fn generated_default_key_changes_cascade_to_grandchildren_and_maintain_indexes() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p VALUES (0),(1)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY DEFAULT 0 REFERENCES p(id) ON DELETE SET DEFAULT)")
            .unwrap();
        db.execute("INSERT INTO c VALUES (1)").unwrap();
        db.execute("CREATE TABLE g (id INT PRIMARY KEY, cid INT REFERENCES c(id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO g VALUES (1,1)").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(0)]]);
        assert_eq!(rows(&db, "g"), vec![vec![Value::Int4(1), Value::Int4(0)]]);
        assert!(db.query("SELECT id FROM c WHERE id = 1", &[]).unwrap().is_empty());
        assert_eq!(db.query("SELECT id FROM c WHERE id = 0", &[]).unwrap().len(), 1);
    }
}
#[test]
fn null_actions_remove_secondary_index_keys_and_rollback_restores_them() {
    for family in FAMILIES {
        let db = setup("SET NULL", "");
        db.execute("CREATE INDEX c_pid ON c(pid)").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        assert!(db.query("SELECT id FROM c WHERE pid = 1", &[]).unwrap().is_empty());
        run(&db, family, "ROLLBACK").unwrap();
        assert_eq!(db.query("SELECT id FROM c WHERE pid = 1", &[]).unwrap().len(), 2);
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        assert!(db.query("SELECT id FROM c WHERE pid = 1", &[]).unwrap().is_empty());
        assert_eq!(db.query("SELECT id FROM c WHERE pid IS NULL", &[]).unwrap().len(), 3);
    }
}
#[test]
fn deferred_delete_no_action_supports_repair_and_savepoint_but_restrict_is_immediate() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY DEFERRED");
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        run(&db, family, "UPDATE c SET pid = 2 WHERE pid = 1").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk IMMEDIATE").unwrap();
        run(&db, family, "COMMIT").unwrap();
        assert_eq!(rows(&db, "c")[0][1], Value::Int4(2));
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SAVEPOINT s").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 2").unwrap();
        run(&db, family, "ROLLBACK TO SAVEPOINT s").unwrap();
        run(&db, family, "COMMIT").unwrap();
        assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(0)], vec![Value::Int4(2)]]);
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 2").unwrap();
        fk_error(run(&db, family, "COMMIT").unwrap_err());
        assert_eq!(db.query("SELECT id FROM p WHERE id = 2", &[]).unwrap().len(), 1);
        let db = setup("RESTRICT", "DEFERRABLE INITIALLY DEFERRED");
        run(&db, family, "BEGIN").unwrap();
        fk_error(run(&db, family, "DELETE FROM p WHERE id = 1").unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
    }
}
#[test]
fn composite_match_simple_and_cross_type_delete_actions_are_exact() {
    for family in FAMILIES {
        for action in ["CASCADE", "SET NULL", "SET DEFAULT"] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE p (a BIGINT,b BIGINT,PRIMARY KEY(a,b))")
                .unwrap();
            db.execute("INSERT INTO p VALUES (0,0),(1,2)").unwrap();
            db.execute(&format!("CREATE TABLE c (id INT PRIMARY KEY,x INT DEFAULT 0,y INT DEFAULT 0,FOREIGN KEY(y,x) REFERENCES p(b,a) ON DELETE {action})")).unwrap();
            db.execute("INSERT INTO c VALUES (1,1,2),(2,NULL,2)").unwrap();
            run(&db, family, "DELETE FROM p WHERE a = 1 AND b = 2").unwrap();
            let mut expected = vec![vec![Value::Int4(2), Value::Null, Value::Int4(2)]];
            if action != "CASCADE" {
                let value = if action == "SET NULL" {
                    Value::Null
                } else {
                    Value::Int4(0)
                };
                expected.insert(0, vec![Value::Int4(1), value.clone(), value]);
            }
            assert_eq!(rows(&db, "c"), expected);
        }
    }
}
#[test]
fn self_and_cyclic_cascade_delete_terminate_and_delete_all_reachable_rows() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE n (id INT PRIMARY KEY,pid INT REFERENCES n(id) ON DELETE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO n VALUES (1,NULL),(2,1),(3,2)").unwrap();
        db.execute("UPDATE n SET pid = 1 WHERE id = 1").unwrap();
        run(&db, family, "DELETE FROM n WHERE id = 1").unwrap();
        assert!(rows(&db, "n").is_empty());
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE a (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO a VALUES (1)").unwrap();
        db.execute("CREATE TABLE b (id INT PRIMARY KEY REFERENCES a(id) ON DELETE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO b VALUES (1)").unwrap();
        db.execute("ALTER TABLE a ADD CONSTRAINT a_fk FOREIGN KEY(id) REFERENCES b(id) ON DELETE CASCADE")
            .unwrap();
        run(&db, family, "DELETE FROM a WHERE id = 1").unwrap();
        assert!(rows(&db, "a").is_empty());
        assert!(rows(&db, "b").is_empty());
    }
}
#[test]
fn recursive_delete_actions_are_branch_local_and_cover_own_staged_children() {
    for family in FAMILIES {
        let db = setup("CASCADE", "");
        db.execute("CREATE TABLE g (id INT PRIMARY KEY,cid INT REFERENCES c(id) ON DELETE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO g VALUES (1,10)").unwrap();
        let main = (rows(&db, "p"), rows(&db, "c"), rows(&db, "g"));
        db.execute("CREATE DATABASE BRANCH work").unwrap();
        db.execute("USE BRANCH work").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "INSERT INTO c VALUES (20,1)").unwrap();
        run(&db, family, "INSERT INTO g VALUES (2,20)").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        run(&db, family, "COMMIT").unwrap();
        assert!(rows(&db, "g").is_empty());
        assert_eq!(
            rows(&db, "c"),
            vec![
                vec![Value::Int4(12), Value::Int4(2)],
                vec![Value::Int4(13), Value::Null]
            ]
        );
        db.execute("USE BRANCH main").unwrap();
        assert_eq!((rows(&db, "p"), rows(&db, "c"), rows(&db, "g")), main);
    }
}
#[test]
fn root_explicit_self_fk_edit_is_preserved_by_update_and_upsert_actions() {
    for family in FAMILIES {
        for upsert in [false, true] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE n (id INT PRIMARY KEY,pid INT REFERENCES n(id) ON UPDATE CASCADE)")
                .unwrap();
            db.execute("INSERT INTO n VALUES (1,NULL),(99,NULL)").unwrap();
            db.execute("UPDATE n SET pid = 1 WHERE id = 1").unwrap();
            run(
                &db,
                family,
                if upsert {
                    "INSERT INTO n VALUES (1,1) ON CONFLICT(id) DO UPDATE SET id = 10, pid = 99"
                } else {
                    "UPDATE n SET id = 10, pid = 99 WHERE id = 1"
                },
            )
            .unwrap();
            assert_eq!(
                rows(&db, "n"),
                vec![
                    vec![Value::Int4(10), Value::Int4(99)],
                    vec![Value::Int4(99), Value::Null]
                ]
            );
        }
    }
}

#[test]
fn each_validation_invocation_reads_fresh_parent_keys_after_set_immediate() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY DEFERRED");
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "INSERT INTO c VALUES (20,1)").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk IMMEDIATE").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk DEFERRED").unwrap();
        run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
        fk_error(run(&db, family, "SET CONSTRAINTS c_fk IMMEDIATE").unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
        assert_eq!(db.query("SELECT id FROM p WHERE id = 1", &[]).unwrap().len(), 1);
        assert!(db.query("SELECT id FROM c WHERE id = 20", &[]).unwrap().is_empty());
    }
}
#[test]
fn named_public_fk_timing_does_not_change_same_named_fk_in_other_schema() {
    for family in FAMILIES {
        let db = setup("NO ACTION", "DEFERRABLE INITIALLY IMMEDIATE");
        db.execute("CREATE SCHEMA s").unwrap();
        db.execute("CREATE TABLE s.p (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO s.p VALUES (1)").unwrap();
        db.execute("CREATE TABLE s.c (id INT PRIMARY KEY,pid INT,CONSTRAINT c_fk FOREIGN KEY(pid) REFERENCES s.p(id) DEFERRABLE INITIALLY IMMEDIATE)").unwrap();
        db.execute("INSERT INTO s.c VALUES (1,1)").unwrap();
        run(&db, family, "BEGIN").unwrap();
        run(&db, family, "SET CONSTRAINTS c_fk DEFERRED").unwrap();
        run(&db, family, "UPDATE public.p SET id = 10 WHERE id = 1").unwrap();
        fk_error(run(&db, family, "UPDATE s.p SET id = 10 WHERE id = 1").unwrap_err());
        run(&db, family, "ROLLBACK").unwrap();
        use heliosdb_nano::session::IsolationLevel;
        let session = db.create_session("schema", IsolationLevel::ReadCommitted).unwrap();
        db.execute_for_session(session, "SET search_path TO s").unwrap();
        db.begin_transaction_for_session(session).unwrap();
        let error = match family {
            Family::Text => db.execute_for_session(session, "SET CONSTRAINTS c_fk DEFERRED"),
            Family::Params => db.execute_params_for_session(session, "SET CONSTRAINTS c_fk DEFERRED", &[]),
        }
        .unwrap_err();
        assert!(error.to_string().contains("default public search_path only"), "{error}");
        db.rollback_transaction_for_session(session).unwrap();
        // A cold public session must consult its own schema, even while the
        // embedded connection has a different search path.
        db.execute("SET search_path TO s").unwrap();
        let public = db.create_session("public", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(public).unwrap();
        match family {
            Family::Text => db.execute_for_session(public, "SET CONSTRAINTS c_fk DEFERRED"),
            Family::Params => db.execute_params_for_session(public, "SET CONSTRAINTS c_fk DEFERRED", &[]),
        }
        .unwrap();
        db.execute_for_session(public, "UPDATE public.p SET id = 10 WHERE id = 1")
            .unwrap();
        db.execute_for_session(public, "UPDATE public.c SET pid = 10 WHERE pid = 1")
            .unwrap();
        db.commit_transaction_for_session(public).unwrap();
        assert_eq!(
            db.query("SELECT pid FROM public.c WHERE id = 10", &[]).unwrap()[0].values,
            vec![Value::Int4(10)]
        );
        assert_eq!(
            db.query("SELECT pid FROM s.c WHERE id = 1", &[]).unwrap()[0].values,
            vec![Value::Int4(1)]
        );
    }
}
#[test]
fn recursive_delete_rows_and_wal_share_one_primary_group_including_columnar_presence() {
    use heliosdb_nano::{
        storage::{WalEntry, WalOperation},
        Config,
    };
    use rocksdb::WriteBatchIterator;
    #[derive(Default, Debug)]
    struct Group {
        keys: Vec<String>,
        deletes: Vec<String>,
    }
    impl WriteBatchIterator for Group {
        fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
            if key.starts_with(b"wal:entries:") {
                if let WalOperation::Delete { table, .. } = WalEntry::deserialize(&value).unwrap().operation {
                    self.deletes.push(table);
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
            db.execute("CREATE TABLE c (id INT PRIMARY KEY,pid INT REFERENCES p(id) ON DELETE CASCADE)")
                .unwrap();
            db.execute("INSERT INTO c VALUES (1,1),(2,1)").unwrap();
            db.execute("CREATE TABLE g (id INT PRIMARY KEY,cid INT REFERENCES c(id) ON DELETE CASCADE)")
                .unwrap();
            db.execute("INSERT INTO g VALUES (1,1),(2,2)").unwrap();
            db.execute("ALTER TABLE c ALTER COLUMN pid SET STORAGE COLUMNAR")
                .unwrap();
            db.execute("ALTER TABLE g ALTER COLUMN cid SET STORAGE COLUMNAR")
                .unwrap();
            let raw = db.storage.db();
            let before = raw.latest_sequence_number();
            run(&db, family, "DELETE FROM p WHERE id = 1").unwrap();
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
            assert_eq!(
                groups[0].deletes.iter().filter(|table| table.as_str() == "p").count(),
                1
            );
            assert_eq!(
                groups[0].deletes.iter().filter(|table| table.as_str() == "c").count(),
                2
            );
            assert_eq!(
                groups[0].deletes.iter().filter(|table| table.as_str() == "g").count(),
                2
            );
            // Both child tables are empty. A complete empty manifest needs no
            // colp row; its colpm marker is part of this same delete/WAL group.
            for table in ["c", "g"] {
                assert!(groups[0].keys.iter().any(|key| key == &format!("colpm:{table}")));
                assert!(heliosdb_nano::storage::ColumnarStore::presence_manifest_complete(
                    &raw, table
                ));
                assert!(heliosdb_nano::storage::ColumnarStore::load_presence_map(&raw, table)
                    .unwrap()
                    .is_empty());
            }
            assert!(rows(&db, "c").is_empty());
            assert!(rows(&db, "g").is_empty());
            drop(iter);
            drop(raw);
            drop(db);
            let reopened = EmbeddedDatabase::new(dir.path()).unwrap();
            assert!(rows(&reopened, "p").is_empty());
            assert!(rows(&reopened, "c").is_empty());
            assert!(rows(&reopened, "g").is_empty());
        }
    }
}

#[test]
fn quoted_colon_constraint_names_keep_child_parent_and_selected_timing_scopes() {
    for family in FAMILIES {
        for parent_change in [false, true] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            db.execute("CREATE TABLE pa (id INT PRIMARY KEY)").unwrap();
            db.execute("CREATE TABLE pb (id INT PRIMARY KEY)").unwrap();
            db.execute("INSERT INTO pa VALUES (1)").unwrap();
            db.execute("INSERT INTO pb VALUES (1)").unwrap();
            db.execute("CREATE TABLE ca (id INT PRIMARY KEY,pid INT,CONSTRAINT \"a:b\" FOREIGN KEY(pid) REFERENCES pa(id) DEFERRABLE INITIALLY DEFERRED)").unwrap();
            db.execute("CREATE TABLE cb (id INT PRIMARY KEY,pid INT,CONSTRAINT \"b:c\" FOREIGN KEY(pid) REFERENCES pb(id) DEFERRABLE INITIALLY DEFERRED)").unwrap();
            if parent_change {
                db.execute("INSERT INTO ca VALUES (1,1)").unwrap();
                db.execute("INSERT INTO cb VALUES (1,1)").unwrap();
            }
            run(&db, family, "BEGIN").unwrap();
            if parent_change {
                // The repaired first FK and unrepaired second FK have independent
                // parent old-key obligations, despite both names containing a colon.
                run(&db, family, "UPDATE pa SET id = 10 WHERE id = 1").unwrap();
                run(&db, family, "UPDATE ca SET pid = 10 WHERE id = 1").unwrap();
                run(&db, family, "UPDATE pb SET id = 10 WHERE id = 1").unwrap();
            } else {
                // The first child obligation is repaired; the second constraint
                // must remain deferred and then fail independently at COMMIT.
                run(&db, family, "INSERT INTO ca VALUES (1,404)").unwrap();
                run(&db, family, "INSERT INTO cb VALUES (1,405)").unwrap();
                run(&db, family, "INSERT INTO pa VALUES (404)").unwrap();
            }
            // Selecting only the repaired quoted FK must not run the other
            // pending colon-named FK. COMMIT still checks that second obligation.
            run(&db, family, "SET CONSTRAINTS \"a:b\" IMMEDIATE").unwrap();
            let error = run(&db, family, "COMMIT").unwrap_err();
            assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
            assert!(
                error.to_string().contains("b:c"),
                "second distinct FK must be validated: {error}"
            );
            assert_eq!(rows(&db, "pa"), vec![vec![Value::Int4(1)]]);
            assert_eq!(rows(&db, "pb"), vec![vec![Value::Int4(1)]]);
            let expected = if parent_change {
                vec![vec![Value::Int4(1), Value::Int4(1)]]
            } else {
                Vec::new()
            };
            assert_eq!(
                db.query("SELECT id,pid FROM ca", &[])
                    .unwrap()
                    .into_iter()
                    .map(|row| row.values)
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(rows(&db, "cb"), expected);
        }
    }
}
