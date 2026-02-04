//! TriggerRegistry Example
//!
//! Demonstrates how to use the TriggerRegistry for managing trigger definitions
//! in HeliosDB-Lite.

use heliosdb_lite::{
    EmbeddedDatabase, Result, Value,
    sql::{
        TriggerRegistry, TriggerDefinition, TriggerContext,
        logical_plan::{TriggerTiming, TriggerEvent, TriggerFor, LogicalPlan},
    },
};

fn main() -> Result<()> {
    // Create an in-memory database
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create a test table
    db.execute("CREATE TABLE users (id INT, name TEXT, email TEXT, updated_at TEXT)")?;

    println!("=== TriggerRegistry Example ===\n");

    // Access the trigger registry
    let trigger_registry = db.storage.trigger_registry();

    // Example 1: Create a BEFORE INSERT trigger
    println!("1. Creating BEFORE INSERT trigger for audit logging...");
    let audit_trigger = TriggerDefinition::new(
        "audit_insert".to_string(),
        "users".to_string(),
        TriggerTiming::Before,
        vec![TriggerEvent::Insert],
        TriggerFor::Row,
        None, // No WHEN condition
        vec![], // Empty body for demonstration
        vec![], // No REFERENCING clause
    );

    trigger_registry.register_trigger(audit_trigger)?;
    println!("   ✓ Trigger 'audit_insert' registered\n");

    // Example 2: Create an AFTER UPDATE trigger
    println!("2. Creating AFTER UPDATE trigger for timestamp tracking...");
    let timestamp_trigger = TriggerDefinition::new(
        "update_timestamp".to_string(),
        "users".to_string(),
        TriggerTiming::After,
        vec![TriggerEvent::Update(None)], // All column updates
        TriggerFor::Row,
        None,
        vec![],
        vec![], // No REFERENCING clause
    );

    trigger_registry.register_trigger(timestamp_trigger)?;
    println!("   ✓ Trigger 'update_timestamp' registered\n");

    // Example 3: Create a BEFORE DELETE trigger with column-specific update
    println!("3. Creating trigger for specific column updates...");
    let email_trigger = TriggerDefinition::new(
        "email_validation".to_string(),
        "users".to_string(),
        TriggerTiming::Before,
        vec![TriggerEvent::Update(Some(vec!["email".to_string()]))],
        TriggerFor::Row,
        None,
        vec![],
        vec![], // No REFERENCING clause
    );

    trigger_registry.register_trigger(email_trigger)?;
    println!("   ✓ Trigger 'email_validation' registered\n");

    // Example 4: Query triggers for a table
    println!("4. Querying all triggers for 'users' table...");
    let user_triggers = trigger_registry.get_triggers_for_table("users")?;
    println!("   Found {} triggers:", user_triggers.len());
    for trigger in &user_triggers {
        println!("     - {} ({:?}, {:?})",
            trigger.name,
            trigger.timing,
            trigger.for_each
        );
    }
    println!();

    // Example 5: Query triggers for specific event
    println!("5. Querying BEFORE INSERT triggers...");
    let before_insert_triggers = trigger_registry.get_triggers_for_event(
        "users",
        &TriggerEvent::Insert,
        &TriggerTiming::Before,
    )?;
    println!("   Found {} BEFORE INSERT triggers:", before_insert_triggers.len());
    for trigger in &before_insert_triggers {
        println!("     - {}", trigger.name);
    }
    println!();

    // Example 6: Disable and re-enable a trigger
    println!("6. Testing trigger enable/disable...");
    trigger_registry.disable_trigger("users", "audit_insert")?;
    println!("   ✓ Trigger 'audit_insert' disabled");

    let triggers = trigger_registry.get_triggers_for_event(
        "users",
        &TriggerEvent::Insert,
        &TriggerTiming::Before,
    )?;
    println!("   Active BEFORE INSERT triggers: {}", triggers.len());

    trigger_registry.enable_trigger("users", "audit_insert")?;
    println!("   ✓ Trigger 'audit_insert' re-enabled");

    let triggers = trigger_registry.get_triggers_for_event(
        "users",
        &TriggerEvent::Insert,
        &TriggerTiming::Before,
    )?;
    println!("   Active BEFORE INSERT triggers: {}\n", triggers.len());

    // Example 7: Trigger cascading depth tracking
    println!("7. Testing trigger cascading depth limits...");
    let mut context = TriggerContext::new();
    println!("   Initial depth: {}", context.depth());

    for i in 0..5 {
        context.enter(&format!("trigger_{}", i))?;
        println!("   Entered trigger_{}, depth: {}", i, context.depth());
    }

    for i in (0..5).rev() {
        context.exit();
        println!("   Exited trigger_{}, depth: {}", i, context.depth());
    }
    println!();

    // Example 8: Test depth limit
    println!("8. Testing maximum depth limit ({})", heliosdb_lite::sql::MAX_TRIGGER_DEPTH);
    let mut context = TriggerContext::new();

    // Fill to max depth
    for i in 0..heliosdb_lite::sql::MAX_TRIGGER_DEPTH {
        context.enter(&format!("trigger_{}", i))?;
    }
    println!("   Reached maximum depth: {}", context.depth());

    // Attempt to exceed limit
    match context.enter("trigger_overflow") {
        Ok(_) => println!("   ✗ FAILED: Should have hit depth limit!"),
        Err(e) => println!("   ✓ Correctly rejected: {}", e),
    }
    println!();

    // Example 9: Persistence demonstration (saving to catalog)
    println!("9. Demonstrating trigger persistence...");
    let catalog = db.storage.catalog();

    // Save a trigger to persistent storage
    let persistent_trigger = TriggerDefinition::new(
        "persistent_audit".to_string(),
        "users".to_string(),
        TriggerTiming::After,
        vec![TriggerEvent::Delete],
        TriggerFor::Row,
        None,
        vec![],
        vec![], // No REFERENCING clause
    );

    catalog.save_trigger(&persistent_trigger)?;
    println!("   ✓ Trigger saved to persistent storage");

    // Load it back
    let loaded = catalog.load_trigger("users", "persistent_audit")?;
    match loaded {
        Some(trigger) => {
            println!("   ✓ Trigger loaded: {} on {}", trigger.name, trigger.table_name);
        }
        None => println!("   ✗ Failed to load trigger"),
    }
    println!();

    // Example 10: Drop a trigger
    println!("10. Dropping triggers...");
    let dropped = trigger_registry.drop_trigger("users", "audit_insert")?;
    if dropped {
        println!("   ✓ Trigger 'audit_insert' dropped");
    }

    let remaining = trigger_registry.get_triggers_for_table("users")?;
    println!("   Remaining triggers on 'users': {}\n", remaining.len());

    // Example 11: Drop all triggers for a table
    println!("11. Dropping all triggers for 'users' table...");
    let count = trigger_registry.drop_table_triggers("users")?;
    println!("   ✓ Dropped {} triggers", count);

    let final_triggers = trigger_registry.get_triggers_for_table("users")?;
    println!("   Final trigger count: {}\n", final_triggers.len());

    println!("=== Example Complete ===");

    Ok(())
}
