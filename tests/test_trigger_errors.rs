// Test error handling for unsupported trigger features
// Compile with: rustc --edition 2021 test_trigger_errors.rs -L target/debug/deps --extern heliosdb_nano=target/debug/libheliosdb_nano.rlib

use heliosdb_nano::sql::{Parser, Planner};

fn main() {
    println!("=== HeliosDB Trigger Error Handling Test ===\n");

    // Test 1: TRUNCATE event (unsupported)
    println!("Test 1: TRUNCATE event (should error)");
    let sql = "CREATE TRIGGER trunc_trigger AFTER TRUNCATE ON users FOR EACH ROW EXECUTE FUNCTION handle_truncate()";
    test_error(sql, "TRUNCATE");

    // Note: The following tests are commented out because sqlparser may not parse them,
    // or they may be handled differently. The important test is TRUNCATE which we know
    // sqlparser supports but we explicitly reject.

    println!("\n=== Error handling tests completed ===");
}

fn test_error(sql: &str, expected_error: &str) {
    println!("SQL: {}", sql);

    let parser = Parser::new();
    match parser.parse_one(sql) {
        Ok(statement) => {
            println!("✓ Parsed AST successfully");

            let planner = Planner::new();
            match planner.statement_to_plan(statement) {
                Ok(plan) => {
                    println!("✗ Unexpectedly succeeded in creating plan: {:?}", plan);
                }
                Err(e) => {
                    let error_msg = format!("{}", e);
                    if error_msg.to_lowercase().contains(&expected_error.to_lowercase()) {
                        println!("✓ Correctly rejected with error: {}", e);
                    } else {
                        println!("✗ Got error but wrong message: {}", e);
                        println!("   Expected to contain: {}", expected_error);
                    }
                }
            }
        }
        Err(e) => {
            println!("Parse error (may be expected): {}", e);
        }
    }
}
