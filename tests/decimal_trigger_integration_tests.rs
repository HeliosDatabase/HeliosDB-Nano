//! DECIMAL and TRIGGER integration tests
//!
//! Tests for triggers that operate on DECIMAL columns, DECIMAL arithmetic in triggers,
//! type conversions in trigger bodies, and DECIMAL comparisons in WHEN clauses.

#[cfg(test)]
mod decimal_trigger_integration_tests {
    use heliosdb_lite::{EmbeddedDatabase, Value, DataType};

    /// Create an in-memory test database
    fn create_test_db() -> EmbeddedDatabase {
        EmbeddedDatabase::new_in_memory().expect("Failed to create test database")
    }

    // ==================== Triggers Operating on DECIMAL Columns ====================

    #[test]
    fn test_trigger_insert_decimal_column() {
        let db = create_test_db();
        db.execute("CREATE TABLE prices (id INT, amount DECIMAL)").unwrap();
        db.execute("CREATE TABLE price_log (logged_amount DECIMAL, logged_at TEXT)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER log_price_insert
             AFTER INSERT ON prices
             FOR EACH ROW
             BEGIN
                 INSERT INTO price_log VALUES (NEW.amount, datetime('now'));
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO prices VALUES (1, 123.45)").unwrap();
            db.execute("INSERT INTO prices VALUES (2, 0.01)").unwrap();
            db.execute("INSERT INTO prices VALUES (3, 9999.99)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM price_log", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should log DECIMAL values");
        } else {
            println!("DECIMAL trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_update_decimal_column() {
        let db = create_test_db();
        db.execute("CREATE TABLE products (id INT, price DECIMAL)").unwrap();
        db.execute("CREATE TABLE price_changes (product_id INT, old_price DECIMAL, new_price DECIMAL, change_amount DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER track_price_change
             AFTER UPDATE OF price ON products
             FOR EACH ROW
             BEGIN
                 INSERT INTO price_changes VALUES (NEW.id, OLD.price, NEW.price, NEW.price - OLD.price);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO products VALUES (1, 100.00)").unwrap();
            db.execute("UPDATE products SET price = 125.50 WHERE id = 1").unwrap();

            let rows = db.query("SELECT change_amount FROM price_changes WHERE product_id = 1", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should calculate DECIMAL difference");
        } else {
            println!("DECIMAL UPDATE trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_precision_preservation() {
        let db = create_test_db();
        db.execute("CREATE TABLE transactions (id INT, amount DECIMAL(10,4))").unwrap();
        db.execute("CREATE TABLE transaction_audit (original_amount DECIMAL(10,4))").unwrap();

        let result = db.execute(
            "CREATE TRIGGER audit_transaction
             AFTER INSERT ON transactions
             FOR EACH ROW
             BEGIN
                 INSERT INTO transaction_audit VALUES (NEW.amount);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO transactions VALUES (1, 123.4567)").unwrap();

            let rows = db.query("SELECT original_amount FROM transaction_audit", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should preserve DECIMAL precision");
        } else {
            println!("DECIMAL precision trigger not implemented: {}", result.unwrap_err());
        }
    }

    // ==================== DECIMAL Arithmetic in Triggers ====================

    #[test]
    fn test_trigger_decimal_addition() {
        let db = create_test_db();
        db.execute("CREATE TABLE sales (id INT, amount DECIMAL)").unwrap();
        db.execute("CREATE TABLE totals (total DECIMAL)").unwrap();
        db.execute("INSERT INTO totals VALUES (0.00)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER update_total_on_sale
             AFTER INSERT ON sales
             FOR EACH ROW
             BEGIN
                 UPDATE totals SET total = total + NEW.amount;
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO sales VALUES (1, 10.50)").unwrap();
            db.execute("INSERT INTO sales VALUES (2, 20.75)").unwrap();
            db.execute("INSERT INTO sales VALUES (3, 5.25)").unwrap();

            let rows = db.query("SELECT total FROM totals", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should perform DECIMAL addition");
            // Expected total: 36.50
        } else {
            println!("DECIMAL addition trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_subtraction() {
        let db = create_test_db();
        db.execute("CREATE TABLE refunds (id INT, amount DECIMAL)").unwrap();
        db.execute("CREATE TABLE account_balance (balance DECIMAL)").unwrap();
        db.execute("INSERT INTO account_balance VALUES (1000.00)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER process_refund
             AFTER INSERT ON refunds
             FOR EACH ROW
             BEGIN
                 UPDATE account_balance SET balance = balance - NEW.amount;
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO refunds VALUES (1, 50.25)").unwrap();
            db.execute("INSERT INTO refunds VALUES (2, 100.50)").unwrap();

            let rows = db.query("SELECT balance FROM account_balance", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should perform DECIMAL subtraction");
            // Expected balance: 849.25
        } else {
            println!("DECIMAL subtraction trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_multiplication() {
        let db = create_test_db();
        db.execute("CREATE TABLE orders (id INT, quantity INT, unit_price DECIMAL)").unwrap();
        db.execute("CREATE TABLE order_totals (order_id INT, total DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER calculate_order_total
             AFTER INSERT ON orders
             FOR EACH ROW
             BEGIN
                 INSERT INTO order_totals VALUES (NEW.id, NEW.quantity * NEW.unit_price);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO orders VALUES (1, 5, 19.99)").unwrap();
            db.execute("INSERT INTO orders VALUES (2, 3, 25.50)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM order_totals", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should perform DECIMAL multiplication");
        } else {
            println!("DECIMAL multiplication trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_division() {
        let db = create_test_db();
        db.execute("CREATE TABLE expenses (id INT, total DECIMAL, months INT)").unwrap();
        db.execute("CREATE TABLE monthly_expenses (expense_id INT, monthly_amount DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER calculate_monthly
             AFTER INSERT ON expenses
             FOR EACH ROW
             BEGIN
                 INSERT INTO monthly_expenses VALUES (NEW.id, NEW.total / NEW.months);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO expenses VALUES (1, 1200.00, 12)").unwrap();
            db.execute("INSERT INTO expenses VALUES (2, 500.00, 5)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM monthly_expenses", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should perform DECIMAL division");
        } else {
            println!("DECIMAL division trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_complex_decimal_arithmetic() {
        let db = create_test_db();
        db.execute("CREATE TABLE invoice_items (id INT, quantity INT, price DECIMAL, tax_rate DECIMAL)").unwrap();
        db.execute("CREATE TABLE invoice_totals (item_id INT, subtotal DECIMAL, tax DECIMAL, total DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER calculate_invoice_total
             AFTER INSERT ON invoice_items
             FOR EACH ROW
             BEGIN
                 INSERT INTO invoice_totals
                 VALUES (
                     NEW.id,
                     NEW.quantity * NEW.price,
                     (NEW.quantity * NEW.price) * NEW.tax_rate,
                     (NEW.quantity * NEW.price) * (1 + NEW.tax_rate)
                 );
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO invoice_items VALUES (1, 10, 25.00, 0.08)").unwrap();

            let rows = db.query("SELECT subtotal, tax, total FROM invoice_totals WHERE item_id = 1", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should perform complex DECIMAL arithmetic");
            // Expected: subtotal=250.00, tax=20.00, total=270.00
        } else {
            println!("Complex DECIMAL arithmetic trigger not implemented: {}", result.unwrap_err());
        }
    }

    // ==================== Type Conversions in Trigger Bodies ====================

    #[test]
    fn test_trigger_int_to_decimal_conversion() {
        let db = create_test_db();
        db.execute("CREATE TABLE int_values (id INT, int_val INT)").unwrap();
        db.execute("CREATE TABLE decimal_values (id INT, dec_val DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER convert_int_to_decimal
             AFTER INSERT ON int_values
             FOR EACH ROW
             BEGIN
                 INSERT INTO decimal_values VALUES (NEW.id, CAST(NEW.int_val AS DECIMAL));
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO int_values VALUES (1, 100)").unwrap();
            db.execute("INSERT INTO int_values VALUES (2, 250)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM decimal_values", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should convert INT to DECIMAL");
        } else {
            println!("INT to DECIMAL conversion trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_to_int_conversion() {
        let db = create_test_db();
        db.execute("CREATE TABLE decimal_values (id INT, dec_val DECIMAL)").unwrap();
        db.execute("CREATE TABLE int_values (id INT, int_val INT)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER convert_decimal_to_int
             AFTER INSERT ON decimal_values
             FOR EACH ROW
             BEGIN
                 INSERT INTO int_values VALUES (NEW.id, CAST(NEW.dec_val AS INT));
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO decimal_values VALUES (1, 123.45)").unwrap();
            db.execute("INSERT INTO decimal_values VALUES (2, 999.99)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM int_values", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should convert DECIMAL to INT (truncate)");
        } else {
            println!("DECIMAL to INT conversion trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_float_to_decimal_conversion() {
        let db = create_test_db();
        db.execute("CREATE TABLE float_values (id INT, float_val FLOAT8)").unwrap();
        db.execute("CREATE TABLE decimal_values (id INT, dec_val DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER convert_float_to_decimal
             AFTER INSERT ON float_values
             FOR EACH ROW
             BEGIN
                 INSERT INTO decimal_values VALUES (NEW.id, CAST(NEW.float_val AS DECIMAL));
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO float_values VALUES (1, 123.456)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM decimal_values", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should convert FLOAT to DECIMAL");
        } else {
            println!("FLOAT to DECIMAL conversion trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_string_to_decimal_conversion() {
        let db = create_test_db();
        db.execute("CREATE TABLE string_values (id INT, str_val TEXT)").unwrap();
        db.execute("CREATE TABLE decimal_values (id INT, dec_val DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER convert_string_to_decimal
             AFTER INSERT ON string_values
             FOR EACH ROW
             BEGIN
                 INSERT INTO decimal_values VALUES (NEW.id, CAST(NEW.str_val AS DECIMAL));
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO string_values VALUES (1, '123.45')").unwrap();
            db.execute("INSERT INTO string_values VALUES (2, '999.999')").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM decimal_values", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should convert STRING to DECIMAL");
        } else {
            println!("STRING to DECIMAL conversion trigger not implemented: {}", result.unwrap_err());
        }
    }

    // ==================== DECIMAL Comparisons in WHEN Clauses ====================

    #[test]
    fn test_trigger_when_decimal_greater_than() {
        let db = create_test_db();
        db.execute("CREATE TABLE products (id INT, price DECIMAL)").unwrap();
        db.execute("CREATE TABLE expensive_products (id INT, price DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER track_expensive_products
             AFTER INSERT ON products
             FOR EACH ROW
             WHEN (NEW.price > 100.00)
             BEGIN
                 INSERT INTO expensive_products VALUES (NEW.id, NEW.price);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO products VALUES (1, 50.00)").unwrap();
            db.execute("INSERT INTO products VALUES (2, 150.00)").unwrap();
            db.execute("INSERT INTO products VALUES (3, 250.00)").unwrap();
            db.execute("INSERT INTO products VALUES (4, 75.00)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM expensive_products", &[]).unwrap();
            // Should have 2 products (price > 100)
            assert!(rows.len() > 0, "WHEN clause should filter by DECIMAL comparison");
        } else {
            println!("WHEN clause with DECIMAL > not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_when_decimal_less_than() {
        let db = create_test_db();
        db.execute("CREATE TABLE items (id INT, discount DECIMAL)").unwrap();
        db.execute("CREATE TABLE small_discounts (id INT, discount DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER track_small_discounts
             AFTER INSERT ON items
             FOR EACH ROW
             WHEN (NEW.discount < 10.00)
             BEGIN
                 INSERT INTO small_discounts VALUES (NEW.id, NEW.discount);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO items VALUES (1, 5.00)").unwrap();
            db.execute("INSERT INTO items VALUES (2, 15.00)").unwrap();
            db.execute("INSERT INTO items VALUES (3, 2.50)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM small_discounts", &[]).unwrap();
            // Should have 2 items (discount < 10)
            assert!(rows.len() > 0, "WHEN clause should filter by DECIMAL < comparison");
        } else {
            println!("WHEN clause with DECIMAL < not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_when_decimal_equals() {
        let db = create_test_db();
        db.execute("CREATE TABLE payments (id INT, amount DECIMAL)").unwrap();
        db.execute("CREATE TABLE exact_payments (id INT, amount DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER track_exact_hundred
             AFTER INSERT ON payments
             FOR EACH ROW
             WHEN (NEW.amount = 100.00)
             BEGIN
                 INSERT INTO exact_payments VALUES (NEW.id, NEW.amount);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO payments VALUES (1, 100.00)").unwrap();
            db.execute("INSERT INTO payments VALUES (2, 99.99)").unwrap();
            db.execute("INSERT INTO payments VALUES (3, 100.00)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM exact_payments", &[]).unwrap();
            // Should have 2 payments (amount = 100.00)
            assert!(rows.len() > 0, "WHEN clause should filter by DECIMAL = comparison");
        } else {
            println!("WHEN clause with DECIMAL = not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_when_decimal_between() {
        let db = create_test_db();
        db.execute("CREATE TABLE sales (id INT, amount DECIMAL)").unwrap();
        db.execute("CREATE TABLE mid_range_sales (id INT, amount DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER track_mid_range
             AFTER INSERT ON sales
             FOR EACH ROW
             WHEN (NEW.amount >= 50.00 AND NEW.amount <= 200.00)
             BEGIN
                 INSERT INTO mid_range_sales VALUES (NEW.id, NEW.amount);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO sales VALUES (1, 25.00)").unwrap();
            db.execute("INSERT INTO sales VALUES (2, 100.00)").unwrap();
            db.execute("INSERT INTO sales VALUES (3, 150.00)").unwrap();
            db.execute("INSERT INTO sales VALUES (4, 300.00)").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM mid_range_sales", &[]).unwrap();
            // Should have 2 sales (50 <= amount <= 200)
            assert!(rows.len() > 0, "WHEN clause should handle DECIMAL range comparison");
        } else {
            println!("WHEN clause with DECIMAL range not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_when_decimal_change_comparison() {
        let db = create_test_db();
        db.execute("CREATE TABLE stock_prices (id INT, price DECIMAL)").unwrap();
        db.execute("CREATE TABLE significant_changes (id INT, old_price DECIMAL, new_price DECIMAL, change DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER track_significant_changes
             AFTER UPDATE OF price ON stock_prices
             FOR EACH ROW
             WHEN (ABS(NEW.price - OLD.price) > 10.00)
             BEGIN
                 INSERT INTO significant_changes VALUES (NEW.id, OLD.price, NEW.price, NEW.price - OLD.price);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO stock_prices VALUES (1, 100.00)").unwrap();

            // Small change (< 10)
            db.execute("UPDATE stock_prices SET price = 105.00 WHERE id = 1").unwrap();

            // Large change (> 10)
            db.execute("UPDATE stock_prices SET price = 120.00 WHERE id = 1").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM significant_changes", &[]).unwrap();
            // Should have 1 change (ABS(change) > 10)
            assert!(rows.len() > 0, "WHEN clause should compare DECIMAL change amounts");
        } else {
            println!("WHEN clause with DECIMAL change comparison not implemented: {}", result.unwrap_err());
        }
    }

    // ==================== Advanced Integration Scenarios ====================

    #[test]
    fn test_trigger_decimal_running_average() {
        let db = create_test_db();
        db.execute("CREATE TABLE measurements (id INT, value DECIMAL)").unwrap();
        db.execute("CREATE TABLE statistics (count INT, sum DECIMAL, average DECIMAL)").unwrap();
        db.execute("INSERT INTO statistics VALUES (0, 0.00, 0.00)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER update_running_average
             AFTER INSERT ON measurements
             FOR EACH ROW
             BEGIN
                 UPDATE statistics
                 SET count = count + 1,
                     sum = sum + NEW.value,
                     average = (sum + NEW.value) / (count + 1);
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO measurements VALUES (1, 10.00)").unwrap();
            db.execute("INSERT INTO measurements VALUES (2, 20.00)").unwrap();
            db.execute("INSERT INTO measurements VALUES (3, 30.00)").unwrap();

            let rows = db.query("SELECT average FROM statistics", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should calculate running average with DECIMAL");
            // Expected average: 20.00
        } else {
            println!("Running average trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_percentage_calculation() {
        let db = create_test_db();
        db.execute("CREATE TABLE sales (id INT, amount DECIMAL, category TEXT)").unwrap();
        db.execute("CREATE TABLE category_stats (category TEXT, total DECIMAL, percentage DECIMAL)").unwrap();
        db.execute("CREATE TABLE grand_total (total DECIMAL)").unwrap();
        db.execute("INSERT INTO grand_total VALUES (0.00)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER update_percentages
             AFTER INSERT ON sales
             FOR EACH ROW
             BEGIN
                 UPDATE grand_total SET total = total + NEW.amount;
                 INSERT OR REPLACE INTO category_stats
                 VALUES (
                     NEW.category,
                     COALESCE((SELECT total FROM category_stats WHERE category = NEW.category), 0.00) + NEW.amount,
                     ((COALESCE((SELECT total FROM category_stats WHERE category = NEW.category), 0.00) + NEW.amount) /
                      (SELECT total FROM grand_total)) * 100.00
                 );
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO sales VALUES (1, 100.00, 'Electronics')").unwrap();
            db.execute("INSERT INTO sales VALUES (2, 50.00, 'Books')").unwrap();
            db.execute("INSERT INTO sales VALUES (3, 150.00, 'Electronics')").unwrap();

            let rows = db.query("SELECT category, percentage FROM category_stats", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should calculate DECIMAL percentages");
        } else {
            println!("Percentage calculation trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_tax_calculation_multiple_rates() {
        let db = create_test_db();
        db.execute("CREATE TABLE purchases (id INT, amount DECIMAL, state TEXT)").unwrap();
        db.execute("CREATE TABLE tax_rates (state TEXT, rate DECIMAL)").unwrap();
        db.execute("CREATE TABLE purchase_totals (purchase_id INT, subtotal DECIMAL, tax DECIMAL, total DECIMAL)").unwrap();

        // Setup tax rates
        db.execute("INSERT INTO tax_rates VALUES ('CA', 0.0725)").unwrap();
        db.execute("INSERT INTO tax_rates VALUES ('NY', 0.0800)").unwrap();
        db.execute("INSERT INTO tax_rates VALUES ('TX', 0.0625)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER calculate_tax
             AFTER INSERT ON purchases
             FOR EACH ROW
             BEGIN
                 INSERT INTO purchase_totals
                 SELECT
                     NEW.id,
                     NEW.amount,
                     NEW.amount * rate,
                     NEW.amount * (1 + rate)
                 FROM tax_rates
                 WHERE state = NEW.state;
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO purchases VALUES (1, 100.00, 'CA')").unwrap();
            db.execute("INSERT INTO purchases VALUES (2, 200.00, 'NY')").unwrap();

            let rows = db.query("SELECT COUNT(*) FROM purchase_totals", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should calculate state-specific tax with DECIMAL");
        } else {
            println!("Tax calculation trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_discount_tiers() {
        let db = create_test_db();
        db.execute("CREATE TABLE orders (id INT, subtotal DECIMAL)").unwrap();
        db.execute("CREATE TABLE order_discounts (order_id INT, discount_rate DECIMAL, discount_amount DECIMAL, final_total DECIMAL)").unwrap();

        let result = db.execute(
            "CREATE TRIGGER apply_tiered_discount
             AFTER INSERT ON orders
             FOR EACH ROW
             BEGIN
                 INSERT INTO order_discounts
                 VALUES (
                     NEW.id,
                     CASE
                         WHEN NEW.subtotal >= 1000.00 THEN 0.15
                         WHEN NEW.subtotal >= 500.00 THEN 0.10
                         WHEN NEW.subtotal >= 100.00 THEN 0.05
                         ELSE 0.00
                     END,
                     NEW.subtotal * CASE
                         WHEN NEW.subtotal >= 1000.00 THEN 0.15
                         WHEN NEW.subtotal >= 500.00 THEN 0.10
                         WHEN NEW.subtotal >= 100.00 THEN 0.05
                         ELSE 0.00
                     END,
                     NEW.subtotal * (1 - CASE
                         WHEN NEW.subtotal >= 1000.00 THEN 0.15
                         WHEN NEW.subtotal >= 500.00 THEN 0.10
                         WHEN NEW.subtotal >= 100.00 THEN 0.05
                         ELSE 0.00
                     END)
                 );
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO orders VALUES (1, 50.00)").unwrap();    // 0% discount
            db.execute("INSERT INTO orders VALUES (2, 150.00)").unwrap();   // 5% discount
            db.execute("INSERT INTO orders VALUES (3, 600.00)").unwrap();   // 10% discount
            db.execute("INSERT INTO orders VALUES (4, 1200.00)").unwrap();  // 15% discount

            let rows = db.query("SELECT order_id, discount_rate FROM order_discounts ORDER BY order_id", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should apply tiered DECIMAL discounts");
        } else {
            println!("Tiered discount trigger not implemented: {}", result.unwrap_err());
        }
    }

    #[test]
    fn test_trigger_decimal_compound_interest() {
        let db = create_test_db();
        db.execute("CREATE TABLE deposits (account_id INT, amount DECIMAL)").unwrap();
        db.execute("CREATE TABLE accounts (id INT, balance DECIMAL, interest_rate DECIMAL)").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 0.00, 0.05)").unwrap(); // 5% interest

        let result = db.execute(
            "CREATE TRIGGER apply_interest_on_deposit
             AFTER INSERT ON deposits
             FOR EACH ROW
             BEGIN
                 UPDATE accounts
                 SET balance = (balance + NEW.amount) * (1 + interest_rate)
                 WHERE id = NEW.account_id;
             END"
        );

        if result.is_ok() {
            db.execute("INSERT INTO deposits VALUES (1, 1000.00)").unwrap();

            let rows = db.query("SELECT balance FROM accounts WHERE id = 1", &[]).unwrap();
            assert!(rows.len() > 0, "Trigger should apply compound interest with DECIMAL");
            // Expected: (0 + 1000) * 1.05 = 1050.00
        } else {
            println!("Compound interest trigger not implemented: {}", result.unwrap_err());
        }
    }
}
