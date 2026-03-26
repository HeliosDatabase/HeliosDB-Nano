//! HeliosDB Nano - Embedded Database Example
//!
//! Demonstrates basic embedded usage: create a table, insert rows,
//! query data, and print results. No server required.

use heliosdb_nano::{EmbeddedDatabase, Result};

fn main() -> Result<()> {
    // Create an in-memory database (no files on disk)
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create a table
    db.execute(
        "CREATE TABLE products (id INT PRIMARY KEY, name TEXT NOT NULL, price INT NOT NULL)",
    )?;

    // Insert rows
    db.execute("INSERT INTO products (id, name, price) VALUES (1, 'Keyboard', 75)")?;
    db.execute("INSERT INTO products (id, name, price) VALUES (2, 'Mouse', 40)")?;
    db.execute("INSERT INTO products (id, name, price) VALUES (3, 'Monitor', 350)")?;
    db.execute("INSERT INTO products (id, name, price) VALUES (4, 'Headset', 95)")?;

    // Query all rows
    let rows = db.query("SELECT id, name, price FROM products ORDER BY price", &[])?;
    println!("All products (by price):");
    for row in &rows {
        println!("  {:?}", row);
    }

    // Filtered query
    let expensive = db.query("SELECT name, price FROM products WHERE price > 50", &[])?;
    println!("\nProducts over 50:");
    for row in &expensive {
        println!("  {:?}", row);
    }

    // Aggregate
    let total = db.query("SELECT COUNT(*), SUM(price) FROM products", &[])?;
    println!("\nSummary: {:?}", total[0]);

    Ok(())
}
