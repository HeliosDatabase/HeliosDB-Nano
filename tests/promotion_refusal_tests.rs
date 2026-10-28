//! Truthful refusal of the reachable SQL switchover placeholder.
//!
//! `SWITCHOVER TO '<uuid>'` used to return a success-shaped status line
//! ("Switchover to node … initiated") from `ddl::handle_switchover` even though
//! no switchover/promotion runtime exists. This test drives the real SQL entry
//! point (`EmbeddedDatabase::execute`) through the same text-family plan path a
//! client uses, with the primary + standby preconditions satisfied, and pins
//! that the statement is refused and that the refusal changes nothing.
//!
//! `SWITCHOVER CHECK '<uuid>'` is the read-only sibling. It must run as real
//! SQL and leave role, read-only, rows and catalog unchanged too. This is an
//! embedded regression, not a live promotion acceptance test.
//!
//! # Why this is its own integration-test target
//!
//! `ha_state()` is a process-wide singleton. A test that sets the role to
//! Primary and registers a standby must not race its neighbours in a shared
//! test binary, so — exactly like `tests/txn_replication_sync.rs`, and for the
//! reasons recorded there — this file is a separate target with its own
//! process. It restores the HA global before it asserts. No port is bound.

#![cfg(feature = "ha-tier1")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use heliosdb_nano::replication::ha_state::{ha_state, HARole, NodeConfig, StandbyRegistration, StandbyState, SyncMode};
use heliosdb_nano::sql::{Executor, LogicalPlan, Parser};
use heliosdb_nano::{DataType, EmbeddedDatabase, Tuple, Value};
use uuid::Uuid;

const TABLE: &str = "promotion_refusal_guard";

/// A connected standby as a replication session registers it from its
/// `Hello` (spec P2.1); it stays registered while the handle lives.
fn mock_standby(node_id: Uuid) -> StandbyRegistration {
    StandbyRegistration {
        node_id,
        address: "127.0.0.1:0".to_string(),
        history: Some(Uuid::new_v4()),
        pg_port: 0,
        sync_mode: SyncMode::Async,
        cursor: Some(0),
        state: StandbyState::Streaming,
        counts_for_sync: true,
    }
}

#[test]
fn switchover_refuses_and_changes_nothing() {
    let db = EmbeddedDatabase::new_in_memory().expect("in-memory database");
    db.execute(&format!("CREATE TABLE {TABLE} (id INT PRIMARY KEY, v TEXT)"))
        .expect("create table");
    db.execute(&format!("INSERT INTO {TABLE} (id, v) VALUES (1, 'before')"))
        .expect("seed row");

    let select = format!("SELECT id, v FROM {TABLE} ORDER BY id");
    let rows_before: Vec<Tuple> = db.query(&select, &[]).expect("rows before");
    assert_eq!(rows_before.len(), 1, "fixture row");

    let catalog_sql = format!("SELECT table_name FROM information_schema.tables WHERE table_name = '{TABLE}'");
    let catalog_before: Vec<Tuple> = db.query(&catalog_sql, &[]).expect("catalog before");
    assert_eq!(catalog_before.len(), 1, "fixture table in catalog");

    // Private HA fixture: this node is a primary with one connected standby.
    // Both preconditions the old placeholder checked are satisfied, so a pass
    // cannot come from missing standby prevalidation.
    let standby_id = Uuid::new_v4();
    ha_state().set_config(NodeConfig {
        role: HARole::Primary,
        ..NodeConfig::default()
    });
    let standby = ha_state().register_standby(mock_standby(standby_id));
    assert_eq!(ha_state().get_role(), HARole::Primary);
    assert!(!ha_state().is_read_only());

    let refusal = db.execute(&format!("SWITCHOVER TO '{standby_id}'"));

    // SWITCHOVER CHECK is the read-only status sibling and must run as real SQL
    // (a parse/validation failure is an Err, not Ok(1)) while mutating nothing.
    // The embedded execute() entry point returns only the affected-row count;
    // the row values are observed directly below.
    let checked = db.execute(&format!("SWITCHOVER CHECK '{standby_id}'"));

    // Directly observe the CHECK row through the public plan-level Executor API.
    // The embedded `execute()` above returns only the affected-row count, so the
    // field values need a plan execute. This re-parses the identical SQL text and
    // runs the same handler through an Executor bound to this database's storage.
    let check_target = Parser::parse_switchover_check_sql(&format!("SWITCHOVER CHECK '{standby_id}'"))
        .expect("parse SWITCHOVER CHECK");
    let check_plan = LogicalPlan::SwitchoverCheck {
        target_node: check_target,
    };
    let mut check_executor = Executor::with_storage(&db.storage);
    let (check_rows, check_schema) = check_executor
        .execute_with_schema(&check_plan)
        .expect("execute SWITCHOVER CHECK plan");

    // Capture everything neither statement may have touched, then restore the
    // global HA state BEFORE asserting: a failure here must not leak
    // Primary + a phantom standby into the rest of this process.
    let role_after = ha_state().get_role();
    let read_only_after = ha_state().is_read_only();
    let rows_after: Vec<Tuple> = db.query(&select, &[]).expect("rows after");
    let catalog_after: Vec<Tuple> = db.query(&catalog_sql, &[]).expect("catalog after");

    drop(standby);
    ha_state().set_config(NodeConfig::default());

    let err = refusal.expect_err("SWITCHOVER must be refused, not reported as initiated");
    let message = err.to_string();
    assert!(
        message.contains("unsupported") && message.contains("fencing") && message.contains("no role was changed"),
        "refusal must be an actionable sentence naming unsupported, fencing and no role change, got: {message}"
    );
    // Spec P5.1 §7: the refusal points at the promotion that exists (offline)
    // and the one that follows (P5.2), and claims neither ran.
    assert!(
        message.contains("heliosdb-nano promote --data-dir") && message.contains("pg_promote(true, N)"),
        "the refusal names `heliosdb-nano promote` and `pg_promote` (P5.2), got: {message}"
    );

    assert_eq!(
        checked.expect("SWITCHOVER CHECK must execute as real SQL, not fail to parse or validate"),
        1,
        "SWITCHOVER CHECK returns exactly one check row"
    );

    assert_eq!(role_after, HARole::Primary, "HA role must be unchanged");
    assert!(!read_only_after, "read-only state must be unchanged");
    assert_eq!(rows_after, rows_before, "actual rows must be unchanged");
    assert_eq!(catalog_after, catalog_before, "catalog must be unchanged");

    // Preserved schema: seven columns, same names and types.
    let check_names: Vec<&str> = check_schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        check_names,
        vec![
            "can_proceed",
            "target_healthy",
            "target_lsn",
            "primary_lsn",
            "lag_bytes",
            "warnings",
            "blockers",
        ],
        "SWITCHOVER CHECK must preserve its seven column names"
    );
    let check_types: Vec<&DataType> = check_schema.columns.iter().map(|c| &c.data_type).collect();
    assert_eq!(
        check_types,
        vec![
            &DataType::Boolean,
            &DataType::Boolean,
            &DataType::Int8,
            &DataType::Int8,
            &DataType::Int8,
            &DataType::Text,
            &DataType::Text,
        ],
        "SWITCHOVER CHECK must preserve its column types"
    );

    assert_eq!(check_rows.len(), 1, "SWITCHOVER CHECK returns exactly one row");
    let check_row = &check_rows[0];
    assert_eq!(
        check_row.get(0),
        Some(&Value::Boolean(false)),
        "can_proceed must be false"
    );
    assert_eq!(
        check_row.get(1),
        Some(&Value::Boolean(true)),
        "target_healthy must stay true for a connected standby"
    );
    match check_row.get(6) {
        Some(Value::String(blockers)) => {
            assert!(
                blockers.contains("Runtime switchover/promotion is unsupported"),
                "blockers must name the unsupported runtime, got: {blockers}"
            );
            // Spec P5.1 §7.
            assert!(
                blockers.contains("use `heliosdb-nano promote` (offline) or `SELECT pg_promote(true, N)` (P5.2)"),
                "blockers must name the offline promotion and P5.2's pg_promote, got: {blockers}"
            );
        }
        other => panic!("blockers must be a String, got: {other:?}"),
    }

    // Spec P5.1 §7, §11.I: `pg_promote` is NOT defined in P5.1 (P5.2 defines
    // it): an UNDEFINED function — the error the wire answers 42883
    // undefined_function (it names a function that is not found or unknown;
    // `tests/physical_promote.rs` asserts the SQLSTATE on a real daemon) —
    // never another failure that would hide a half-defined one.
    let missing = db
        .query("SELECT pg_promote(true, 5)", &[])
        .expect_err("pg_promote must stay undefined until P5.2");
    let lower = missing.to_string().to_lowercase();
    assert!(
        lower.contains("pg_promote")
            && lower.contains("function")
            && (lower.contains("not found") || lower.contains("does not exist") || lower.contains("unknown")),
        "pg_promote must fail as an undefined function (42883): {missing}"
    );
}
