//! Batch E — storage / transaction hygiene.
//!
//! Three sprinter items, all "the structure exists and nothing calls it" or
//! "the same state is derived twice and the two copies disagree":
//!
//! * **1b7141f517ce** — `Transaction` drew its id from TWO per-constructor
//!   statics, so the first embedded and the first session transaction in a
//!   process both got `transaction_id = 1`. The pairwise-collision test for
//!   that one lives in `src/storage/transaction.rs`'s unit-test module, NOT
//!   here: `Transaction::new_with_session` needs `StorageEngine::db`, which is
//!   `pub(crate)` and unreachable from an integration test.
//! * **29ecb34e3245** — `PredicatePushdownManager::remove_table` had zero
//!   callers, so a dropped table's bloom filters and zone maps outlived it.
//! * **4c1cf9054f0f** — a trigger body's derived-table alias stamp
//!   (`Project::source_alias`) is `#[serde(skip)]`, so it did not survive the
//!   bincode round-trip `Catalog::save_trigger` / `load_all_triggers` performs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use heliosdb_nano::sql::logical_plan::{
    TransitionTable, TriggerCharacteristics, TriggerEvent, TriggerFor, TriggerTiming, TriggerType,
};
use heliosdb_nano::sql::{LogicalExpr, LogicalPlan, TriggerDefinition};
use heliosdb_nano::{Column, DataType, EmbeddedDatabase, Schema, Value};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn memory_db() -> EmbeddedDatabase {
    EmbeddedDatabase::new_in_memory().expect("in-memory database")
}

/// A unique scratch directory for a reopen test.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nano_batch_e_{tag}_{id}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

// ===========================================================================
// sprinter 29ecb34e3245 — DROP TABLE must purge the pushdown structures
// ===========================================================================

/// Register bloom filters + zone maps for `table` the way the public engine
/// API does, and index `rows` values into them.
///
/// Nothing in the engine builds these structures on its own today (there is no
/// caller of `initialize_table` / `register_bloom_filters` on the write path),
/// so the state `remove_table` exists to clean up has to be created
/// explicitly — which is also exactly the state an operator using the public
/// `StorageEngine::register_bloom_filters` / `register_zone_maps` API has.
fn build_pushdown_structures(db: &EmbeddedDatabase, table: &str, ids: &[i32], status: &str) {
    let pushdown = db.storage.predicate_pushdown();
    pushdown.initialize_table(table, &["id".to_string(), "status".to_string()], 64);
    for &id in ids {
        pushdown.index_row(
            table,
            id as u64,
            &[
                ("id".to_string(), Value::Int4(id)),
                ("status".to_string(), Value::String(status.to_string())),
            ],
        );
    }
}

#[test]
fn drop_table_purges_predicate_pushdown_structures() {
    let db = memory_db();

    db.execute("CREATE TABLE pd_t (id INT PRIMARY KEY, status TEXT)")
        .expect("create");
    for id in 0..20i32 {
        db.execute(&format!("INSERT INTO pd_t (id, status) VALUES ({id}, 'open')"))
            .expect("insert");
    }
    build_pushdown_structures(&db, "pd_t", &(0..20).collect::<Vec<_>>(), "open");

    assert!(
        db.storage.predicate_pushdown().has_structures_for_table("pd_t"),
        "the test must actually have built the structures it is about to check the drop removes"
    );

    db.execute("DROP TABLE pd_t").expect("drop");

    // The direct assertion: `remove_table` really was invoked.
    assert!(
        !db.storage.predicate_pushdown().has_structures_for_table("pd_t"),
        "DROP TABLE must purge the dropped table's bloom filters and zone maps \
         (sprinter 29ecb34e3245: `remove_table` had no callers)"
    );

    // And the consequence that made this a correctness bug rather than a leak:
    // the name is reused by a table whose rows are ENTIRELY OUTSIDE the old
    // bloom filter / zone map, so a surviving structure would prune them away.
    db.execute("CREATE TABLE pd_t (id INT PRIMARY KEY, status TEXT)")
        .expect("recreate");
    db.execute("INSERT INTO pd_t (id, status) VALUES (900, 'archived')")
        .expect("insert into recreated table");

    let rows = db
        .query("SELECT id FROM pd_t WHERE status = 'archived'", &[])
        .expect("query the recreated table");
    assert_eq!(
        rows.len(),
        1,
        "the recreated table's row must be returned, not pruned by the dropped table's stale filters"
    );
    assert_eq!(rows[0].values.first(), Some(&Value::Int4(900)));

    let all = db.query("SELECT id FROM pd_t", &[]).expect("scan the recreated table");
    assert_eq!(all.len(), 1, "the recreated table holds exactly its own one row");
}

/// Same purge on the other statement that invalidates every summarised row.
/// TRUNCATE keeps the table, so a surviving zone map would go on describing the
/// pre-truncate contents and could prune rows inserted afterwards.
#[test]
fn truncate_purges_predicate_pushdown_structures() {
    let db = memory_db();

    db.execute("CREATE TABLE pd_trunc (id INT PRIMARY KEY, status TEXT)")
        .expect("create");
    for id in 0..10i32 {
        db.execute(&format!("INSERT INTO pd_trunc (id, status) VALUES ({id}, 'open')"))
            .expect("insert");
    }
    build_pushdown_structures(&db, "pd_trunc", &(0..10).collect::<Vec<_>>(), "open");
    assert!(db.storage.predicate_pushdown().has_structures_for_table("pd_trunc"));

    db.execute("TRUNCATE TABLE pd_trunc").expect("truncate");

    assert!(
        !db.storage.predicate_pushdown().has_structures_for_table("pd_trunc"),
        "TRUNCATE must purge the summaries of the rows it deleted"
    );

    db.execute("INSERT INTO pd_trunc (id, status) VALUES (900, 'archived')")
        .expect("insert after truncate");
    let rows = db
        .query("SELECT id FROM pd_trunc WHERE status = 'archived'", &[])
        .expect("query after truncate");
    assert_eq!(
        rows.len(),
        1,
        "a post-TRUNCATE row must not be pruned by a stale summary"
    );
}

// ===========================================================================
// sprinter 4c1cf9054f0f — a trigger body's derived-table alias stamp
// ===========================================================================

fn int_column(name: &str) -> Column {
    Column::new(name, DataType::Int4)
}

fn col(table: Option<&str>, name: &str) -> LogicalExpr {
    LogicalExpr::Column {
        table: table.map(str::to_string),
        name: name.to_string(),
    }
}

/// The plan for `INSERT INTO log SELECT s.id FROM (SELECT id FROM t) s`,
/// reduced to the part that matters: an outer projection whose only expression
/// is the QUALIFIED reference `s.id`, over an inner projection STAMPED with the
/// derived-table alias `s`.
///
/// Built by hand rather than through the planner because the SQL route cannot
/// produce it: `Planner::create_trigger_to_plan` hardcodes `let body = vec![]`,
/// so a trigger's persisted body is always empty today (see the report note).
fn stamped_trigger_body() -> LogicalPlan {
    let scan = LogicalPlan::Scan {
        table_name: "t".to_string(),
        alias: None,
        schema: Arc::new(Schema {
            columns: vec![int_column("id")],
        }),
        projection: None,
        as_of: None,
    };
    let derived = LogicalPlan::Project {
        input: Box::new(scan),
        exprs: vec![col(None, "id")],
        aliases: vec!["id".to_string()],
        distinct: false,
        distinct_on: None,
        source_alias: Some("s".to_string()),
    };
    LogicalPlan::Project {
        input: Box::new(derived),
        exprs: vec![col(Some("s"), "id")],
        aliases: vec!["id".to_string()],
        distinct: false,
        distinct_on: None,
        source_alias: None,
    }
}

/// Does any `Project` in this plan still carry a `source_alias` stamp?
fn has_stamp(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Project {
            source_alias: Some(_), ..
        } => true,
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Aggregate { input, .. } => has_stamp(input),
        _ => false,
    }
}

fn trigger_definition(name: &str, body: Vec<LogicalPlan>) -> TriggerDefinition {
    TriggerDefinition {
        name: name.to_string(),
        table_name: "t".to_string(),
        timing: TriggerTiming::After,
        events: vec![TriggerEvent::Insert],
        for_each: TriggerFor::Row,
        when_condition: None,
        body,
        enabled: true,
        created_at: 0,
        referencing: Vec::<TransitionTable>::new(),
        characteristics: TriggerCharacteristics::default(),
        trigger_type: TriggerType::Regular,
        from_constraint: None,
    }
}

#[test]
fn trigger_with_derived_table_alias_survives_restart() {
    let dir = scratch_dir("trigger_destamp");
    let stamped = stamped_trigger_body();
    assert!(has_stamp(&stamped), "the fixture must carry the stamp");

    // --- The mechanism. `Project::source_alias` is `#[serde(skip)]`, so the
    // plan that comes back out of the catalog is NOT the plan that went in:
    // the qualified `s.id` survives, the stamp it resolves through does not,
    // and re-executing it is what produced "Column s.id not found".
    let raw_bytes = bincode::serialize(&stamped).expect("serialize");
    let raw_reloaded: LogicalPlan = bincode::deserialize(&raw_bytes).expect("deserialize");
    assert!(!has_stamp(&raw_reloaded), "the stamp cannot survive persistence");
    assert_ne!(
        raw_reloaded, stamped,
        "an un-destamped trigger body does not round-trip"
    );

    // --- The fix. `execute_create_trigger_plan` now runs the same transform
    // CREATE MATERIALIZED VIEW runs before serializing its plan, so the body
    // that reaches `save_trigger` needs no stamp at all: `s.id` has become the
    // bare `id`, and the plan round-trips byte-identically.
    let destamped = stamped.clone().destamp_source_aliases().expect("destamp accepts");
    assert!(!has_stamp(&destamped), "the transform clears every stamp");
    let LogicalPlan::Project { exprs, .. } = &destamped else {
        panic!("root must still be a Project");
    };
    assert_eq!(exprs[0], col(None, "id"), "`s.id` must have become the bare `id`");

    // --- End to end, through the real persistence funnel and a real reopen.
    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        db.execute("CREATE TABLE t (id INT PRIMARY KEY)").expect("create t");
        db.execute("CREATE TABLE trg_log (id INT)").expect("create trg_log");
        db.storage
            .catalog()
            .save_trigger(&trigger_definition("derived_alias_trg", vec![destamped.clone()]))
            .expect("persist the trigger definition");
    }
    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");
        let loaded = db.storage.catalog().load_all_triggers().expect("load");
        let def = loaded
            .iter()
            .find(|d| d.name == "derived_alias_trg")
            .expect("the trigger must come back after the reopen");
        assert_eq!(
            def.body,
            vec![destamped],
            "the reloaded body must be EXACTLY what was stored — no stamp was needed, \
             so none was lost (sprinter 4c1cf9054f0f)"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The reachable end of the same claim: a trigger created through SQL is
/// re-registered from the catalog at the next open. Bodies are still always
/// empty (`Planner::create_trigger_to_plan` hardcodes `vec![]`), so this pins
/// the persistence round-trip, not body execution.
#[test]
fn a_sql_created_trigger_is_reregistered_after_a_reopen() {
    let dir = scratch_dir("trigger_reopen");

    {
        let db = EmbeddedDatabase::new(&dir).expect("open");
        db.execute("CREATE TABLE trg_t (id INT, tag TEXT)").expect("create");
        db.execute("CREATE FUNCTION trg_fn() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql")
            .expect("create function");
        db.execute("CREATE TRIGGER trg BEFORE INSERT ON trg_t FOR EACH ROW EXECUTE FUNCTION trg_fn()")
            .expect("create trigger");
    }
    {
        let db = EmbeddedDatabase::new(&dir).expect("reopen");
        let triggers = db.trigger_registry.get_triggers_for_table("trg_t").expect("lookup");
        assert_eq!(triggers.len(), 1, "the trigger must be re-registered at open");
        assert_eq!(triggers[0].name, "trg");
        assert!(triggers[0].body.is_empty(), "bodies are still always empty");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
