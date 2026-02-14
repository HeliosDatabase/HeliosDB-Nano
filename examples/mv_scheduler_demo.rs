//! CPU-Aware Materialized View Scheduler Demo
//!
//! This example demonstrates the complete lifecycle of the MV scheduler:
//! 1. Creating materialized views
//! 2. Configuring the scheduler
//! 3. Scheduling refreshes with different priorities
//! 4. Monitoring scheduler statistics
//! 5. Handling base table changes
//!
//! Run with:
//! ```bash
//! cargo run --example mv_scheduler_demo
//! ```

use heliosdb_nano::{
    EmbeddedDatabase, Config,
    storage::{MVScheduler, SchedulerConfig, Priority},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    println!("=== CPU-Aware MV Scheduler Demo ===\n");

    // Create in-memory database
    let db = EmbeddedDatabase::new_in_memory()?;
    let storage = Arc::clone(&db.storage);

    // Step 1: Create base tables
    println!("1. Creating base tables...");
    db.execute("CREATE TABLE users (id INT, name TEXT, status TEXT)")?;
    db.execute("CREATE TABLE orders (id INT, user_id INT, amount FLOAT, order_date TEXT)")?;
    db.execute("CREATE TABLE products (id INT, name TEXT, category TEXT, price FLOAT)")?;

    // Insert sample data
    db.execute("INSERT INTO users VALUES (1, 'Alice', 'active')")?;
    db.execute("INSERT INTO users VALUES (2, 'Bob', 'active')")?;
    db.execute("INSERT INTO users VALUES (3, 'Charlie', 'inactive')")?;

    db.execute("INSERT INTO orders VALUES (1, 1, 100.0, '2024-01-01')")?;
    db.execute("INSERT INTO orders VALUES (2, 1, 200.0, '2024-01-02')")?;
    db.execute("INSERT INTO orders VALUES (3, 2, 150.0, '2024-01-03')")?;

    db.execute("INSERT INTO products VALUES (1, 'Laptop', 'Electronics', 999.99)")?;
    db.execute("INSERT INTO products VALUES (2, 'Mouse', 'Electronics', 29.99)")?;
    db.execute("INSERT INTO products VALUES (3, 'Desk', 'Furniture', 299.99)")?;

    println!("   ✓ Tables created and populated\n");

    // Step 2: Create materialized views
    println!("2. Creating materialized views...");

    // Critical MV: User summary (frequently accessed)
    db.execute(
        "CREATE MATERIALIZED VIEW user_summary AS \
         SELECT status, COUNT(*) as count FROM users GROUP BY status"
    )?;

    // High priority MV: Order statistics
    db.execute(
        "CREATE MATERIALIZED VIEW order_stats AS \
         SELECT user_id, COUNT(*) as order_count, SUM(amount) as total_amount \
         FROM orders GROUP BY user_id"
    )?;

    // Normal priority MV: Product by category
    db.execute(
        "CREATE MATERIALIZED VIEW product_summary AS \
         SELECT category, COUNT(*) as count, AVG(price) as avg_price \
         FROM products GROUP BY category"
    )?;

    // Low priority MV: Detailed product info
    db.execute(
        "CREATE MATERIALIZED VIEW product_details AS \
         SELECT * FROM products WHERE price > 100"
    )?;

    println!("   ✓ Created 4 materialized views\n");

    // Step 3: Configure and start scheduler
    println!("3. Configuring CPU-aware scheduler...");

    let scheduler_config = SchedulerConfig::default()
        .with_max_cpu_percent(75.0)         // Pause if CPU > 75%
        .with_check_interval(2)              // Check every 2 seconds
        .with_batch_size(5)                  // Process up to 5 MVs per cycle
        .with_max_concurrent(3)              // Max 3 concurrent refreshes
        .with_adaptive_batch_sizing(true)    // Adjust batch size dynamically
        .with_auto_retry(true);              // Retry failed refreshes

    let scheduler = MVScheduler::new(scheduler_config, Arc::clone(&storage));

    println!("   ✓ Scheduler configured:");
    println!("     - Max CPU: 75%");
    println!("     - Check interval: 2s");
    println!("     - Max concurrent: 3");
    println!("     - Adaptive batch sizing: enabled\n");

    // Start scheduler background loop
    let scheduler_clone = scheduler.clone();
    let scheduler_handle = tokio::spawn(async move {
        if let Err(e) = scheduler_clone.run().await {
            eprintln!("Scheduler error: {}", e);
        }
    });

    // Give scheduler time to start
    sleep(Duration::from_millis(100)).await;

    // Step 4: Schedule refreshes with different priorities
    println!("4. Scheduling MV refreshes...");

    scheduler.schedule_refresh("user_summary", Priority::Critical)?;
    println!("   ✓ Scheduled 'user_summary' with CRITICAL priority");

    scheduler.schedule_refresh("order_stats", Priority::High)?;
    println!("   ✓ Scheduled 'order_stats' with HIGH priority");

    scheduler.schedule_refresh("product_summary", Priority::Normal)?;
    println!("   ✓ Scheduled 'product_summary' with NORMAL priority");

    scheduler.schedule_refresh("product_details", Priority::Low)?;
    println!("   ✓ Scheduled 'product_details' with LOW priority\n");

    // Step 5: Monitor scheduler statistics
    println!("5. Monitoring scheduler activity...\n");

    for i in 0..10 {
        let stats = scheduler.get_stats();
        println!(
            "   [{}s] Queue: {} tasks, Running: {} tasks, CPU: {:.1}%",
            i * 2,
            stats.queue_size,
            stats.running_tasks,
            stats.cpu_usage
        );

        if stats.queue_size == 0 && stats.running_tasks == 0 {
            println!("\n   ✓ All tasks completed!");
            break;
        }

        sleep(Duration::from_secs(2)).await;
    }

    // Step 6: Demonstrate base table change triggering
    println!("\n6. Testing base table change triggers...");

    // Modify users table
    db.execute("INSERT INTO users VALUES (4, 'David', 'active')")?;
    println!("   ✓ Inserted new user");

    // Trigger dependent MV refreshes
    scheduler.on_base_table_change("users")?;
    println!("   ✓ Triggered refresh for MVs depending on 'users' table");

    let stats = scheduler.get_stats();
    println!("   → Queue size after trigger: {} tasks\n", stats.queue_size);

    // Wait for triggered refreshes to complete
    for i in 0..5 {
        let stats = scheduler.get_stats();
        println!(
            "   [{}s] Queue: {} tasks, Running: {} tasks",
            i * 2,
            stats.queue_size,
            stats.running_tasks
        );

        if stats.queue_size == 0 && stats.running_tasks == 0 {
            println!("\n   ✓ Triggered refreshes completed!");
            break;
        }

        sleep(Duration::from_secs(2)).await;
    }

    // Step 7: Query refreshed materialized views
    println!("\n7. Querying refreshed materialized views...");

    let user_summary = db.query("SELECT * FROM user_summary", &[])?;
    println!("\n   User Summary:");
    for row in user_summary {
        println!("     {:?}", row);
    }

    let order_stats = db.query("SELECT * FROM order_stats", &[])?;
    println!("\n   Order Statistics:");
    for row in order_stats {
        println!("     {:?}", row);
    }

    let product_summary = db.query("SELECT * FROM product_summary", &[])?;
    println!("\n   Product Summary:");
    for row in product_summary {
        println!("     {:?}", row);
    }

    // Step 8: Demonstrate CPU threshold behavior
    println!("\n8. Testing CPU threshold enforcement...");
    println!("   (Scheduler will pause if CPU > 75%)");

    // Schedule multiple refreshes
    for i in 0..5 {
        scheduler.schedule_refresh(&format!("test_mv_{}", i), Priority::Normal).ok();
    }

    let stats = scheduler.get_stats();
    println!("   → Current CPU usage: {:.1}%", stats.cpu_usage);

    if stats.cpu_usage > 75.0 {
        println!("   ⚠ CPU above threshold - scheduler will throttle");
    } else {
        println!("   ✓ CPU within limits - scheduler active");
    }

    // Clean up
    scheduler_handle.abort();

    println!("\n=== Demo Complete ===");
    println!("\nKey Features Demonstrated:");
    println!("  ✓ Priority-based scheduling (CRITICAL → HIGH → NORMAL → LOW)");
    println!("  ✓ CPU usage monitoring and throttling");
    println!("  ✓ Concurrent task execution with limits");
    println!("  ✓ Adaptive batch sizing based on system load");
    println!("  ✓ Base table change triggers");
    println!("  ✓ Real-time statistics monitoring");

    Ok(())
}
