//! Actual branch metadata writes must never alias main's layout identity.
use super::*;
use crate::{Config, EmbeddedDatabase, Result, Schema, Value};

fn database(history: bool) -> Result<EmbeddedDatabase> {
    let mut config = Config::in_memory();
    config.storage.time_travel_enabled = history;
    let db = EmbeddedDatabase::with_config(config)?;
    db.execute("CREATE TABLE other(id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO other VALUES(7,70)")?;
    db.execute("CREATE TABLE d(id INT PRIMARY KEY, v INT)")?;
    db.execute("INSERT INTO d VALUES(1,10)")?;
    Ok(db)
}

fn branch_statement(db: &EmbeddedDatabase, branch: &str, sql: &str) -> Result<u64> {
    db.with_request_branch_scope(branch, || db.execute(sql))
}

fn schema_stamp(db: &EmbeddedDatabase, branch: &str) -> Result<(Schema, LayoutStamp)> {
    db.with_request_branch_scope(branch, || {
        let (schema, stamp) = collect_stamp(|| db.storage.catalog().get_table_schema("d"));
        Ok((schema?, stamp))
    })
}

#[test]
fn reclaiming_a_frozen_other_definition_leaves_main_other_epoch_unchanged() -> Result<()> {
    for history in [false, true] {
        let db = database(history)?;
        db.create_branch("r")?;
        branch_statement(&db, "r", "INSERT INTO d VALUES(2,20)")?;
        // The real main DDL freezes r's definition before it publishes its index.
        db.execute("CREATE INDEX other_v ON other(v)")?;
        let level = db.storage.get_branch("r")?.branch_id;
        assert!(db.storage.branch_table_record(level, "other")?.is_some());
        let raw = db.storage.db();
        let schema_before = raw.get(b"meta:table:other")?;
        let index_before = raw.get(b"meta:index:other_v")?;
        assert!(schema_before.is_some() && index_before.is_some());
        let main_before = db.storage.layout_epochs().current("other");
        db.execute("MERGE BRANCH r INTO main")?;
        assert!(raw
            .get(super::super::branch_catalog::branch_table_key(level, "other"))?
            .is_none());
        assert_eq!(raw.get(b"meta:table:other")?, schema_before);
        assert_eq!(raw.get(b"meta:index:other_v")?, index_before);
        assert_eq!(db.storage.scan_table("other")?.len(), 1);
        assert_eq!(db.storage.scan_table("d")?.len(), 2);
        assert_eq!(
            db.storage.layout_epochs().current("other"),
            main_before,
            "reclaiming a branch's frozen other record cannot alter MAIN other"
        );
        assert_eq!(
            db.query("SELECT id FROM other WHERE v=70", &[])?
                .first()
                .map(|row| row.values.as_slice()),
            Some([Value::Int4(7)].as_slice())
        );
        eprintln!("branch-layout-identity-cell reclaim history={history}");
    }
    Ok(())
}

#[test]
fn branch_plan_dependencies_distinguish_own_ancestor_nearer_and_sibling_levels() -> Result<()> {
    for history in [false, true] {
        for mode in ["own", "ancestor", "nearer", "sibling"] {
            let db = database(history)?;
            db.create_branch("a")?;
            branch_statement(&db, "a", "ALTER TABLE d ADD COLUMN a_col INT DEFAULT 3")?;
            db.with_request_branch_scope("a", || db.create_branch("b"))?;
            db.with_request_branch_scope("a", || db.create_branch("c"))?;
            let observed = if mode == "own" { "a" } else { "b" };
            let (schema_before, stamp) = schema_stamp(&db, observed)?;
            assert!(stamp.is_current());
            let main_before = db.storage.layout_epochs().current("d");
            let changed = match mode {
                "own" | "ancestor" => "a",
                "nearer" => "b",
                _ => "c",
            };
            // A real second-thread DDL occurs after planning, before this plan
            // can be reused. No manufactured epoch bump or error is injected.
            std::thread::scope(|scope| {
                scope
                    .spawn(|| branch_statement(&db, changed, "ALTER TABLE d ADD COLUMN changed_col INT DEFAULT 9"))
                    .join()
                    .map_err(|_| crate::Error::storage("branch layout DDL worker panicked"))?
            })?;
            assert_eq!(
                db.storage.layout_epochs().current("d"),
                main_before,
                "branch-only DDL, mode={mode}"
            );
            let (fresh, fresh_stamp) = schema_stamp(&db, observed)?;
            assert!(fresh_stamp.is_current());
            if mode == "sibling" {
                assert!(stamp.is_current(), "a sibling's metadata cannot invalidate this plan");
                assert_eq!(fresh.columns.len(), schema_before.columns.len());
            } else {
                assert!(!stamp.is_current(), "the actual branch resolution changed, mode={mode}");
                assert_eq!(
                    fresh.columns.len(),
                    schema_before.columns.len() + usize::from(mode != "ancestor"),
                    "ancestor DDL freezes this child's old schema; own/nearer DDL adds its column"
                );
            }
            eprintln!("branch-layout-identity-cell plan history={history} mode={mode}");
        }
    }
    Ok(())
}

#[test]
fn branch_metadata_ranges_fence_old_and_mid_publication_plans_and_reload_cached_records() -> Result<()> {
    for history in [false, true] {
        for all_branches in [false, true] {
            let db = database(history)?;
            db.create_branch("a")?;
            db.create_branch("c")?;
            branch_statement(&db, "a", "ALTER TABLE d ADD COLUMN a_col INT DEFAULT 3")?;
            branch_statement(&db, "c", "ALTER TABLE d ADD COLUMN c_col INT DEFAULT 4")?;
            let level = db.storage.get_branch("a")?.branch_id;
            let (old_a, a_stamp) = schema_stamp(&db, "a")?;
            let (old_c, c_stamp) = schema_stamp(&db, "c")?;
            assert_eq!(old_a.columns.len(), 3);
            assert_eq!(old_c.columns.len(), 3);
            assert!(a_stamp.is_current() && c_stamp.is_current());
            let main_before = db.storage.layout_epochs().current("d");
            let raw = db.storage.db();
            let sequence = raw.latest_sequence_number();
            let (start, end) = if all_branches {
                ("bmeta:".to_string(), "bmeta;".to_string())
            } else {
                (format!("bmeta:{level}:"), format!("bmeta:{level};"))
            };
            let mut batch = rocksdb::WriteBatch::default();
            batch.delete_range(start.as_bytes(), end.as_bytes());
            // An actual native range write, with the same publication lifetime
            // as a real DDL owner. No synthetic epoch or injected error.
            let publication = PublicationScope::enter();
            db.storage.write_gate().write(raw.as_ref(), batch, None)?;
            assert_eq!(raw.latest_sequence_number(), sequence + 1);
            assert!(raw
                .get(super::super::branch_catalog::branch_table_key(level, "d"))?
                .is_none());
            let (_, during) = std::thread::scope(|scope| {
                scope
                    .spawn(|| schema_stamp(&db, "a"))
                    .join()
                    .map_err(|_| crate::Error::storage("range-publication reader panicked"))?
            })?;
            assert!(
                !during.is_current(),
                "new reader cannot accept an in-flight range publication"
            );
            drop(publication);
            assert!(
                !a_stamp.is_current() && !c_stamp.is_current(),
                "unattributable range has a branch-wide fence"
            );
            assert!(
                !during.is_current(),
                "an in-flight sampled epoch never becomes a valid plan"
            );
            let (fresh_a, fresh_a_stamp) = schema_stamp(&db, "a")?;
            let (fresh_c, fresh_c_stamp) = schema_stamp(&db, "c")?;
            assert_eq!(
                fresh_a.columns.len(),
                2,
                "cached removed branch record must reload main"
            );
            assert_eq!(fresh_c.columns.len(), if all_branches { 2 } else { 3 });
            assert!(fresh_a_stamp.is_current() && fresh_c_stamp.is_current());
            assert_eq!(db.storage.layout_epochs().current("d"), main_before);
            assert_eq!(db.storage.scan_table("d")?.len(), 1);
            eprintln!("branch-layout-identity-cell range history={history} all_branches={all_branches}");
        }
    }
    Ok(())
}
