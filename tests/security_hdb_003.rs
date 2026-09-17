//! HDB-003 — a binary dump/restore round trip must not drop FOREIGN KEY,
//! CHECK or table-level UNIQUE constraints.
//!
//! Reported shape (v4.31.1, still present at d44d4eb):
//!
//! ```sql
//! CREATE TABLE child (
//!   id integer PRIMARY KEY,
//!   parent_id integer REFERENCES parent(id),
//!   status text CHECK (status <> 'invalid')
//! );
//! -- dump, restore into a fresh database, then:
//! INSERT INTO child VALUES (1, 999, 'valid');   -- ACCEPTED (orphan)
//! INSERT INTO child VALUES (2, 1, 'invalid');   -- ACCEPTED (bad status)
//! ```
//!
//! The dump serialised the `Schema` + vector index metadata + rows per table.
//! `Schema` carries only the COLUMN flags, so every FK, CHECK and table-level
//! UNIQUE — which live in the catalog's separate `TableConstraints` record —
//! was silently dropped, and the restore validated nothing.
//!
//! The contract these tests pin:
//!
//! * format v2 carries each table's `TableConstraints` and the restore
//!   re-registers them through the same catalog funnels DDL uses, so the
//!   restored database enforces exactly what the source did;
//! * constraints are registered AFTER every table is created and populated, so
//!   a child that sorts before its parent — and a cycle — both survive;
//! * a restore whose data violates its own constraints FAILS, naming the
//!   table, the constraint and the offending keys;
//! * version-1 dumps still restore (without constraints);
//! * a dump written uncompressed restores through a zstd-configured manager
//!   (the reader used to decompress with the restoring manager's setting).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::sql::{ConstraintEnforcement, ReferentialAction};
use heliosdb_nano::storage::{RestoreOptions, RestoreReport};
use heliosdb_nano::{EmbeddedDatabase, Result};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------- helpers --

/// Assert a statement is refused, and that the message names `expect`.
fn assert_rejected(db: &EmbeddedDatabase, sql: &str, expect: &str) {
    match db.execute(sql) {
        Ok(_) => panic!("`{sql}` was ACCEPTED; expected a constraint violation mentioning `{expect}`"),
        Err(e) => {
            let message = e.to_string();
            assert!(
                message.to_lowercase().contains(&expect.to_lowercase()),
                "`{sql}` was refused, but the message does not mention `{expect}`: {message}"
            );
        }
    }
}

/// A dump file path inside `dir` (never inside a data directory).
fn dump_path(dir: &TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

/// The repository's `tests/fixtures` directory.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Restore through the manager so the test can read the `RestoreReport`
/// (`EmbeddedDatabase::restore_from_dump` returns `()` and drops the warnings).
/// `validate` is `RestoreOptions::validate_constraints` — the `--no-validate`
/// switch.
fn restore_with_report(db: &mut EmbeddedDatabase, dump: &Path, validate: bool) -> Result<RestoreReport> {
    let manager = db.dump_manager.clone();
    let options = RestoreOptions {
        input_path: dump.to_path_buf(),
        validate_constraints: validate,
        ..RestoreOptions::default()
    };
    manager.restore(&options, db)
}

/// A source database holding an orphan child row: the FK is ordinary, the row
/// is written with the documented session escape hatch
/// `SET helios.fk_validation = 'off'` (docs/guides/fk_validation_modes.md).
fn source_with_one_orphan(dump: &Path) -> Result<()> {
    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE op (id integer PRIMARY KEY)")?;
    source.execute("CREATE TABLE oc (id integer PRIMARY KEY, pid integer REFERENCES op(id))")?;
    source.execute("INSERT INTO op VALUES (1)")?;
    source.execute("SET helios.fk_validation = 'off'")?;
    source.execute("INSERT INTO oc VALUES (2, 999)")?;
    source.execute("SET helios.fk_validation = 'enforced'")?;
    assert_eq!(
        source.query("SELECT id FROM oc", &[])?.len(),
        1,
        "the orphan row was not written; this helper can no longer craft a bad dump"
    );
    source.dump_full(dump)?;
    Ok(())
}

// ------------------------------------------------------------------ tests --

/// 1. The report's own proof-of-concept: both invalid inserts must fail after a
///    restore, a valid one must succeed, and the enforcement must survive a
///    reopen of the restored data directory.
#[test]
fn hdb003_report_poc_restored_constraints_are_enforced() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "poc.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE parent (id integer PRIMARY KEY)")?;
    source.execute(
        "CREATE TABLE child (id integer PRIMARY KEY, parent_id integer REFERENCES parent(id), \
         status text CHECK (status <> 'invalid'))",
    )?;
    source.execute("INSERT INTO parent VALUES (1)")?;
    source.execute("INSERT INTO child VALUES (10, 1, 'valid')")?;
    source.dump_full(&dump)?;
    drop(source);

    let restored_dir = workspace.path().join("restored");
    std::fs::create_dir_all(&restored_dir)?;
    {
        let mut db = EmbeddedDatabase::new(&restored_dir)?;
        db.restore_from_dump(&dump)?;

        assert_eq!(db.query("SELECT id FROM child", &[])?.len(), 1, "row not restored");

        assert_rejected(&db, "INSERT INTO child VALUES (1, 999, 'valid')", "parent");
        assert_rejected(&db, "INSERT INTO child VALUES (2, 1, 'invalid')", "check");

        db.execute("INSERT INTO child VALUES (3, 1, 'ok')")?;
        assert_eq!(db.query("SELECT id FROM child", &[])?.len(), 2);
        drop(db);
    }

    // Reopen: the constraints are durable catalog records, not process state.
    {
        let db = EmbeddedDatabase::new(&restored_dir)?;
        assert_rejected(&db, "INSERT INTO child VALUES (4, 999, 'valid')", "parent");
        assert_rejected(&db, "INSERT INTO child VALUES (5, 1, 'invalid')", "check");
    }

    Ok(())
}

/// 2. A composite FK keeps its name, columns, referential actions and
///    deferrability, and still cascades after a restore.
#[test]
fn hdb003_composite_fk_actions_and_deferrable_survive() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "composite_fk.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE p2 (a integer, b integer, PRIMARY KEY (a, b))")?;
    source.execute(
        "CREATE TABLE c2 (id integer PRIMARY KEY, a integer, b integer, \
         CONSTRAINT c2_ab_fk FOREIGN KEY (a, b) REFERENCES p2 (a, b) \
         ON DELETE CASCADE ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED)",
    )?;
    source.execute("INSERT INTO p2 VALUES (1, 2)")?;
    source.execute("INSERT INTO c2 VALUES (100, 1, 2)")?;
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&dump)?;

    let constraints = db.storage.catalog().load_table_constraints("c2")?;
    assert_eq!(
        constraints.foreign_keys.len(),
        1,
        "exactly one FK should have been restored, got {:?}",
        constraints.foreign_keys
    );
    let fk = &constraints.foreign_keys[0];
    assert_eq!(fk.name, "c2_ab_fk");
    assert_eq!(fk.columns, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(fk.references_table, "p2");
    assert_eq!(fk.references_columns, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(fk.on_delete, ReferentialAction::Cascade);
    assert_eq!(fk.on_update, ReferentialAction::Restrict);
    assert!(fk.deferrable, "DEFERRABLE was lost");
    assert!(fk.initially_deferred, "INITIALLY DEFERRED was lost");

    // The restored FK is enforced, not merely recorded.
    assert_rejected(&db, "INSERT INTO c2 VALUES (101, 9, 9)", "c2_ab_fk");

    // ... and ON DELETE CASCADE still fires in the restored database.
    db.execute("DELETE FROM p2")?;
    assert_eq!(
        db.query("SELECT id FROM c2", &[])?.len(),
        0,
        "ON DELETE CASCADE did not cascade after restore"
    );

    Ok(())
}

/// 3. CHECK expressions survive verbatim, under both the user's name and the
///    auto-generated one.
#[test]
fn hdb003_check_expressions_survive_verbatim() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "checks.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute(
        "CREATE TABLE ck (id integer PRIMARY KEY, status text CHECK (status <> 'invalid'), qty integer, \
         CONSTRAINT qty_positive CHECK (qty > 0))",
    )?;
    source.execute("INSERT INTO ck VALUES (1, 'ok', 5)")?;
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&dump)?;

    let constraints = db.storage.catalog().load_table_constraints("ck")?;
    let names: Vec<String> = constraints.check_constraints.iter().map(|c| c.name.clone()).collect();
    assert_eq!(
        constraints.check_constraints.len(),
        2,
        "both CHECK constraints should survive, got {names:?}"
    );
    assert!(
        names.contains(&"qty_positive".to_string()),
        "named CHECK lost: {names:?}"
    );
    assert!(
        names.contains(&"ck_check".to_string()),
        "auto-named CHECK lost or renamed: {names:?}"
    );

    assert_rejected(&db, "INSERT INTO ck VALUES (2, 'invalid', 5)", "check");
    assert_rejected(&db, "INSERT INTO ck VALUES (3, 'ok', 0)", "qty_positive");
    db.execute("INSERT INTO ck VALUES (4, 'ok', 1)")?;

    Ok(())
}

/// 4. A table-level (composite) UNIQUE is invisible to `Schema`; it must still
///    be restored AND backed by a populated index.
#[test]
fn hdb003_table_level_unique_survives() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "unique.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE uq (id integer PRIMARY KEY, a integer, b integer, UNIQUE (a, b))")?;
    source.execute("INSERT INTO uq VALUES (1, 1, 1)")?;
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&dump)?;

    let constraints = db.storage.catalog().load_table_constraints("uq")?;
    assert!(
        constraints
            .unique_constraints
            .iter()
            .any(|uc| !uc.is_primary_key && uc.columns == vec!["a".to_string(), "b".to_string()]),
        "table-level UNIQUE (a, b) was not restored: {:?}",
        constraints.unique_constraints
    );

    // The duplicate is rejected against a row restored BEFORE the constraint
    // was registered — i.e. the enforcing index was backfilled.
    assert_rejected(&db, "INSERT INTO uq VALUES (2, 1, 1)", "uniq");
    db.execute("INSERT INTO uq VALUES (3, 1, 2)")?;

    Ok(())
}

/// 5. Constraints are registered only after every table exists: a child that
///    sorts before its parent, and a two-table cycle, both restore.
#[test]
fn hdb003_cyclic_and_child_first_ordering() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "cycle.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE b_parent (id integer PRIMARY KEY)")?;
    source.execute("CREATE TABLE a_child (id integer PRIMARY KEY, pid integer REFERENCES b_parent(id))")?;
    // A cycle: x -> y and y -> x, both nullable so rows can exist at all.
    source.execute("CREATE TABLE y (id integer PRIMARY KEY, x_id integer)")?;
    source.execute("CREATE TABLE x (id integer PRIMARY KEY, y_id integer REFERENCES y(id))")?;
    source.execute("ALTER TABLE y ADD CONSTRAINT y_x_fk FOREIGN KEY (x_id) REFERENCES x(id)")?;

    source.execute("INSERT INTO b_parent VALUES (1)")?;
    source.execute("INSERT INTO a_child VALUES (1, 1)")?;
    source.execute("INSERT INTO y VALUES (1, NULL)")?;
    source.execute("INSERT INTO x VALUES (1, 1)")?;
    source.dump_full(&dump)?;
    drop(source);

    // Table names are written in the clear, so the file itself shows that the
    // child section really does precede the parent's.
    let bytes = std::fs::read(&dump)?;
    let position = |needle: &[u8]| {
        bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap_or_else(|| panic!("table name {:?} missing from the dump", String::from_utf8_lossy(needle)))
    };
    assert!(
        position(b"a_child".as_slice()) < position(b"b_parent".as_slice()),
        "fixture no longer exercises child-before-parent ordering"
    );

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&dump)?;

    assert_rejected(&db, "INSERT INTO a_child VALUES (2, 999)", "b_parent");
    db.execute("INSERT INTO a_child VALUES (3, 1)")?;

    // Both halves of the cycle are enforced, named by their own constraints.
    assert_rejected(&db, "INSERT INTO x VALUES (2, 999)", "fk_x_y_id__y");
    assert_rejected(&db, "INSERT INTO y VALUES (2, 999)", "y_x_fk");

    Ok(())
}

/// 6. A restore whose DATA violates its own constraints must fail, naming the
///    table, the constraint and the offending key.
///
/// The violation is crafted with the documented session escape hatch
/// `SET helios.fk_validation = 'off'` (docs/guides/fk_validation_modes.md),
/// which lets the source database accept an orphan row; the destination is a
/// fresh database with the default (enforced) mode.
#[test]
fn hdb003_invalid_data_fails_validation() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "invalid.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE fkp (id integer PRIMARY KEY)")?;
    source.execute("CREATE TABLE fkc (id integer PRIMARY KEY, pid integer REFERENCES fkp(id))")?;
    source.execute("INSERT INTO fkp VALUES (1)")?;
    source.execute("SET helios.fk_validation = 'off'")?;
    source.execute("INSERT INTO fkc VALUES (2, 999)")?;
    source.execute("SET helios.fk_validation = 'enforced'")?;
    assert_eq!(
        source.query("SELECT id FROM fkc", &[])?.len(),
        1,
        "the orphan row was not written; this test can no longer craft a bad dump"
    );
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    let error = db
        .restore_from_dump(&dump)
        .expect_err("a dump whose rows violate its own FK must not restore as a valid database");
    let message = error.to_string();
    assert!(
        message.contains("restore validation failed"),
        "unexpected error text: {message}"
    );
    assert!(message.contains("fkc"), "the error does not name the table: {message}");
    assert!(
        message.contains("fk_fkc_pid__fkp"),
        "the error does not name the constraint: {message}"
    );
    assert!(
        message.contains("pid=999"),
        "the error does not name the offending key: {message}"
    );

    Ok(())
}

/// 7. A real version-1 dump (`tests/fixtures/dump_v1_users.hdmp`, generated on
///    48b1322, which still wrote v1) restores — without constraints, because a
///    v1 file carries none. Do NOT regenerate the fixture.
#[test]
fn hdb003_v1_dumps_still_restore() -> Result<()> {
    let path = fixture("dump_v1_users.hdmp");
    assert!(path.exists(), "missing v1 fixture at {}", path.display());

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&path)?;

    let rows = db.query("SELECT id, name FROM users ORDER BY id", &[])?;
    assert_eq!(rows.len(), 2, "v1 dump did not restore its rows");

    let constraints = db.storage.catalog().load_table_constraints("users")?;
    assert!(
        constraints.foreign_keys.is_empty()
            && constraints.check_constraints.is_empty()
            && constraints.unique_constraints.is_empty(),
        "a v1 dump carries no constraints, so none should be invented: {constraints:?}"
    );

    Ok(())
}

/// 8. The incremental writer emits the same TABL sections, so constraints ride
///    along there too: full dump + incremental dump restores an enforced FK.
#[test]
fn hdb003_incremental_dump_carries_constraints() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let full = dump_path(&workspace, "base.hdmp");
    let incremental = dump_path(&workspace, "delta.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE ip (id integer PRIMARY KEY)")?;
    source.execute("INSERT INTO ip VALUES (1)")?;
    source.dump_full(&full)?; // clears the dirty set

    source.execute("CREATE TABLE ic (id integer PRIMARY KEY, pid integer REFERENCES ip(id))")?;
    source.execute("INSERT INTO ic VALUES (1, 1)")?;
    source.dump_manager.dirty_tracker().mark_table_dirty("ic");
    source.dump_incremental(&incremental)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&full)?;
    db.restore_from_dump(&incremental)?;

    assert_eq!(db.query("SELECT id FROM ic", &[])?.len(), 1);
    assert_rejected(&db, "INSERT INTO ic VALUES (2, 999)", "ip");
    db.execute("INSERT INTO ic VALUES (3, 1)")?;

    Ok(())
}

/// 9. Constraints on an EMPTY table survive (there is no row to hang them on,
///    and nothing for phase C to validate).
#[test]
fn hdb003_empty_table_with_constraints() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "empty.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE ep (id integer PRIMARY KEY)")?;
    source
        .execute("CREATE TABLE ec (id integer PRIMARY KEY, pid integer REFERENCES ep(id), v integer CHECK (v > 0))")?;
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&dump)?;

    assert_rejected(&db, "INSERT INTO ec VALUES (1, 999, 5)", "ep");
    assert_rejected(&db, "INSERT INTO ec VALUES (2, NULL, 0)", "check");

    db.execute("INSERT INTO ep VALUES (1)")?;
    db.execute("INSERT INTO ec VALUES (3, 1, 5)")?;

    Ok(())
}

/// 1b. A dump written with compression `None` must restore through a manager
///     configured with zstd: the reader decompresses with the compression the
///     FILE records, not its own setting.
#[test]
fn hdb003_uncompressed_dump_restores() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "plain.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE u (id integer PRIMARY KEY, name text, parent_id integer)")?;
    source.execute("INSERT INTO u VALUES (1, 'a', NULL)")?;
    source.execute("INSERT INTO u VALUES (2, 'b', 1)")?;
    source.dump_full_uncompressed(&dump)?;
    drop(source);

    // `new_in_memory` builds its dump manager with Zstd — the mismatch that
    // used to make this restore fail immediately after a successful dump.
    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&dump)?;
    assert_eq!(db.query("SELECT id FROM u ORDER BY id", &[])?.len(), 2);

    Ok(())
}

/// 10. BLOCK — a `LOCK-FREE` FK is one of the write path's non-fatal cases
///     (`check_fk_constraints_on_write` records the violation and continues),
///     so a dump of a database holding such rows must RESTORE, with the
///     violation reported as a warning. Before this, the backup of a supported
///     configuration could not be restored at all.
#[test]
fn hdb003_lock_free_fk_violations_restore_with_warnings() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "lock_free.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE lp (id integer PRIMARY KEY)")?;
    source.execute("CREATE TABLE lc (id integer PRIMARY KEY, pid integer REFERENCES lp(id))")?;
    source.execute("INSERT INTO lp VALUES (1)")?;
    source.execute("SET helios.fk_validation = 'off'")?;
    source.execute("INSERT INTO lc VALUES (2, 999)")?;
    source.execute("SET helios.fk_validation = 'enforced'")?;

    // LOCK-FREE has no CREATE TABLE spelling; it is a field of the stored
    // constraint record, which is exactly what the dump carries.
    {
        let catalog = source.storage.catalog();
        let mut constraints = catalog.load_table_constraints("lc")?;
        for fk in constraints.foreign_keys.iter_mut() {
            fk.enforcement = ConstraintEnforcement::LockFree;
        }
        catalog.save_table_constraints("lc", &constraints)?;
    }
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    // Default (strict) validation: this must NOT fail.
    let report = restore_with_report(&mut db, &dump, true)?;

    assert_eq!(
        db.query("SELECT id FROM lc", &[])?.len(),
        1,
        "the orphan row was dropped"
    );
    assert!(
        report
            .validation_warnings
            .iter()
            .any(|w| w.contains("lc.") && w.contains("LOCK-FREE") && w.contains("pid=999")),
        "the LOCK-FREE violation was not reported as a warning: {:?}",
        report.validation_warnings
    );

    let constraints = db.storage.catalog().load_table_constraints("lc")?;
    assert_eq!(
        constraints.foreign_keys.first().map(|fk| fk.enforcement),
        Some(ConstraintEnforcement::LockFree),
        "the enforcement mode did not survive the round trip"
    );

    Ok(())
}

/// 11. BLOCK — the same parity for the DESTINATION session's mode: under
///     `SET helios.fk_validation = 'audit'` the write path accepts orphans and
///     logs them, so a restore must too. (The CLI cannot set the mode on the
///     database it opens, which is why `--no-validate` exists as well.)
#[test]
fn hdb003_audit_mode_destination_restores_with_warnings() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "audit.hdmp");
    source_with_one_orphan(&dump)?;

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.execute("SET helios.fk_validation = 'audit'")?;
    let report = restore_with_report(&mut db, &dump, true)?;

    assert_eq!(
        db.query("SELECT id FROM oc", &[])?.len(),
        1,
        "the orphan row was dropped"
    );
    assert!(
        report
            .validation_warnings
            .iter()
            .any(|w| w.contains("oc.") && w.contains("audit") && w.contains("pid=999")),
        "the audit-mode violation was not reported as a warning: {:?}",
        report.validation_warnings
    );

    // The constraint is registered, so a session back in the default mode
    // rejects NEW violations.
    db.execute("SET helios.fk_validation = 'enforced'")?;
    assert_rejected(&db, "INSERT INTO oc VALUES (3, 404)", "op");

    Ok(())
}

/// 12. BLOCK — the supported override: the destination is a fresh (enforced)
///     database, the dump holds an orphan, and `--no-validate`
///     (`RestoreOptions::validate_constraints = false`) restores it with the
///     violation as a warning instead of a failure.
#[test]
fn hdb003_no_validate_restores_a_violating_dump_with_warnings() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "no_validate.hdmp");
    source_with_one_orphan(&dump)?;

    // Default: a hard failure, and the error says how to override it.
    {
        let mut strict = EmbeddedDatabase::new_in_memory()?;
        let error = restore_with_report(&mut strict, &dump, true)
            .expect_err("an ordinary enforced FK with an orphan row must still fail by default");
        let message = error.to_string();
        assert!(message.contains("restore validation failed"), "{message}");
        assert!(
            message.contains("--no-validate"),
            "the error does not name the override: {message}"
        );
        assert!(
            message.contains("constraints are registered"),
            "the error does not say what is in the target directory: {message}"
        );
    }

    let mut db = EmbeddedDatabase::new_in_memory()?;
    let report = restore_with_report(&mut db, &dump, false)?;

    assert_eq!(
        db.query("SELECT id FROM oc", &[])?.len(),
        1,
        "the orphan row was dropped"
    );
    assert!(
        report
            .validation_warnings
            .iter()
            .any(|w| w.contains("oc.") && w.contains("--no-validate") && w.contains("pid=999")),
        "the violation was not reported as a warning: {:?}",
        report.validation_warnings
    );
    // The constraints are registered either way: NEW violations are rejected.
    assert_rejected(&db, "INSERT INTO oc VALUES (3, 404)", "op");

    Ok(())
}

/// 13. One error for the WHOLE dump: an operator fixing a multi-table dump must
///     not discover one table per restore attempt. Also pins the FK grammar
///     ("1 row references" / "2 rows reference").
#[test]
fn hdb003_validation_failure_names_every_offending_table() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "multi_bad.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE ap (id integer PRIMARY KEY)")?;
    source.execute("CREATE TABLE ac (id integer PRIMARY KEY, pid integer REFERENCES ap(id))")?;
    source.execute("CREATE TABLE bc (id integer PRIMARY KEY, pid integer REFERENCES ap(id))")?;
    source.execute("INSERT INTO ap VALUES (1)")?;
    source.execute("SET helios.fk_validation = 'off'")?;
    source.execute("INSERT INTO ac VALUES (1, 111)")?;
    source.execute("INSERT INTO bc VALUES (1, 222)")?;
    source.execute("INSERT INTO bc VALUES (2, 333)")?;
    source.execute("SET helios.fk_validation = 'enforced'")?;
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    let error = db
        .restore_from_dump(&dump)
        .expect_err("a dump whose rows violate its own FKs must not restore as a valid database");
    let message = error.to_string();

    assert!(
        message.contains("ac."),
        "the error does not name the first table: {message}"
    );
    assert!(
        message.contains("bc."),
        "the error does not name the second table: {message}"
    );
    assert!(
        message.contains("1 row references missing ap rows"),
        "singular FK grammar is wrong: {message}"
    );
    assert!(
        message.contains("2 rows reference missing ap rows"),
        "plural FK grammar is wrong: {message}"
    );

    Ok(())
}

/// 14. A COMPOSITE FK is validated by key, not by "some parent row exists":
///     the matching child row passes and only the orphan is reported.
#[test]
fn hdb003_composite_fk_violation_is_detected() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "composite_bad.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE cp (a integer, b integer, PRIMARY KEY (a, b))")?;
    source.execute(
        "CREATE TABLE cc (id integer PRIMARY KEY, a integer, b integer, \
         CONSTRAINT cc_ab_fk FOREIGN KEY (a, b) REFERENCES cp (a, b))",
    )?;
    source.execute("INSERT INTO cp VALUES (1, 2)")?;
    source.execute("INSERT INTO cc VALUES (1, 1, 2)")?;
    source.execute("SET helios.fk_validation = 'off'")?;
    source.execute("INSERT INTO cc VALUES (2, 9, 9)")?;
    source.execute("SET helios.fk_validation = 'enforced'")?;
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    let error = db
        .restore_from_dump(&dump)
        .expect_err("the orphan composite key must fail the restore");
    let message = error.to_string();
    assert!(message.contains("cc.cc_ab_fk"), "{message}");
    assert!(
        message.contains("a=9, b=9"),
        "the error does not name the composite key: {message}"
    );
    assert!(
        message.contains("1 row references missing cp rows"),
        "the matching row (1, 2) must not be reported as a violation: {message}"
    );

    Ok(())
}

/// 15. A restore on a non-main branch is refused BEFORE anything is created —
///     phase B mints constraint indexes through `ALTER TABLE`, which only runs
///     on main, and discovering that half way through would leave a
///     part-restored database behind.
#[test]
fn hdb003_restore_on_a_branch_is_refused_before_anything_is_created() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "branch.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE bt (id integer PRIMARY KEY, a integer, b integer, UNIQUE (a, b))")?;
    source.execute("INSERT INTO bt VALUES (1, 1, 1)")?;
    source.dump_full(&dump)?;
    drop(source);

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE seed (id integer PRIMARY KEY)")?;
    db.execute("CREATE BRANCH br AS OF NOW")?;
    db.execute("USE BRANCH br")?;

    let error = db
        .restore_from_dump(&dump)
        .expect_err("a restore while a branch is active must be refused");
    let message = error.to_string();
    assert!(
        message.contains("main branch"),
        "the refusal does not say why or what to do: {message}"
    );
    assert!(
        db.query("SELECT id FROM bt", &[]).is_err(),
        "the refused restore created the table anyway"
    );

    db.execute("USE BRANCH main")?;
    db.restore_from_dump(&dump)?;
    assert_eq!(db.query("SELECT id FROM bt", &[])?.len(), 1);

    Ok(())
}

/// 16. `dump_incremental_append` on a path that does not exist yet must write
///     the file HEADER. `File::create` had already made the path exist by the
///     time the "is this a new file?" test ran, so both of its disjuncts were
///     false and the header was skipped: the file began with `INCR` and every
///     restore of it died with "Invalid dump file: bad magic bytes".
#[test]
fn hdb003_incremental_append_to_new_path_writes_a_header() -> Result<()> {
    let workspace = TempDir::new().unwrap();
    let dump = dump_path(&workspace, "append_new.hdmp");

    let source = EmbeddedDatabase::new_in_memory()?;
    source.execute("CREATE TABLE ia (id integer PRIMARY KEY, v text)")?;
    source.execute("INSERT INTO ia VALUES (1, 'x')")?;
    source.dump_manager.dirty_tracker().mark_table_dirty("ia");
    assert!(!dump.exists(), "the test must start from a path that does not exist");
    source.dump_incremental_append(&dump)?;
    drop(source);

    let bytes = std::fs::read(&dump)?;
    assert_eq!(
        &bytes[0..8],
        b"HELIODMP",
        "the appended-to-nothing file has no magic bytes"
    );

    let mut db = EmbeddedDatabase::new_in_memory()?;
    db.restore_from_dump(&dump)?;
    assert_eq!(db.query("SELECT id FROM ia", &[])?.len(), 1);

    Ok(())
}
