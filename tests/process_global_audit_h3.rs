//! Process-global `static` audit — two-database sharing probes.
//!
//! Every test here has the same shape, because it is the shape that has
//! already caught this class of defect: construct TWO `EmbeddedDatabase`
//! instances in one process and assert that state belonging to one of them
//! cannot be read, overwritten or fired by the other.
//!
//! The rule these probes defend is written down in `src/lib.rs` ("DESIGN RULE
//! — what a process-global `static` may hold"), next to the thread-local
//! block that is the sanctioned alternative.

use heliosdb_nano::EmbeddedDatabase;

/// The identity every per-engine lookup key is built from.
///
/// Two open databases must never report the same id, or every table keyed by
/// it collapses back into the shared-state bug it was introduced to fix. The
/// id is a monotonic counter and not a pointer precisely so that an engine
/// dropped and replaced at the same allocation cannot inherit the earlier
/// engine's entries.
#[test]
fn two_databases_have_distinct_instance_ids() {
    let a = EmbeddedDatabase::new_in_memory().expect("database a");
    let b = EmbeddedDatabase::new_in_memory().expect("database b");

    assert_ne!(
        a.storage.instance_id(),
        b.storage.instance_id(),
        "two open databases in one process must have distinct instance ids"
    );
    // Stable for the life of the engine (two separate reads, so `clippy::eq_op`
    // does not see one expression compared with itself).
    let first_read = a.storage.instance_id();
    let second_read = a.storage.instance_id();
    assert_eq!(
        first_read, second_read,
        "an instance id is stable for the life of the engine"
    );
}

/// A third database opened after two others still gets its own id — the
/// counter is never reset and never recycled.
#[test]
fn instance_ids_are_not_recycled_after_a_database_is_dropped() {
    let first_id = {
        let a = EmbeddedDatabase::new_in_memory().expect("database a");
        a.storage.instance_id()
    }; // `a` is dropped here; its allocation may well be reused.

    let b = EmbeddedDatabase::new_in_memory().expect("database b");
    assert_ne!(
        first_id,
        b.storage.instance_id(),
        "a dropped database's id must never be handed to a later database"
    );
}

/// The MCP tool-result cache is a process-global LRU, and `call_tool` takes
/// the database as a PARAMETER — so one process can ask it about several
/// databases. Keyed by `(tool, args)` alone it answered database B's
/// `heliosdb_list_tables` with database A's tables for the next five minutes.
#[cfg(feature = "mcp-endpoint")]
#[test]
fn mcp_result_cache_does_not_serve_one_database_from_another() {
    use heliosdb_nano::mcp::{result_cache, tools::call_tool};
    use serde_json::json;

    result_cache::_clear_for_tests();

    let a = EmbeddedDatabase::new_in_memory().expect("database a");
    a.execute("CREATE TABLE alpha_only (id INT4 PRIMARY KEY)")
        .expect("create alpha_only");

    let b = EmbeddedDatabase::new_in_memory().expect("database b");
    b.execute("CREATE TABLE beta_only (id INT4 PRIMARY KEY)")
        .expect("create beta_only");

    // Identical tool, identical arguments — only the database differs.
    let args = json!({});

    let from_a = call_tool(Some(&a), "heliosdb_list_tables", args.clone());
    assert!(!from_a.is_error, "{:?}", from_a.payload);
    let a_tables = from_a.payload["tables"].to_string();
    assert!(
        a_tables.contains("alpha_only"),
        "database a lists its own table: {a_tables}"
    );

    let from_b = call_tool(Some(&b), "heliosdb_list_tables", args);
    assert!(!from_b.is_error, "{:?}", from_b.payload);
    let b_tables = from_b.payload["tables"].to_string();
    assert!(
        b_tables.contains("beta_only"),
        "database b must be answered from its OWN catalog, got {b_tables}"
    );
    assert!(
        !b_tables.contains("alpha_only"),
        "database b must not be served database a's cached tool result, got {b_tables}"
    );
}

/// An AST index names a table in ONE database. Keyed by index name alone, the
/// registry let a second database's declaration replace the first's, and made
/// the auto-reparse hook fire for every open database that wrote to a
/// same-named table — including databases that had declared no index at all.
#[cfg(feature = "code-graph")]
#[test]
fn ast_index_registry_is_per_database() {
    use heliosdb_nano::code_graph::{storage as cg, AstIndexMeta};

    let a = EmbeddedDatabase::new_in_memory().expect("database a");
    let b = EmbeddedDatabase::new_in_memory().expect("database b");
    let (a_id, b_id) = (a.storage.instance_id(), b.storage.instance_id());

    let meta = |endpoint: &str| AstIndexMeta {
        index_name: "shared_name".to_string(),
        table: "docs".to_string(),
        content_col: "body".to_string(),
        lang_col: None,
        embed_endpoint: Some(endpoint.to_string()),
        embed_bearer: None,
        embed_bodies: false,
        auto_reparse: true,
        resolve_cross_file: false,
        paused: false,
    };

    // Same index name, same source table, two different databases.
    cg::register_ast_index(a_id, meta("http://a.invalid"));

    // Before b declares anything: b's DML on `docs` must trigger nothing.
    assert!(
        cg::ast_indexes_for_table(b_id, "docs").is_empty(),
        "database b must not auto-reparse because database a declared an index"
    );

    cg::register_ast_index(b_id, meta("http://b.invalid"));

    // Neither registration clobbered the other.
    assert_eq!(
        cg::get_ast_index(a_id, "shared_name")
            .expect("database a keeps its declaration")
            .embed_endpoint
            .as_deref(),
        Some("http://a.invalid")
    );
    assert_eq!(
        cg::get_ast_index(b_id, "shared_name")
            .expect("database b keeps its declaration")
            .embed_endpoint
            .as_deref(),
        Some("http://b.invalid")
    );

    // Pausing one database's index leaves the other running.
    assert!(cg::set_ast_index_paused(a_id, "shared_name", true));
    assert!(
        cg::ast_indexes_for_table(a_id, "docs").is_empty(),
        "database a's index is paused"
    );
    assert_eq!(
        cg::ast_indexes_for_table(b_id, "docs").len(),
        1,
        "database b's index must be unaffected by database a's pause"
    );
}

/// `User::new` and `User::new_passwordless` used to declare a function-scope
/// `static COUNTER` each, so the first user minted through each constructor
/// both got `UserId(1)`. `SessionManager::get_user_sessions` and
/// `enforce_quota` key on that id, so the two users would have shared one
/// session list and one resource quota.
#[test]
fn user_ids_are_unique_across_both_constructors() {
    use heliosdb_nano::session::User;

    let with_password = User::new("audit-h3-with-password", "s3cret");
    let without_password = User::new_passwordless("audit-h3-without-password");

    assert_ne!(
        with_password.id, without_password.id,
        "both User constructors must draw from ONE id counter"
    );
}
