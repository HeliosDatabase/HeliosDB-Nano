// Standalone test for trigger parsing
// Compile with: rustc --edition 2021 test_trigger_manual.rs -L target/debug/deps --extern heliosdb_nano=target/debug/libheliosdb_nano.rlib

use heliosdb_nano::sql::{Parser, Planner};
use heliosdb_nano::sql::logical_plan::{LogicalPlan, TriggerTiming, TriggerEvent, TriggerFor};

fn main() {
    println!("=== HeliosDB Trigger Parser Manual Test ===\n");

    // Test 1: Basic AFTER INSERT trigger
    println!("Test 1: AFTER INSERT trigger");
    let sql = "CREATE TRIGGER audit_insert AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION audit_log()";
    test_trigger(sql);

    // Test 2: BEFORE UPDATE trigger
    println!("\nTest 2: BEFORE UPDATE trigger");
    let sql = "CREATE TRIGGER update_timestamp BEFORE UPDATE ON products FOR EACH ROW EXECUTE FUNCTION update_modified_at()";
    test_trigger(sql);

    // Test 3: INSTEAD OF DELETE trigger
    println!("\nTest 3: INSTEAD OF DELETE trigger");
    let sql = "CREATE TRIGGER prevent_delete INSTEAD OF DELETE ON users FOR EACH ROW EXECUTE FUNCTION log_delete_attempt()";
    test_trigger(sql);

    // Test 4: UPDATE OF specific columns
    println!("\nTest 4: UPDATE OF columns trigger");
    let sql = "CREATE TRIGGER track_price_change AFTER UPDATE OF price, discount ON products FOR EACH ROW EXECUTE FUNCTION log_price_change()";
    test_trigger(sql);

    // Test 5: FOR EACH STATEMENT
    println!("\nTest 5: FOR EACH STATEMENT trigger");
    let sql = "CREATE TRIGGER bulk_audit AFTER INSERT ON orders FOR EACH STATEMENT EXECUTE FUNCTION audit_bulk_insert()";
    test_trigger(sql);

    // Test 6: OR REPLACE
    println!("\nTest 6: OR REPLACE trigger");
    let sql = "CREATE OR REPLACE TRIGGER replace_audit AFTER INSERT ON logs FOR EACH ROW EXECUTE FUNCTION audit_logs()";
    test_trigger(sql);

    // Test 7: Multiple events
    println!("\nTest 7: Multiple events trigger");
    let sql = "CREATE TRIGGER multi_event AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION track_changes()";
    test_trigger(sql);

    // Test 8: DROP TRIGGER
    println!("\nTest 8: DROP TRIGGER");
    let sql = "DROP TRIGGER audit_insert ON users";
    test_trigger(sql);

    // Test 9: DROP TRIGGER IF EXISTS
    println!("\nTest 9: DROP TRIGGER IF EXISTS");
    let sql = "DROP TRIGGER IF EXISTS old_trigger ON products";
    test_trigger(sql);

    // Test 10: DROP TRIGGER with CASCADE
    println!("\nTest 10: DROP TRIGGER CASCADE");
    let sql = "DROP TRIGGER legacy_trigger ON orders CASCADE";
    test_trigger(sql);

    println!("\n=== All tests completed ===");
}

fn test_trigger(sql: &str) {
    println!("SQL: {}", sql);

    let parser = Parser::new();
    match parser.parse_one(sql) {
        Ok(statement) => {
            println!("✓ Parsed AST successfully");

            let planner = Planner::new();
            match planner.statement_to_plan(statement) {
                Ok(plan) => {
                    println!("✓ Converted to logical plan successfully");
                    match plan {
                        LogicalPlan::CreateTrigger { name, table_name, timing, events, for_each, when_condition, body, if_not_exists, .. } => {
                            println!("  Type: CREATE TRIGGER");
                            println!("  Name: {}", name);
                            println!("  Table: {}", table_name);
                            println!("  Timing: {:?}", timing);
                            println!("  Events: {:?}", events);
                            println!("  For Each: {:?}", for_each);
                            println!("  When: {:?}", when_condition.is_some());
                            println!("  Body statements: {}", body.len());
                            println!("  OR REPLACE: {}", if_not_exists);
                        }
                        LogicalPlan::DropTrigger { name, table_name, if_exists } => {
                            println!("  Type: DROP TRIGGER");
                            println!("  Name: {}", name);
                            println!("  Table: {:?}", table_name);
                            println!("  IF EXISTS: {}", if_exists);
                        }
                        _ => {
                            println!("✗ Unexpected plan type");
                        }
                    }
                }
                Err(e) => {
                    println!("✗ Failed to convert to logical plan: {}", e);
                }
            }
        }
        Err(e) => {
            println!("✗ Failed to parse: {}", e);
        }
    }
}
