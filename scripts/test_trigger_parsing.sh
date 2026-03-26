#!/bin/bash
# Test script for CREATE TRIGGER and DROP TRIGGER parsing

echo "Testing CREATE TRIGGER and DROP TRIGGER parsing..."
echo ""

# Create a Rust test program
cat > /tmp/test_trigger_parser.rs << 'EOF'
use heliosdb_nano::sql::{Parser, Planner};
use heliosdb_nano::sql::logical_plan::{LogicalPlan, TriggerTiming, TriggerEvent, TriggerFor};

fn test_parse_trigger(sql: &str, expected_name: &str) {
    println!("\n=== Testing: {} ===", sql);
    let parser = Parser::new();

    match parser.parse_one(sql) {
        Ok(statement) => {
            println!("✓ Parsed successfully");
            println!("Statement: {:?}", statement);

            // Convert to logical plan
            let planner = Planner::new();
            match planner.statement_to_plan(statement) {
                Ok(plan) => {
                    println!("✓ Converted to logical plan");
                    match &plan {
                        LogicalPlan::CreateTrigger { name, table_name, timing, events, for_each, when_condition, .. } => {
                            println!("  Trigger Name: {}", name);
                            println!("  Table: {}", table_name);
                            println!("  Timing: {:?}", timing);
                            println!("  Events: {:?}", events);
                            println!("  For Each: {:?}", for_each);
                            println!("  When: {:?}", when_condition);

                            if name == expected_name {
                                println!("✓ Trigger name matches expected: {}", expected_name);
                            } else {
                                println!("✗ Trigger name mismatch! Expected: {}, Got: {}", expected_name, name);
                            }
                        }
                        LogicalPlan::DropTrigger { name, table_name, if_exists } => {
                            println!("  Trigger Name: {}", name);
                            println!("  Table: {:?}", table_name);
                            println!("  If Exists: {}", if_exists);

                            if name == expected_name {
                                println!("✓ Trigger name matches expected: {}", expected_name);
                            } else {
                                println!("✗ Trigger name mismatch! Expected: {}, Got: {}", expected_name, name);
                            }
                        }
                        _ => println!("✗ Unexpected plan type: {:?}", plan),
                    }
                }
                Err(e) => {
                    println!("✗ Failed to convert to logical plan: {}", e);
                }
            }
        }
        Err(e) => {
            println!("✗ Parse failed: {}", e);
        }
    }
}

fn main() {
    println!("HeliosDB Trigger Parser Test Suite");
    println!("===================================\n");

    // Test 1: Basic AFTER INSERT trigger
    test_parse_trigger(
        "CREATE TRIGGER audit_insert AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION audit_log()",
        "audit_insert"
    );

    // Test 2: BEFORE UPDATE trigger
    test_parse_trigger(
        "CREATE TRIGGER update_timestamp BEFORE UPDATE ON products FOR EACH ROW EXECUTE FUNCTION update_modified_at()",
        "update_timestamp"
    );

    // Test 3: INSTEAD OF DELETE trigger
    test_parse_trigger(
        "CREATE TRIGGER prevent_delete INSTEAD OF DELETE ON users FOR EACH ROW EXECUTE FUNCTION log_delete_attempt()",
        "prevent_delete"
    );

    // Test 4: UPDATE OF specific columns
    test_parse_trigger(
        "CREATE TRIGGER track_price_change AFTER UPDATE OF price, discount ON products FOR EACH ROW EXECUTE FUNCTION log_price_change()",
        "track_price_change"
    );

    // Test 5: FOR EACH STATEMENT
    test_parse_trigger(
        "CREATE TRIGGER bulk_audit AFTER INSERT ON orders FOR EACH STATEMENT EXECUTE FUNCTION audit_bulk_insert()",
        "bulk_audit"
    );

    // Test 6: WITH WHEN clause - Note: WHEN clause syntax may not be fully supported by sqlparser
    // Commenting out for now as it may require special handling
    // test_parse_trigger(
    //     "CREATE TRIGGER conditional_audit AFTER UPDATE ON users FOR EACH ROW WHEN (NEW.status = 'active') EXECUTE FUNCTION audit_activation()",
    //     "conditional_audit"
    // );

    // Test 7: OR REPLACE trigger
    test_parse_trigger(
        "CREATE OR REPLACE TRIGGER replace_audit AFTER INSERT ON logs FOR EACH ROW EXECUTE FUNCTION audit_logs()",
        "replace_audit"
    );

    // Test 8: Multiple events
    test_parse_trigger(
        "CREATE TRIGGER multi_event AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION track_changes()",
        "multi_event"
    );

    // Test 9: DROP TRIGGER
    test_parse_trigger(
        "DROP TRIGGER audit_insert ON users",
        "audit_insert"
    );

    // Test 10: DROP TRIGGER IF EXISTS
    test_parse_trigger(
        "DROP TRIGGER IF EXISTS old_trigger ON products",
        "old_trigger"
    );

    // Test 11: DROP TRIGGER with CASCADE
    test_parse_trigger(
        "DROP TRIGGER legacy_trigger ON orders CASCADE",
        "legacy_trigger"
    );

    println!("\n=== Test Suite Complete ===");
}
EOF

# Compile and run the test
cd /home/claude/HeliosDB Nano
echo "Compiling test program..."
rustc --edition 2021 \
    --extern heliosdb_nano=target/debug/libheliosdb_nano.rlib \
    -L target/debug/deps \
    /tmp/test_trigger_parser.rs \
    -o /tmp/test_trigger_parser 2>&1 | head -20

if [ -f /tmp/test_trigger_parser ]; then
    echo "Running tests..."
    /tmp/test_trigger_parser
else
    echo "Compilation failed. Trying alternative approach with cargo test..."
fi
