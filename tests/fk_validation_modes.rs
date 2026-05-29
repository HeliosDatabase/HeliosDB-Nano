use heliosdb_nano::{EmbeddedDatabase, Value};

#[test]
fn fk_validation_off_allows_bulk_orphans() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");

    db.execute("SET helios.fk_validation = 'off'").expect("set off");
    db.execute("INSERT INTO c (id, parent_id) VALUES (1, 404)")
        .expect("off mode should skip FK validation");

    db.execute("RESET helios.fk_validation").expect("reset");
    let bad = db.execute("INSERT INTO c (id, parent_id) VALUES (2, 404)");
    assert!(bad.is_err(), "reset should restore FK enforcement");
}

#[test]
fn fk_validation_deferred_checks_at_commit() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");

    db.execute("SET helios.fk_validation = 'deferred'")
        .expect("set deferred");
    db.execute("BEGIN").expect("begin");
    db.execute("INSERT INTO c (id, parent_id) VALUES (1, 7)")
        .expect("deferred mode queues FK check");
    db.execute("INSERT INTO p (id) VALUES (7)")
        .expect("parent arrives before commit");
    db.execute("COMMIT").expect("commit validates queued FK");

    db.execute("BEGIN").expect("begin 2");
    db.execute("INSERT INTO c (id, parent_id) VALUES (2, 8)")
        .expect("queue orphan");
    let commit = db.execute("COMMIT");
    assert!(commit.is_err(), "orphan should fail deferred validation at commit");
    db.execute("ROLLBACK").ok();
}

#[test]
fn not_enforced_fk_is_catalogued_but_not_checked() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute(
        "CREATE TABLE c (
            id INTEGER PRIMARY KEY,
            parent_id INTEGER,
            CONSTRAINT fk_c_p FOREIGN KEY (parent_id) REFERENCES p(id) NOT ENFORCED
        )",
    )
    .expect("create c");

    db.execute("INSERT INTO c (id, parent_id) VALUES (1, 123)")
        .expect("NOT ENFORCED should skip FK check");

    db.execute("ALTER TABLE c ALTER CONSTRAINT fk_c_p ENFORCED")
        .expect("toggle enforced");
    let bad = db.execute("INSERT INTO c (id, parent_id) VALUES (2, 123)");
    assert!(bad.is_err(), "ENFORCED should re-enable FK checks");
}

#[test]
fn audit_mode_logs_fk_violations_without_rejecting() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)").expect("create p");
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES p(id))")
        .expect("create c");

    db.execute("SET helios.fk_validation = 'audit'").expect("set audit");
    db.execute("INSERT INTO c (id, parent_id) VALUES (1, 999)")
        .expect("audit mode should not reject");

    let rows = db
        .query("SELECT constraint_name, referenced_table FROM pg_log_violations", &[])
        .expect("audit rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[1], Value::String("p".to_string()));
}

#[test]
fn bulk_load_mode_setting_reaches_storage_engine() {
    let db = EmbeddedDatabase::new_in_memory().expect("db");
    assert!(!db.storage.is_bulk_load_mode());

    db.execute("SET bulk_load_mode = true").expect("set true");
    assert!(db.storage.is_bulk_load_mode());

    db.execute("SET helios.bulk_load_mode = off").expect("set off");
    assert!(!db.storage.is_bulk_load_mode());

    db.execute("SET bulk_load_mode = 1").expect("set 1");
    assert!(db.storage.is_bulk_load_mode());

    db.execute("RESET bulk_load_mode").expect("reset");
    assert!(!db.storage.is_bulk_load_mode());
}
