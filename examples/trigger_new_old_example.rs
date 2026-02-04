// Example demonstrating NEW and OLD context variables in triggers
//
// This example shows how to use NEW and OLD to access row data in trigger bodies

use heliosdb_lite::sql::{LogicalExpr, Evaluator, triggers::TriggerRowContext};
use heliosdb_lite::{Schema, Column, DataType, Tuple, Value};
use std::sync::Arc;

fn main() -> heliosdb_lite::Result<()> {
    println!("NEW and OLD Context Variables in Triggers\n");
    println!("==========================================\n");

    // Define a sample schema for a table
    let schema = Arc::new(Schema {
        columns: vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
            },
            Column {
                name: "price".to_string(),
                data_type: DataType::Float8,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
            },
        ],
    });

    // Example 1: INSERT trigger - NEW row only
    println!("Example 1: INSERT Trigger (NEW row access)");
    println!("-------------------------------------------");

    let new_row = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Widget".to_string()),
        Value::Float8(19.99),
    ]);

    let insert_context = TriggerRowContext::for_insert(new_row.clone());

    // Create evaluator with trigger context
    let evaluator = Evaluator::with_trigger_row_context(
        schema.clone(),
        vec![],
        insert_context,
        schema.clone(),
    );

    // Access NEW.name
    let new_name_expr = LogicalExpr::NewRow {
        column: "name".to_string(),
    };

    let empty_tuple = Tuple::new(vec![]);
    let result = evaluator.evaluate(&new_name_expr, &empty_tuple)?;
    println!("NEW.name = {:?}", result);

    // Access NEW.price
    let new_price_expr = LogicalExpr::NewRow {
        column: "price".to_string(),
    };

    let result = evaluator.evaluate(&new_price_expr, &empty_tuple)?;
    println!("NEW.price = {:?}\n", result);

    // Example 2: UPDATE trigger - both NEW and OLD rows
    println!("Example 2: UPDATE Trigger (NEW and OLD row access)");
    println!("---------------------------------------------------");

    let old_row = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Widget".to_string()),
        Value::Float8(19.99),
    ]);

    let new_row = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Super Widget".to_string()),
        Value::Float8(24.99),
    ]);

    let update_context = TriggerRowContext::for_update(old_row, new_row);

    let evaluator = Evaluator::with_trigger_row_context(
        schema.clone(),
        vec![],
        update_context,
        schema.clone(),
    );

    // Access OLD.name
    let old_name_expr = LogicalExpr::OldRow {
        column: "name".to_string(),
    };

    let result = evaluator.evaluate(&old_name_expr, &empty_tuple)?;
    println!("OLD.name = {:?}", result);

    // Access NEW.name
    let result = evaluator.evaluate(&new_name_expr, &empty_tuple)?;
    println!("NEW.name = {:?}", result);

    // Access OLD.price
    let old_price_expr = LogicalExpr::OldRow {
        column: "price".to_string(),
    };

    let result = evaluator.evaluate(&old_price_expr, &empty_tuple)?;
    println!("OLD.price = {:?}", result);

    // Access NEW.price
    let result = evaluator.evaluate(&new_price_expr, &empty_tuple)?;
    println!("NEW.price = {:?}\n", result);

    // Example 3: DELETE trigger - OLD row only
    println!("Example 3: DELETE Trigger (OLD row access)");
    println!("-------------------------------------------");

    let deleted_row = Tuple::new(vec![
        Value::Int4(1),
        Value::String("Widget".to_string()),
        Value::Float8(19.99),
    ]);

    let delete_context = TriggerRowContext::for_delete(deleted_row);

    let evaluator = Evaluator::with_trigger_row_context(
        schema.clone(),
        vec![],
        delete_context,
        schema.clone(),
    );

    // Access OLD.name
    let result = evaluator.evaluate(&old_name_expr, &empty_tuple)?;
    println!("OLD.name = {:?}", result);

    // Access OLD.price
    let result = evaluator.evaluate(&old_price_expr, &empty_tuple)?;
    println!("OLD.price = {:?}\n", result);

    // Example 4: Demonstrate error handling
    println!("Example 4: Error Handling");
    println!("-------------------------");

    // Try to access NEW in DELETE context (should error)
    match evaluator.evaluate(&new_name_expr, &empty_tuple) {
        Ok(_) => println!("Unexpected success"),
        Err(e) => println!("Expected error: {}", e),
    }

    println!("\nAll examples completed successfully!");

    Ok(())
}
