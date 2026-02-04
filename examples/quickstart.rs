//! HeliosDB Lite Quickstart Example
//!
//! This example demonstrates the basic CRUD operations and features of HeliosDB Lite.

use heliosdb_lite::{EmbeddedDatabase, Result};

fn main() -> Result<()> {
    println!("🚀 HeliosDB Lite - Quickstart Example\n");
    println!("═══════════════════════════════════════\n");

    // Step 1: Create an in-memory database
    println!("📦 Creating in-memory database...");
    let db = EmbeddedDatabase::new_in_memory()?;
    println!("✅ Database created\n");

    // Step 2: Create a table
    println!("📋 Creating 'users' table...");
    db.execute("CREATE TABLE users (id INT, name TEXT, email TEXT, age INT)")?;
    println!("✅ Table created\n");

    // Step 3: Insert data
    println!("➕ Inserting data...");
    db.execute("INSERT INTO users VALUES (1, 'Alice Johnson', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users VALUES (2, 'Bob Smith', 'bob@example.com', 25)")?;
    db.execute("INSERT INTO users VALUES (3, 'Charlie Brown', 'charlie@example.com', 35)")?;
    db.execute("INSERT INTO users VALUES (4, 'Diana Prince', 'diana@example.com', 28)")?;
    println!("✅ Inserted 4 users\n");

    // Step 4: Query all data
    println!("🔍 Querying all users...");
    let all_users = db.query("SELECT * FROM users", &[])?;
    println!("Found {} users:", all_users.len());
    for user in &all_users {
        println!("  {:?}", user);
    }
    println!();

    // Step 5: Query with WHERE clause
    println!("🔍 Querying users older than 28...");
    let filtered = db.query("SELECT * FROM users WHERE age > 28", &[])?;
    println!("Found {} users:", filtered.len());
    for user in &filtered {
        println!("  {:?}", user);
    }
    println!();

    // Step 6: Update data
    println!("✏️  Updating Alice's age...");
    let updated = db.execute("UPDATE users SET age = 31 WHERE name = 'Alice Johnson'")?;
    println!("✅ Updated {} rows\n", updated);

    // Step 7: Verify update
    println!("🔍 Querying Alice after update...");
    let alice = db.query("SELECT * FROM users WHERE name = 'Alice Johnson'", &[])?;
    println!("Alice's record: {:?}\n", alice[0]);

    // Step 8: Aggregates
    println!("📊 Running aggregate queries...");
    let count = db.query("SELECT COUNT(*) FROM users", &[])?;
    println!("Total users: {:?}", count[0]);

    let avg_age = db.query("SELECT AVG(age) FROM users", &[])?;
    println!("Average age: {:?}", avg_age[0]);

    let max_age = db.query("SELECT MAX(age) FROM users", &[])?;
    println!("Maximum age: {:?}\n", max_age[0]);

    // Step 9: ORDER BY and LIMIT
    println!("🔍 Top 2 users by age (descending)...");
    let top_users = db.query("SELECT * FROM users ORDER BY age DESC LIMIT 2", &[])?;
    for user in &top_users {
        println!("  {:?}", user);
    }
    println!();

    // Step 10: Transactions
    println!("💼 Testing transactions...");
    let tx = db.begin_transaction()?;
    println!("✅ Transaction started");

    // Transactions are committed when the handle is dropped
    // In real usage: tx.execute(...), then tx.commit()
    tx.commit()?;
    println!("✅ Transaction committed\n");

    // Step 11: Delete data
    println!("🗑️  Deleting users younger than 28...");
    let deleted = db.execute("DELETE FROM users WHERE age < 28")?;
    println!("✅ Deleted {} rows\n", deleted);

    // Step 12: Final count
    println!("🔍 Final user count...");
    let final_users = db.query("SELECT * FROM users", &[])?;
    println!("Remaining users: {}", final_users.len());
    for user in &final_users {
        println!("  {:?}", user);
    }
    println!();

    // Step 13: GROUP BY example
    println!("📊 Creating orders table for GROUP BY demo...");
    db.execute("CREATE TABLE orders (id INT, customer_id INT, amount INT, region TEXT)")?;
    db.execute("INSERT INTO orders VALUES (1, 1, 100, 'North')")?;
    db.execute("INSERT INTO orders VALUES (2, 1, 150, 'North')")?;
    db.execute("INSERT INTO orders VALUES (3, 2, 200, 'South')")?;
    db.execute("INSERT INTO orders VALUES (4, 2, 250, 'South')")?;
    db.execute("INSERT INTO orders VALUES (5, 3, 300, 'North')")?;
    println!("✅ Orders table created\n");

    println!("📊 GROUP BY region with aggregates...");
    let grouped = db.query("SELECT region, COUNT(*), SUM(amount) FROM orders GROUP BY region", &[])?;
    println!("Orders by region:");
    for row in &grouped {
        println!("  {:?}", row);
    }
    println!();

    // Step 14: JOIN example
    println!("🔗 Creating JOIN example...");
    db.execute("CREATE TABLE customers (id INT, name TEXT)")?;
    db.execute("INSERT INTO customers VALUES (1, 'Alice Johnson')")?;
    db.execute("INSERT INTO customers VALUES (2, 'Bob Smith')")?;
    db.execute("INSERT INTO customers VALUES (3, 'Charlie Brown')")?;
    println!("✅ Customers table created\n");

    println!("🔗 JOIN customers with orders...");
    let joined = db.query(
        "SELECT customers.name, orders.amount, orders.region
         FROM customers
         INNER JOIN orders ON customers.id = orders.customer_id",
        &[]
    )?;
    println!("Customer orders:");
    for row in &joined {
        println!("  {:?}", row);
    }
    println!();

    // Summary
    println!("═══════════════════════════════════════");
    println!("✅ Quickstart Complete!\n");
    println!("You've successfully:");
    println!("  • Created an in-memory database");
    println!("  • Created tables");
    println!("  • Inserted, queried, updated, and deleted data");
    println!("  • Used aggregates (COUNT, SUM, AVG, MAX)");
    println!("  • Used GROUP BY");
    println!("  • Used JOINs");
    println!("  • Used transactions");
    println!("  • Used ORDER BY and LIMIT");
    println!();
    println!("Next steps:");
    println!("  • Check out examples/encryption.rs for encryption");
    println!("  • Read docs/ORM_SUPPORT.md for ORM integration");
    println!("  • See TEST_REPORT.md for performance benchmarks");

    Ok(())
}
