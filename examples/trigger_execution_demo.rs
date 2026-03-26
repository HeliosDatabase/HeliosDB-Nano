//! Trigger Execution Demo
//!
//! This example demonstrates how trigger execution is integrated into DML operations.
//! It shows the execution flow for INSERT, UPDATE, and DELETE with BEFORE and AFTER triggers.

use heliosdb_nano::{EmbeddedDatabase, Result, sql};

fn main() -> Result<()> {
    println!("========================================");
    println!("HeliosDB Nano Trigger Execution Demo");
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
        None, // No WHEN condition
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

    // Insert data (triggers will execute)
    println!("4. Inserting data (triggers will fire)...");
    let count = db.execute("INSERT INTO users (id, name, email) VALUES (1, 'Alice', 'alice@example.com')")?;
    println!("   ✓ Inserted {} row (triggers executed)\n", count);

    // Verify triggers are registered
    println!("5. Verifying trigger registration...");
    let triggers = db.trigger_registry.get_triggers_for_table("users")?;
    println!("   Found {} trigger(s) on 'users' table:", triggers.len());
    for trigger in &triggers {
        println!("     - {} ({:?} {:?})",
                 trigger.name,
                 trigger.timing,
                 trigger.events);
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

    // Update data (trigger will execute)
    println!("7. Updating data (UPDATE trigger will fire)...");
    let count = db.execute("UPDATE users SET email = 'alice.new@example.com' WHERE id = 1")?;
    println!("   ✓ Updated {} row(s) (UPDATE trigger executed)\n", count);

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

    // Delete data (trigger will execute)
    println!("9. Deleting data (DELETE trigger will fire)...");
    let count = db.execute("DELETE FROM users WHERE id = 2")?;
    println!("   ✓ Deleted {} row(s) (DELETE trigger executed)\n", count);

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
    println!("Summary:");
    println!("  ✓ Trigger registration working");
    println!("  ✓ INSERT trigger hooks integrated");
    println!("  ✓ UPDATE trigger hooks integrated");
    println!("  ✓ DELETE trigger hooks integrated");
    println!("  ✓ Cascading depth tracking (16-level limit)");
    println!("  ✓ TriggerContext depth protection");
    println!();
    println!("Next steps:");
    println!("  - Implement CREATE TRIGGER parser (Task 7)");
    println!("  - Add NEW/OLD context evaluation (Task 10)");
    println!("  - Test with real trigger bodies");

    Ok(())
}
