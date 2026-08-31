//! `ALTER TABLE … RENAME TO` carries the table's TRIGGER records — item #85.
//!
//! # What was wrong
//!
//! `Catalog::rename_table` moved the schema, the row counter, every data row, the
//! compression records and the ART index registrations — and left the
//! `trigger:{old}:*` definitions and `trigger_rowmut:{old}:*` rewrite recipes
//! behind. Both registries are keyed by TABLE NAME, so after a rename:
//!
//!   * the trigger stopped firing (nothing looks under the old name any more), and
//!   * the records were stranded under a name that no longer exists — where the
//!     open-time loaders (`load_all_triggers` /
//!     `load_all_trigger_row_mutations`) still replay them with NO
//!     table-existence filter. So `ALTER TABLE t RENAME TO t_old; CREATE TABLE t
//!     (…)` made the NEW `t` inherit the OLD `t`'s trigger at the next open.
//!
//! That last one is the same resurrection class `Catalog::delete_table_triggers`
//! closes for `DROP TABLE`, and it is why this is a correctness item and not
//! hygiene.
//!
//! # Scope of the fix these tests pin
//!
//! `Catalog::rename_table_inner` — the ONE funnel every rename passes through
//! (`ALTER TABLE … RENAME TO`, `ALTER TABLE … SET SCHEMA`, the MV refresh swap,
//! WAL replay) — now calls `move_table_triggers`.
//! That moves the DURABLE half. The LIVE in-memory registry the executor consults
//! (`EmbeddedDatabase::trigger_registry`) is not reachable from the storage layer,
//! so within the renaming PROCESS the trigger still answers to the old key until
//! the next open. `renaming_a_table_does_not_deregister_the_live_trigger_yet`
//! states that honestly rather than leaving it undiscovered.
//!
//! # What triggers actually do here
//!
//! Bodies still do not execute (see `tests/trigger_row_mutation_tests.rs`). The
//! one mechanism with an effect is the compiled `TriggerRowMutation` recipe from a
//! `BEFORE INSERT … FOR EACH ROW` function whose body is `NEW.<col> = <expr>`,
//! and that is what these tests observe: whether the rewrite follows the table.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Harness (mirrors tests/trigger_row_mutation_tests.rs)
// ---------------------------------------------------------------------------

const REWRITE_BODY: &str = "BEGIN NEW.tag = 'set-by-trigger'; RETURN NEW; END";

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nano_rename_trg_{tag}_{id}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn create_fn(db: &EmbeddedDatabase, name: &str) {
    db.execute(&format!(
        "CREATE FUNCTION {name}() RETURNS TRIGGER AS $$ {REWRITE_BODY} $$ LANGUAGE plpgsql"
    ))
    .unwrap_or_else(|e| panic!("CREATE FUNCTION {name} failed: {e}"));
}

fn first_text(db: &EmbeddedDatabase, sql: &str) -> String {
    let rows = db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    match rows.first().and_then(|r| r.values.first()) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected text from `{sql}`, got {other:?}"),
    }
}

/// Table names carried by the PERSISTED trigger definitions — the durable half,
/// read through the same loader the open-time restore uses.
fn persisted_trigger_tables(db: &EmbeddedDatabase) -> Vec<String> {
    db.storage
        .catalog()
        .load_all_triggers()
        .expect("load_all_triggers")
        .into_iter()
        .map(|t| t.table_name)
        .collect()
}

/// Table names carried by the PERSISTED row-rewrite recipes.
fn persisted_recipe_tables(db: &EmbeddedDatabase) -> Vec<String> {
    db.storage
        .catalog()
        .load_all_trigger_row_mutations()
        .expect("load_all_trigger_row_mutations")
        .into_iter()
        .map(|(table, _trigger, _recipe)| table)
        .collect()
}

fn setup(db: &EmbeddedDatabase) {
    db.execute("CREATE TABLE ren_a (id INT, tag TEXT)")
        .expect("subject table");
    create_fn(db, "ren_mut_fn");
    db.execute("CREATE TRIGGER ren_trg BEFORE INSERT ON ren_a FOR EACH ROW EXECUTE FUNCTION ren_mut_fn()")
        .expect("create trigger");
}

// ---------------------------------------------------------------------------
// 1. The durable records move
// ---------------------------------------------------------------------------

/// The definition record AND the rewrite recipe must both end up keyed by the new
/// table name, with the definition's own `table_name` FIELD rewritten too — the
/// open-time loader registers by that field, not by the key.
///
/// TEXT FAMILY ONLY, and that is a finding rather than an omission:
/// `ALTER TABLE … RENAME TO` has NO arm in `Executor::plan_to_operator`
/// (`LogicalPlan::AlterTableRename` appears only in `src/lib.rs`'s text-family
/// match), so over the PG extended protocol it fails loudly with
/// "Operator not yet implemented: AlterTableRename". That parity gap is pinned by
/// `alter_table_rename_is_still_unimplemented_on_the_params_family` below. The fix
/// under test lives in the shared `Catalog::rename_table` funnel, so it will cover
/// the params family for free the moment that arm exists.
#[test]
fn rename_moves_the_persisted_trigger_records() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    setup(&db);

    assert_eq!(
        persisted_trigger_tables(&db),
        vec!["ren_a".to_string()],
        "sanity: the definition must be persisted under the original name"
    );
    assert_eq!(
        persisted_recipe_tables(&db),
        vec!["ren_a".to_string()],
        "sanity: the recipe must be persisted under the original name"
    );

    db.execute("ALTER TABLE ren_a RENAME TO ren_b").expect("rename");

    assert_eq!(
        persisted_trigger_tables(&db),
        vec!["ren_b".to_string()],
        "*** STRANDED *** the trigger definition stayed under the old table name"
    );
    assert_eq!(
        persisted_recipe_tables(&db),
        vec!["ren_b".to_string()],
        "*** STRANDED *** the rewrite recipe stayed under the old table name"
    );
}

/// PINS A KNOWN GAP, not the fix. `ALTER TABLE … RENAME TO` is handled only in the
/// text family's plan match; the params family (PG extended protocol — psycopg,
/// JDBC, sqlx, node-postgres, and every REST write) reaches
/// `Executor::plan_to_operator`, which has no `AlterTableRename` arm.
///
/// It fails LOUDLY, which is why this is a separate item and not folded in here.
/// If this test starts failing because the rename succeeded, the parity gap has
/// been closed: delete this test and extend `rename_moves_the_persisted_trigger_records`
/// to loop over both families instead.
#[test]
fn alter_table_rename_is_still_unimplemented_on_the_params_family() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE pf_a (id INT, tag TEXT)").unwrap();

    // Today this is `Operator not yet implemented: AlterTableRename` from
    // `Executor::plan_to_operator`'s catch-all. The exact wording is not pinned
    // here — only that the statement does not silently claim to have worked.
    db.execute_params("ALTER TABLE pf_a RENAME TO pf_b", &[]).expect_err(
        "ALTER TABLE … RENAME TO now works on the params family — the parity gap is closed; \
         delete this test and cover both families in rename_moves_the_persisted_trigger_records",
    );

    // The table is untouched under its original name — a failed rename must not
    // half-apply.
    assert!(
        db.query("SELECT id FROM pf_a", &[]).is_ok(),
        "the failed params-family rename damaged the table"
    );
    assert!(
        db.query("SELECT id FROM pf_b", &[]).is_err(),
        "the params-family rename partially applied: the new name exists"
    );
}

/// A rename must not invent, duplicate or lose records: exactly one definition and
/// exactly one recipe before, exactly one of each after.
#[test]
fn rename_neither_duplicates_nor_drops_trigger_records() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    setup(&db);
    db.execute("ALTER TABLE ren_a RENAME TO ren_b").expect("rename");

    assert_eq!(
        persisted_trigger_tables(&db).len(),
        1,
        "the rename left more than one trigger definition behind"
    );
    assert_eq!(
        persisted_recipe_tables(&db).len(),
        1,
        "the rename left more than one rewrite recipe behind"
    );
}

/// NEGATIVE: another table's triggers must not move with the renamed one. This is
/// the guard on the `trigger:{old}:` prefix — a prefix that matched too much would
/// silently re-attach an unrelated table's trigger.
#[test]
fn rename_leaves_another_tables_triggers_alone() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    setup(&db);
    db.execute("CREATE TABLE ren_other (id INT, tag TEXT)")
        .expect("other table");
    db.execute("CREATE TRIGGER ren_other_trg BEFORE INSERT ON ren_other FOR EACH ROW EXECUTE FUNCTION ren_mut_fn()")
        .expect("other trigger");

    db.execute("ALTER TABLE ren_a RENAME TO ren_b").expect("rename");

    let mut tables = persisted_trigger_tables(&db);
    tables.sort();
    assert_eq!(
        tables,
        vec!["ren_b".to_string(), "ren_other".to_string()],
        "the rename disturbed an unrelated table's trigger records"
    );

    // And the untouched table's rewrite still fires.
    db.execute("INSERT INTO ren_other (id, tag) VALUES (1, 'original')")
        .expect("insert into the untouched table");
    assert_eq!(
        first_text(&db, "SELECT tag FROM ren_other WHERE id = 1"),
        "set-by-trigger",
        "an unrelated table's rewrite stopped working after a rename"
    );
}

/// A table with no triggers renames exactly as before — the two prefix scans find
/// nothing and change nothing.
#[test]
fn rename_of_a_trigger_free_table_is_unaffected() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    db.execute("CREATE TABLE plain_a (id INT, tag TEXT)").unwrap();
    db.execute("INSERT INTO plain_a (id, tag) VALUES (1, 'v')").unwrap();
    db.execute("ALTER TABLE plain_a RENAME TO plain_b").expect("rename");

    assert_eq!(
        first_text(&db, "SELECT tag FROM plain_b WHERE id = 1"),
        "v",
        "the rename lost the row"
    );
    assert!(
        persisted_trigger_tables(&db).is_empty(),
        "a trigger-free rename invented a trigger record"
    );
}

// ---------------------------------------------------------------------------
// 2. *** THE RESURRECTION CASE *** — the reason this is not hygiene
// ---------------------------------------------------------------------------

/// Rename a table out of the way, create a FRESH table under the old name, and
/// restart. The open-time loaders replay every persisted trigger record with no
/// table-existence filter, so a stranded `trigger:ren_a:*` record attaches itself
/// to the brand-new `ren_a` — a table the user never put a trigger on silently
/// starts rewriting its rows.
#[test]
fn a_new_table_under_the_old_name_does_not_inherit_the_renamed_tables_trigger() {
    let dir = scratch_dir("resurrect");

    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        setup(&db);
        db.execute("ALTER TABLE ren_a RENAME TO ren_b").expect("rename");
        db.execute("CREATE TABLE ren_a (id INT, tag TEXT)")
            .expect("a fresh, unrelated table under the freed name");
    }

    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");

        assert!(
            db.trigger_registry.has_triggers_for_table("ren_b"),
            "the renamed table lost its trigger across the restart"
        );
        assert!(
            !db.trigger_registry.has_triggers_for_table("ren_a"),
            "*** RESURRECTION *** the fresh table created under the old name inherited the \
             renamed table's trigger"
        );

        db.execute("INSERT INTO ren_a (id, tag) VALUES (1, 'original')")
            .expect("insert into the fresh table");
        assert_eq!(
            first_text(&db, "SELECT tag FROM ren_a WHERE id = 1"),
            "original",
            "*** RESURRECTION *** a row written to the fresh table was rewritten by the \
             renamed table's stranded trigger recipe"
        );

        db.execute("INSERT INTO ren_b (id, tag) VALUES (1, 'original')")
            .expect("insert into the renamed table");
        assert_eq!(
            first_text(&db, "SELECT tag FROM ren_b WHERE id = 1"),
            "set-by-trigger",
            "the renamed table's rewrite recipe did not follow it"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 3. What is NOT fixed — stated, not left to be discovered
// ---------------------------------------------------------------------------

/// HONEST LIMIT. The fix moves the DURABLE records only. The LIVE registry the
/// executor consults is `EmbeddedDatabase::trigger_registry`, which the storage
/// layer has no handle on, so inside the renaming process the trigger is still
/// registered under the OLD name until the next open.
///
/// This asserts the CURRENT behaviour on purpose. If it starts failing because the
/// live registry learned to follow a rename, that is the completion of #85 —
/// delete this test, do not relax it, and update the note in
/// `Catalog::move_table_triggers`.
#[test]
fn renaming_a_table_does_not_deregister_the_live_trigger_yet() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory db");
    setup(&db);
    db.execute("ALTER TABLE ren_a RENAME TO ren_b").expect("rename");

    assert!(
        db.trigger_registry.has_triggers_for_table("ren_a"),
        "the live registry no longer keys the trigger under the old name — if the in-memory \
         half of #85 has been implemented, delete this test rather than relaxing it"
    );
    assert!(
        !db.trigger_registry.has_triggers_for_table("ren_b"),
        "the live registry now keys the trigger under the new name — the in-memory half of \
         #85 appears to be implemented; delete this test"
    );

    // The durable half, which IS fixed, is what makes the next open correct.
    assert_eq!(persisted_trigger_tables(&db), vec!["ren_b".to_string()]);
}
