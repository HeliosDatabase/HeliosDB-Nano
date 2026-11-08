//! MERGE checks its final target image before intent creation and preserves policy on resume.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
use heliosdb_nano::storage::branch_catalog::MergeFailSite;
use heliosdb_nano::{EmbeddedDatabase, Error, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
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
fn rows(db: &EmbeddedDatabase, table: &str) -> Vec<Vec<Value>> {
    db.query(&format!("SELECT * FROM {table} ORDER BY id"), &[])
        .unwrap()
        .into_iter()
        .map(|row| row.values)
        .collect()
}
fn fk_error(error: Error) {
    assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
    assert!(error.to_string().contains("Foreign key"), "{error}");
}
fn seed(db: &EmbeddedDatabase, family: Family) {
    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO p VALUES (1)").unwrap();
    db.execute("CREATE TABLE c (id INT PRIMARY KEY,pid INT,CONSTRAINT c_fk FOREIGN KEY(pid) REFERENCES p(id) ON UPDATE CASCADE)").unwrap();
    db.execute("INSERT INTO c VALUES (1,1)").unwrap();
    db.execute("CREATE DATABASE BRANCH work").unwrap();
    db.execute("USE BRANCH work").unwrap();
    run(db, family, "UPDATE p SET id = 10 WHERE id = 1").unwrap();
    db.execute("USE BRANCH main").unwrap();
}
#[test]
fn merge_rejects_unseen_target_children_before_any_mutation_and_branch_can_retry() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db, family);
        db.execute("INSERT INTO c VALUES (2,1)").unwrap();
        let before = (rows(&db, "p"), rows(&db, "c"));
        let sequence = db.storage.db().latest_sequence_number();
        fk_error(run(&db, family, "MERGE BRANCH work INTO main").unwrap_err());
        assert_eq!(
            db.storage.db().latest_sequence_number(),
            sequence,
            "strict refusal precedes any intent/catalog/data/WAL write"
        );
        assert_eq!((rows(&db, "p"), rows(&db, "c")), before);
        let branch = db.storage.get_branch("work").unwrap();
        assert!(matches!(branch.state, heliosdb_nano::storage::BranchState::Active));
        assert!(db
            .storage
            .get(&format!("branch:merge:{}", branch.branch_id).into_bytes())
            .unwrap()
            .is_none());
        assert!(db
            .storage
            .get(&format!("branch:merge_fk_policy:{}", branch.branch_id).into_bytes())
            .unwrap()
            .is_none());
        db.execute("USE BRANCH work").unwrap();
        assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(10)]]);
        assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(10)]]);
        db.execute("USE BRANCH main").unwrap();
        db.execute("DELETE FROM c WHERE id = 2").unwrap();
        run(&db, family, "MERGE BRANCH work INTO main").unwrap();
        assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(10)]]);
        assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(10)]]);
    }
}
#[test]
fn merge_validates_new_child_table_unknown_to_source_and_legacy_orphans_are_not_revalidated() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        seed(&db, family);
        db.execute("CREATE TABLE new_child (id INT PRIMARY KEY,pid INT REFERENCES p(id))")
            .unwrap();
        db.execute("INSERT INTO new_child VALUES (1,1)").unwrap();
        fk_error(run(&db, family, "MERGE BRANCH work INTO main").unwrap_err());
        db.execute("DELETE FROM new_child").unwrap();
        db.execute("SET helios.fk_validation = 'off'").unwrap();
        db.execute("INSERT INTO c VALUES (99,999)").unwrap();
        db.execute("SET helios.fk_validation = 'enforced'").unwrap();
        run(&db, family, "MERGE BRANCH work INTO main").unwrap();
        assert_eq!(
            rows(&db, "c"),
            vec![
                vec![Value::Int4(1), Value::Int4(10)],
                vec![Value::Int4(99), Value::Int4(999)]
            ]
        );
    }
}
#[test]
fn branch_target_final_validation_preserves_both_branches_on_constraint_conflict() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY,pid INT REFERENCES p(id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO c VALUES (1,1)").unwrap();
        db.execute("CREATE DATABASE BRANCH target").unwrap();
        db.execute("CREATE DATABASE BRANCH source PARENT target").unwrap();
        db.execute("USE BRANCH source").unwrap();
        run(&db, family, "UPDATE p SET id = 10 WHERE id = 1").unwrap();
        db.execute("USE BRANCH target").unwrap();
        db.execute("INSERT INTO c VALUES (2,1)").unwrap();
        let before = (rows(&db, "p"), rows(&db, "c"));
        fk_error(run(&db, family, "MERGE BRANCH source INTO target").unwrap_err());
        assert_eq!((rows(&db, "p"), rows(&db, "c")), before);
        db.execute("DELETE FROM c WHERE id = 2").unwrap();
        run(&db, family, "MERGE BRANCH source INTO target").unwrap();
        assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(10)]]);
        assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(10)]]);
        db.execute("USE BRANCH main").unwrap();
        assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(1)]]);
        assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(1)]]);
    }
}
#[test]
fn changed_child_fk_is_checked_against_current_target_parent_and_match_simple_is_preserved() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (a BIGINT,b BIGINT,PRIMARY KEY(a,b))")
            .unwrap();
        db.execute("INSERT INTO p VALUES (1,2),(3,4)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY,x INT,y INT,FOREIGN KEY(y,x) REFERENCES p(b,a))")
            .unwrap();
        db.execute("CREATE DATABASE BRANCH work").unwrap();
        db.execute("USE BRANCH work").unwrap();
        run(&db, family, "INSERT INTO c VALUES (1,1,2),(2,NULL,2)").unwrap();
        db.execute("USE BRANCH main").unwrap();
        db.execute("DELETE FROM p WHERE a = 1 AND b = 2").unwrap();
        fk_error(run(&db, family, "MERGE BRANCH work INTO main").unwrap_err());
        assert!(rows(&db, "c").is_empty());
        db.execute("INSERT INTO p VALUES (1,2)").unwrap();
        run(&db, family, "MERGE BRANCH work INTO main").unwrap();
        assert_eq!(
            rows(&db, "c"),
            vec![
                vec![Value::Int4(1), Value::Int4(1), Value::Int4(2)],
                vec![Value::Int4(2), Value::Null, Value::Int4(2)]
            ]
        );
    }
}
#[test]
fn merge_policy_honours_off_proxy_audit_and_stored_non_enforcement() {
    use heliosdb_nano::sql::ConstraintEnforcement;
    for family in FAMILIES {
        for policy in ["off", "proxy", "audit", "not_enforced", "lock_free"] {
            let db = EmbeddedDatabase::new_in_memory().unwrap();
            seed(&db, family);
            db.execute("INSERT INTO c VALUES (2,1)").unwrap();
            match policy {
                "off" => {
                    db.execute("SET helios.fk_validation = 'off'").unwrap();
                }
                "proxy" => {
                    db.execute("SET helios.fk_validation_source = 'proxy'").unwrap();
                }
                "audit" => {
                    db.execute("SET helios.fk_validation = 'audit'").unwrap();
                }
                _ => {
                    // Change only target runtime enforcement after the fork. FK enforcement is
                    // catalog metadata; this definition change properly makes this old branch
                    // schema-incompatible, so seed a new branch with the selected declaration.
                    db.execute("DROP BRANCH work").unwrap();
                    let catalog = db.storage.catalog();
                    let mut constraints = catalog.load_table_constraints("c").unwrap();
                    constraints.foreign_keys[0].enforcement = if policy == "not_enforced" {
                        ConstraintEnforcement::NotEnforced
                    } else {
                        ConstraintEnforcement::LockFree
                    };
                    catalog.save_table_constraints("c", &constraints).unwrap();
                    db.execute("CREATE DATABASE BRANCH work").unwrap();
                    db.execute("USE BRANCH work").unwrap();
                    run(&db, family, "UPDATE p SET id = 10 WHERE id = 1").unwrap();
                    db.execute("USE BRANCH main").unwrap();
                    db.execute("INSERT INTO c VALUES (3,1)").unwrap();
                }
            }
            run(&db, family, "MERGE BRANCH work INTO main").unwrap();
            assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(10)]]);
            assert!(!db.query("SELECT id FROM c WHERE pid = 1", &[]).unwrap().is_empty());
            if matches!(policy, "audit" | "lock_free") {
                assert!(!db
                    .query(
                        "SELECT constraint_name FROM pg_log_violations WHERE constraint_name = 'c_fk'",
                        &[]
                    )
                    .unwrap()
                    .is_empty());
            }
        }
    }
}
#[test]
fn child_insert_waits_behind_merge_validation_of_inbound_table_not_changed_by_source() {
    use heliosdb_nano::session::IsolationLevel;
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.storage.debug_set_table_lock_timeout(Duration::from_secs(10));
        db.execute("CREATE TABLE z_parent (id INT PRIMARY KEY)").unwrap();
        db.execute("INSERT INTO z_parent VALUES (1)").unwrap();
        db.execute("CREATE DATABASE BRANCH work").unwrap();
        db.execute("USE BRANCH work").unwrap();
        run(&db, family, "UPDATE z_parent SET id = 10 WHERE id = 1").unwrap();
        db.execute("USE BRANCH main").unwrap();
        db.execute("CREATE TABLE a_child (id INT PRIMARY KEY,pid INT REFERENCES z_parent(id))")
            .unwrap();
        let holder = db.create_session("holder", IsolationLevel::ReadCommitted).unwrap();
        db.begin_transaction_for_session(holder).unwrap();
        db.execute_for_session(holder, "UPDATE a_child SET pid = pid WHERE id = -1")
            .unwrap();
        let (merged, child) = std::thread::scope(|scope| {
            let db_ref = &db;
            let merged = scope.spawn(move || run(db_ref, family, "MERGE BRANCH work INTO main"));
            let deadline = Instant::now() + Duration::from_secs(10);
            while db.storage.debug_table_lock_waiters("a_child").0 == 0 {
                assert!(
                    !merged.is_finished() && Instant::now() < deadline,
                    "merge must wait on newly discovered inbound table"
                );
                std::thread::yield_now();
            }
            let child = scope.spawn(move || run(db_ref, family, "INSERT INTO a_child VALUES (1,1)"));
            while db.storage.debug_table_lock_waiters("a_child").1 == 0 {
                assert!(!child.is_finished() && Instant::now() < deadline);
                std::thread::yield_now();
            }
            db.commit_transaction_for_session(holder).unwrap();
            (merged.join().unwrap(), child.join().unwrap())
        });
        merged.unwrap();
        fk_error(child.unwrap_err());
        assert!(rows(&db, "a_child").is_empty());
        assert_eq!(rows(&db, "z_parent"), vec![vec![Value::Int4(10)]]);
    }
}
#[test]
fn merge_intent_and_policy_share_one_write_group_and_cleanup_together() {
    use rocksdb::WriteBatchIterator;
    #[derive(Default)]
    struct Keys(Vec<String>);
    impl WriteBatchIterator for Keys {
        fn put(&mut self, key: Box<[u8]>, _: Box<[u8]>) {
            self.0.push(String::from_utf8_lossy(&key).into_owned());
        }
        fn delete(&mut self, key: Box<[u8]>) {
            self.0.push(String::from_utf8_lossy(&key).into_owned());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let db = EmbeddedDatabase::new(dir.path()).unwrap();
    seed(&db, Family::Text);
    db.execute("INSERT INTO c VALUES (2,1)").unwrap();
    db.execute("SET helios.fk_validation = 'off'").unwrap();
    let id = db.storage.get_branch("work").unwrap().branch_id;
    let intent = format!("branch:merge:{id}");
    let policy = format!("branch:merge_fk_policy:{id}");
    let before = db.storage.db().latest_sequence_number();
    db.storage.debug_fail_merge_at(MergeFailSite::AfterIntent, 0);
    let error = db.execute("MERGE BRANCH work INTO main").unwrap_err();
    assert!(matches!(&error, Error::QueryExecution(_)), "{error}");
    assert!(
        error.to_string().contains("stopped after it began changing")
            && error.to_string().contains("run it again to finish it"),
        "{error}"
    );
    assert_eq!(db.storage.get(&policy.as_bytes().to_vec()).unwrap(), Some(vec![1, 2]));
    let mut found = false;
    let mut iter = db.storage.db().get_updates_since(before).unwrap();
    for item in iter.by_ref() {
        let (_, batch) = item.unwrap();
        let mut keys = Keys::default();
        batch.iterate(&mut keys);
        if keys.0.contains(&intent) {
            assert!(keys.0.contains(&policy));
            found = true;
        }
    }
    iter.status().unwrap();
    assert!(found, "intent batch must also include its policy");
    db.execute("SET helios.fk_validation = 'enforced'").unwrap();
    db.execute("MERGE BRANCH work INTO main").unwrap();
    assert!(db.storage.get(&intent.as_bytes().to_vec()).unwrap().is_none());
    assert!(db.storage.get(&policy.as_bytes().to_vec()).unwrap().is_none());
    assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(10)]]);
}
#[test]
fn merge_crash_resume_uses_durable_policy_instead_of_new_handles_strict_default() {
    const EXACT: &str = "merge_crash_resume_uses_durable_policy_instead_of_new_handles_strict_default";
    const CHILD_DIR: &str = "HDB_FK_MERGE_CRASH_DIR";
    const CHILD_MODE: &str = "HDB_FK_MERGE_CRASH_MODE";
    const CHILD_SITE: &str = "HDB_FK_MERGE_CRASH_SITE";
    if let (Ok(dir), Ok(mode)) = (std::env::var(CHILD_DIR), std::env::var(CHILD_MODE)) {
        let db = EmbeddedDatabase::new(&dir).unwrap();
        seed(&db, Family::Text);
        db.execute("INSERT INTO c VALUES (2,1)").unwrap();
        db.execute(&format!("SET helios.fk_validation = '{mode}'")).unwrap();
        if std::env::var(CHILD_SITE).unwrap() == "partial_table" {
            let hits = AtomicUsize::new(0);
            let raw = db.storage.db();
            let proof = std::path::Path::new(&dir).join("partial-table-proof");
            db.storage.debug_on_merge_site(
                MergeFailSite::Row,
                Some(Arc::new(move || {
                    if hits.fetch_add(1, Ordering::SeqCst) == 1 {
                        let child: heliosdb_nano::Tuple =
                            bincode::deserialize(&raw.get(b"data:c:1").unwrap().unwrap()).unwrap();
                        let parent: heliosdb_nano::Tuple =
                            bincode::deserialize(&raw.get(b"data:p:1").unwrap().unwrap()).unwrap();
                        assert_eq!(
                            child.values,
                            vec![Value::Int4(1), Value::Int4(10)],
                            "first table really landed"
                        );
                        assert_eq!(parent.values, vec![Value::Int4(1)], "second table did not land");
                        std::fs::write(&proof, b"c_new p_old").unwrap();
                        std::process::exit(91);
                    }
                })),
            );
        } else {
            db.storage
                .debug_on_merge_site(MergeFailSite::AfterIntent, Some(Arc::new(|| std::process::exit(91))));
        }
        let _ = db.execute("MERGE BRANCH work INTO main");
        std::process::exit(0);
    }
    for mode in ["off", "audit"] {
        for site in ["after_intent", "partial_table"] {
            let dir = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", EXACT])
                .env(CHILD_DIR, dir.path())
                .env(CHILD_MODE, mode)
                .env(CHILD_SITE, site)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(
                status.code(),
                Some(91),
                "child must exit at durable intent boundary, not from an unrelated failure"
            );
            if site == "partial_table" {
                assert_eq!(
                    std::fs::read(dir.path().join("partial-table-proof")).unwrap(),
                    b"c_new p_old"
                );
            }
            let db = EmbeddedDatabase::new(dir.path()).unwrap();
            assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(10)]]);
            assert_eq!(
                rows(&db, "c"),
                vec![
                    vec![Value::Int4(1), Value::Int4(10)],
                    vec![Value::Int4(2), Value::Int4(1)]
                ]
            );
            let id = db.storage.get_branch("work").unwrap().branch_id;
            assert!(db
                .storage
                .get(&format!("branch:merge:{id}").into_bytes())
                .unwrap()
                .is_none());
            assert!(db
                .storage
                .get(&format!("branch:merge_fk_policy:{id}").into_bytes())
                .unwrap()
                .is_none());
            if mode == "audit" {
                assert!(!db
                    .query(
                        "SELECT constraint_name FROM pg_log_violations WHERE constraint_name = 'c_fk'",
                        &[]
                    )
                    .unwrap()
                    .is_empty());
            }
        }
    }
}

#[test]
fn final_fk_schema_is_validated_for_empty_and_null_children_even_when_row_checks_are_bypassed() {
    use heliosdb_nano::sql::ConstraintEnforcement;
    for family in FAMILIES {
        for null_child in [false, true] {
            for replacement in [
                None,
                Some("CREATE TABLE p (other INT PRIMARY KEY)"),
                Some("CREATE TABLE p (id INT,other INT PRIMARY KEY)"),
            ] {
                for mode in ["enforced", "off", "audit", "proxy", "not_enforced"] {
                    let db = EmbeddedDatabase::new_in_memory().unwrap();
                    db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
                    db.execute(
                        "CREATE TABLE c (id INT PRIMARY KEY,pid INT,CONSTRAINT c_fk FOREIGN KEY(pid) REFERENCES p(id))",
                    )
                    .unwrap();
                    if null_child {
                        db.execute("INSERT INTO c VALUES (1,NULL)").unwrap();
                    }
                    if mode == "not_enforced" {
                        let catalog = db.storage.catalog();
                        let mut constraints = catalog.load_table_constraints("c").unwrap();
                        constraints.foreign_keys[0].enforcement = ConstraintEnforcement::NotEnforced;
                        catalog.save_table_constraints("c", &constraints).unwrap();
                    }
                    db.execute("CREATE DATABASE BRANCH work").unwrap();
                    db.execute("USE BRANCH work").unwrap();
                    run(&db, family, "DROP TABLE p").unwrap();
                    if let Some(sql) = replacement {
                        run(&db, family, sql).unwrap();
                    }
                    db.execute("USE BRANCH main").unwrap();
                    if mode == "proxy" {
                        db.execute("SET helios.fk_validation_source = 'proxy'").unwrap();
                    } else if mode != "not_enforced" {
                        db.execute(&format!("SET helios.fk_validation = '{mode}'")).unwrap();
                    }
                    let sequence = db.storage.db().latest_sequence_number();
                    let error = run(&db, family, "MERGE BRANCH work INTO main").unwrap_err();
                    assert!(matches!(error, Error::ConstraintViolation(_)), "{error}");
                    assert!(error.to_string().contains("invalid final MERGE schema"), "{error}");
                    assert_eq!(
                        db.storage.db().latest_sequence_number(),
                        sequence,
                        "schema refusal precedes intent and any data/log mutation"
                    );
                    let schema = db.storage.catalog().get_table_schema("p").unwrap();
                    assert_eq!(
                        schema
                            .columns
                            .iter()
                            .map(|column| column.name.as_str())
                            .collect::<Vec<_>>(),
                        vec!["id"]
                    );
                    assert_eq!(
                        rows(&db, "c"),
                        if null_child {
                            vec![vec![Value::Int4(1), Value::Null]]
                        } else {
                            Vec::new()
                        }
                    );
                    assert!(matches!(
                        db.storage.get_branch("work").unwrap().state,
                        heliosdb_nano::storage::BranchState::Active
                    ));
                    // A final image dropping the dependent child too is valid;
                    // this is dependency checking, not refusing DROP/recreate.
                    db.execute("USE BRANCH work").unwrap();
                    run(&db, family, "DROP TABLE c").unwrap();
                    db.execute("USE BRANCH main").unwrap();
                    run(&db, family, "MERGE BRANCH work INTO main").unwrap();
                    assert!(!db.storage.catalog().table_exists("c").unwrap());
                    if replacement.is_none() {
                        assert!(!db.storage.catalog().table_exists("p").unwrap());
                    }
                }
            }
        }
    }
}

#[test]
fn final_fk_key_shape_accepts_a_qualifying_user_created_unique_index() {
    for family in FAMILIES {
        let db = EmbeddedDatabase::new_in_memory().unwrap();
        db.execute("CREATE TABLE p (id INT)").unwrap();
        db.execute("CREATE UNIQUE INDEX p_id_key ON p(id)").unwrap();
        db.execute("CREATE TABLE c (id INT PRIMARY KEY,pid INT REFERENCES p(id))")
            .unwrap();
        db.execute("CREATE DATABASE BRANCH work").unwrap();
        db.execute("USE BRANCH work").unwrap();
        run(&db, family, "INSERT INTO p VALUES (1)").unwrap();
        run(&db, family, "INSERT INTO c VALUES (1,1)").unwrap();
        db.execute("USE BRANCH main").unwrap();
        run(&db, family, "MERGE BRANCH work INTO main").unwrap();
        assert_eq!(rows(&db, "p"), vec![vec![Value::Int4(1)]]);
        assert_eq!(rows(&db, "c"), vec![vec![Value::Int4(1), Value::Int4(1)]]);
    }
}

fn seed_strict_resume(
    db: &EmbeddedDatabase,
    family: Family,
    parent_first: bool,
    branch_target: bool,
) -> (&'static str, &'static str, &'static str) {
    let parent = if parent_first { "a_parent" } else { "z_parent" };
    let child = if parent_first { "z_child" } else { "a_child" };
    db.execute(&format!("CREATE TABLE {parent} (id INT PRIMARY KEY)"))
        .unwrap();
    db.execute(&format!("INSERT INTO {parent} VALUES (1)")).unwrap();
    db.execute(&format!("CREATE TABLE {child} (id INT PRIMARY KEY,pid INT,CONSTRAINT child_fk FOREIGN KEY(pid) REFERENCES {parent}(id) ON UPDATE CASCADE)")).unwrap();
    db.execute(&format!("INSERT INTO {child} VALUES (1,1)")).unwrap();
    let target = if branch_target {
        db.execute("CREATE DATABASE BRANCH target").unwrap();
        "target"
    } else {
        "main"
    };
    db.execute(&format!("CREATE DATABASE BRANCH work PARENT {target}"))
        .unwrap();
    db.execute("USE BRANCH work").unwrap();
    run(db, family, &format!("UPDATE {parent} SET id = 10 WHERE id = 1")).unwrap();
    db.execute(&format!("USE BRANCH {target}")).unwrap();
    (parent, child, target)
}

#[test]
fn strict_partial_merge_revalidates_new_target_children_in_both_table_orders_and_target_levels() {
    for family in FAMILIES {
        for parent_first in [false, true] {
            for branch_target in [false, true] {
                let db = EmbeddedDatabase::new_in_memory().unwrap();
                let (parent, child, target) = seed_strict_resume(&db, family, parent_first, branch_target);
                let merge = format!("MERGE BRANCH work INTO {target}");
                db.storage.debug_fail_merge_at(MergeFailSite::Row, 1);
                let error = run(&db, family, &merge).unwrap_err();
                assert!(matches!(&error, Error::QueryExecution(_)), "{error}");
                assert!(
                    error.to_string().contains("stopped after it began changing")
                        && error.to_string().contains("run it again to finish it"),
                    "{error}"
                );
                assert_eq!(
                    rows(&db, parent),
                    vec![vec![Value::Int4(if parent_first { 10 } else { 1 })]]
                );
                assert_eq!(
                    rows(&db, child),
                    vec![vec![Value::Int4(1), Value::Int4(if parent_first { 1 } else { 10 })]],
                    "one table/row actually landed"
                );
                let id = db.storage.get_branch("work").unwrap().branch_id;
                assert!(db
                    .storage
                    .get(&format!("branch:merge:{id}").into_bytes())
                    .unwrap()
                    .is_some());
                assert_eq!(
                    db.storage
                        .get(&format!("branch:merge_fk_policy:{id}").into_bytes())
                        .unwrap(),
                    Some(vec![1, 0])
                );
                let old_insert = run(&db, family, &format!("INSERT INTO {child} VALUES (2,1)"));
                if parent_first {
                    // The parent already changed. Current strict FK checking rejects
                    // the missing old key immediately; pending intents do not fence DML.
                    fk_error(old_insert.unwrap_err());
                    assert_eq!(
                        run(&db, family, &format!("INSERT INTO {child} VALUES (2,10)")).unwrap(),
                        1
                    );
                } else {
                    // The old key still exists in the current target. This write is
                    // valid now; final-image revalidation must reject its later orphan.
                    assert_eq!(old_insert.unwrap(), 1);
                    let before = (rows(&db, parent), rows(&db, child));
                    let sequence = db.storage.db().latest_sequence_number();
                    fk_error(run(&db, family, &merge).unwrap_err());
                    assert_eq!(
                        db.storage.db().latest_sequence_number(),
                        sequence,
                        "resume rejection writes no further rows/intent/WAL"
                    );
                    assert_eq!((rows(&db, parent), rows(&db, child)), before);
                    assert_eq!(
                        run(&db, family, &format!("DELETE FROM {child} WHERE id = 2")).unwrap(),
                        1
                    );
                }
                run(&db, family, &merge).unwrap();
                assert_eq!(rows(&db, parent), vec![vec![Value::Int4(10)]]);
                let mut expected = vec![vec![Value::Int4(1), Value::Int4(10)]];
                if parent_first {
                    expected.push(vec![Value::Int4(2), Value::Int4(10)]);
                }
                assert_eq!(
                    rows(&db, child),
                    expected,
                    "source changes and valid target edits both survive"
                );
                assert!(db
                    .storage
                    .get(&format!("branch:merge:{id}").into_bytes())
                    .unwrap()
                    .is_none());
                assert!(db
                    .storage
                    .get(&format!("branch:merge_fk_policy:{id}").into_bytes())
                    .unwrap()
                    .is_none());
                if branch_target {
                    db.execute("USE BRANCH main").unwrap();
                    assert_eq!(rows(&db, parent), vec![vec![Value::Int4(1)]]);
                    assert_eq!(rows(&db, child), vec![vec![Value::Int4(1), Value::Int4(1)]]);
                }
            }
        }
    }
}

#[test]
fn strict_partial_merge_crash_reopens_to_exact_final_images_on_main_and_branch_targets() {
    const EXACT: &str = "strict_partial_merge_crash_reopens_to_exact_final_images_on_main_and_branch_targets";
    const DIR: &str = "HDB_FK_STRICT_RESUME_DIR";
    const ORDER: &str = "HDB_FK_STRICT_RESUME_ORDER";
    const LEVEL: &str = "HDB_FK_STRICT_RESUME_LEVEL";
    if let Ok(dir) = std::env::var(DIR) {
        let parent_first = std::env::var(ORDER).unwrap() == "parent";
        let branch_target = std::env::var(LEVEL).unwrap() == "branch";
        let db = EmbeddedDatabase::new(&dir).unwrap();
        let (parent, child, target) = seed_strict_resume(&db, Family::Text, parent_first, branch_target);
        let target_id = db.storage.get_branch(target).unwrap().branch_id;
        let storage = Arc::clone(&db.storage);
        let proof = std::path::Path::new(&dir).join("strict-partial-proof");
        let hits = AtomicUsize::new(0);
        db.storage.debug_on_merge_site(
            MergeFailSite::Row,
            Some(Arc::new(move || {
                if hits.fetch_add(1, Ordering::SeqCst) == 1 {
                    let view =
                        heliosdb_nano::storage::BranchView::for_chain(&storage, target, vec![target_id]).unwrap();
                    let parent_schema = storage.catalog().get_table_schema(parent).unwrap();
                    let child_schema = storage.catalog().get_table_schema(child).unwrap();
                    let p = view.get(None, parent, 1, &parent_schema).unwrap().unwrap();
                    let c = view.get(None, child, 1, &child_schema).unwrap().unwrap();
                    assert_eq!(p.values, vec![Value::Int4(if parent_first { 10 } else { 1 })]);
                    assert_eq!(
                        c.values,
                        vec![Value::Int4(1), Value::Int4(if parent_first { 1 } else { 10 })]
                    );
                    std::fs::write(
                        &proof,
                        if parent_first {
                            b"parent_new child_old".as_slice()
                        } else {
                            b"child_new parent_old".as_slice()
                        },
                    )
                    .unwrap();
                    std::process::exit(91);
                }
            })),
        );
        let _ = db.execute(&format!("MERGE BRANCH work INTO {target}"));
        std::process::exit(0);
    }
    for parent_first in [false, true] {
        for branch_target in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", EXACT])
                .env(DIR, dir.path())
                .env(ORDER, if parent_first { "parent" } else { "child" })
                .env(LEVEL, if branch_target { "branch" } else { "main" })
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(
                status.code(),
                Some(91),
                "the child must prove a real partial table/row state before exiting"
            );
            assert_eq!(
                std::fs::read(dir.path().join("strict-partial-proof")).unwrap(),
                if parent_first {
                    b"parent_new child_old".as_slice()
                } else {
                    b"child_new parent_old".as_slice()
                }
            );
            let db = EmbeddedDatabase::new(dir.path()).unwrap();
            let parent = if parent_first { "a_parent" } else { "z_parent" };
            let child = if parent_first { "z_child" } else { "a_child" };
            if branch_target {
                db.execute("USE BRANCH target").unwrap();
            }
            assert_eq!(rows(&db, parent), vec![vec![Value::Int4(10)]]);
            assert_eq!(rows(&db, child), vec![vec![Value::Int4(1), Value::Int4(10)]]);
            let id = db.storage.get_branch("work").unwrap().branch_id;
            assert!(db
                .storage
                .get(&format!("branch:merge:{id}").into_bytes())
                .unwrap()
                .is_none());
            assert!(db
                .storage
                .get(&format!("branch:merge_fk_policy:{id}").into_bytes())
                .unwrap()
                .is_none());
            if branch_target {
                db.execute("USE BRANCH main").unwrap();
                assert_eq!(rows(&db, parent), vec![vec![Value::Int4(1)]]);
                assert_eq!(rows(&db, child), vec![vec![Value::Int4(1), Value::Int4(1)]]);
            }
        }
    }
}
