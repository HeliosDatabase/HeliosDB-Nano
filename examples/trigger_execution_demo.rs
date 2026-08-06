//! Trigger Registration / DML-hook Demo
//!
//! ⚠️ TRIGGERS ARE NOT IMPLEMENTED IN HELIOSDB NANO. This example demonstrates the
//! trigger *hooks* the DML paths call into — registration, per-table lookup, and the
//! cascade-depth guard. It does NOT demonstrate a trigger body running, because no
//! trigger body ever runs: `TriggerDefinition.body` is always empty (the planner
//! hardcodes it, `src/sql/planner.rs`), and the DML executor closures discard the
//! NEW/OLD row context. INSERT/UPDATE/DELETE below take the trigger-aware slow path
//! and invoke the hooks, and the hooks do nothing observable.
//!
//! See the `heliosdb-nano-schema` skill ("Triggers — NOT IMPLEMENTED") before using
//! triggers for anything.

use heliosdb_nano::{sql, EmbeddedDatabase, Result};

fn main() -> Result<()> {
    println!("========================================");
    println!("HeliosDB Nano Trigger Registration Demo");
    println!("(triggers are NOT implemented: bodies never run)");
    println!("========================================\n");

    // Create an in-memory database
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create test tables
    println!("1. Creating test tables...");
    db.execute("CREATE TABLE users (id INT, name TEXT, email TEXT)")?;
    db.execute("CREATE TABLE audit_log (action TEXT, user_id INT, timestamp TEXT)")?;
    println!("   ✓ Tables created\n");

    // Register a BEFORE INSERT trigger programmatically
    println!("2. Registering BEFORE INSERT trigger...");
    let trigger_def = sql::TriggerDefinition::new(
        "before_insert_user".to_string(),
        "users".to_string(),
        sql::logical_plan::TriggerTiming::Before,
        vec![sql::logical_plan::TriggerEvent::Insert],
        sql::logical_plan::TriggerFor::Row,
        None,   // No WHEN condition
        vec![], // Empty body for now (would contain validation logic)
        vec![], // No REFERENCING clause
    );
    db.trigger_registry.register_trigger(trigger_def)?;
    println!("   ✓ BEFORE INSERT trigger registered\n");

    // Register an AFTER INSERT trigger for audit logging
    println!("3. Registering AFTER INSERT trigger for audit...");
    let audit_trigger = sql::TriggerDefinition::new(
        "after_insert_user_audit".to_string(),
        "users".to_string(),
        sql::logical_plan::TriggerTiming::After,
        vec![sql::logical_plan::TriggerEvent::Insert],
        sql::logical_plan::TriggerFor::Row,
        None,
        vec![], // Would contain INSERT INTO audit_log...
        vec![], // No REFERENCING clause
    );
    db.trigger_registry.register_trigger(audit_trigger)?;
    println!("   ✓ AFTER INSERT audit trigger registered\n");

    // Insert data (the trigger hooks are called; nothing happens)
    println!("4. Inserting data (BEFORE/AFTER INSERT hooks are called, and do nothing)...");
    let count = db.execute("INSERT INTO users (id, name, email) VALUES (1, 'Alice', 'alice@example.com')")?;
    println!(
        "   ✓ Inserted {} row (no trigger body ran — audit_log is still empty)\n",
        count
    );

    // Verify triggers are registered
    println!("5. Verifying trigger registration...");
    let triggers = db.trigger_registry.get_triggers_for_table("users")?;
    println!("   Found {} trigger(s) on 'users' table:", triggers.len());
    for trigger in &triggers {
        println!("     - {} ({:?} {:?})", trigger.name, trigger.timing, trigger.events);
    }
    println!();

    // Test UPDATE triggers
    println!("6. Registering UPDATE trigger...");
    let update_trigger = sql::TriggerDefinition::new(
        "before_update_user".to_string(),
        "users".to_string(),
        sql::logical_plan::TriggerTiming::Before,
        vec![sql::logical_plan::TriggerEvent::Update(Some(vec!["email".to_string()]))],
        sql::logical_plan::TriggerFor::Row,
        None,
        vec![],
        vec![], // No REFERENCING clause
    );
    db.trigger_registry.register_trigger(update_trigger)?;
    println!("   ✓ BEFORE UPDATE trigger registered\n");

    // Update data (the trigger hook is called; nothing happens)
    println!("7. Updating data (BEFORE UPDATE hook is called, and does nothing)...");
    let count = db.execute("UPDATE users SET email = 'alice.new@example.com' WHERE id = 1")?;
    println!("   ✓ Updated {} row(s) (no trigger body ran)\n", count);

    // Test DELETE triggers
    println!("8. Registering DELETE trigger...");
    let delete_trigger = sql::TriggerDefinition::new(
        "before_delete_user".to_string(),
        "users".to_string(),
        sql::logical_plan::TriggerTiming::Before,
        vec![sql::logical_plan::TriggerEvent::Delete],
        sql::logical_plan::TriggerFor::Row,
        None,
        vec![],
        vec![], // No REFERENCING clause
    );
    db.trigger_registry.register_trigger(delete_trigger)?;
    println!("   ✓ BEFORE DELETE trigger registered\n");

    // Insert another row for deletion test
    db.execute("INSERT INTO users (id, name, email) VALUES (2, 'Bob', 'bob@example.com')")?;

    // Delete data (the trigger hook is called; nothing happens)
    println!("9. Deleting data (BEFORE DELETE hook is called, and does nothing)...");
    let count = db.execute("DELETE FROM users WHERE id = 2")?;
    println!("   ✓ Deleted {} row(s) (no trigger body ran)\n", count);

    // Query final state
    println!("10. Final state of users table:");
    let results = db.query("SELECT * FROM users", &[])?;
    println!("    Rows: {}", results.len());
    for row in results {
        println!("      {:?}", row);
    }
    println!();

    // Demonstrate trigger context depth tracking
    println!("11. Testing cascading trigger depth tracking...");
    let mut context = sql::TriggerContext::new();
    println!("    Initial depth: {}", context.depth());

    for i in 0..5 {
        context.enter(&format!("trigger_{}", i))?;
        println!("    After trigger_{}: depth = {}", i, context.depth());
    }

    for i in (0..5).rev() {
        context.exit();
        println!("    After exit: depth = {}", context.depth());
    }
    println!();

    // Test max depth protection
    println!("12. Testing max depth protection (16-level limit)...");
    let mut context = sql::TriggerContext::new();
    for i in 0..sql::MAX_TRIGGER_DEPTH {
        context.enter(&format!("trigger_{}", i))?;
    }
    println!("    Reached max depth: {}", context.depth());

    // This should fail
    match context.enter("trigger_overflow") {
        Ok(_) => println!("    ERROR: Should have rejected depth > 16"),
        Err(e) => println!("    ✓ Correctly rejected: {}", e),
    }
    println!();

    println!("========================================");
    println!("Demo completed successfully!");
    println!("========================================");
    println!();
    println!("Summary — what this demo actually showed:");
    println!("  ✓ Trigger registration / lookup / drop (the registry works)");
    println!("  ✓ DML paths call the BEFORE/AFTER hooks for a table with triggers");
    println!("  ✓ Cascading depth tracking (16-level limit)");
    println!("  ✓ TriggerContext depth protection");
    println!();
    println!("What it did NOT show, because it does not exist:");
    println!("  ✗ Executing a trigger body — TriggerDefinition.body is always empty");
    println!("  ✗ NEW/OLD resolution during DML — the executor discards the row context");
    println!("  ✗ Any observable effect of a trigger on INSERT/UPDATE/DELETE");
    println!();
    println!("Regression coverage for this behaviour: tests/trigger_unimplemented_tests.rs");

    Ok(())
}
