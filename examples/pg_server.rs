//! PostgreSQL wire protocol server example
//!
//! Run with: cargo run --example pg_server
//! Connect with: psql -h localhost -p 5432 -U postgres

use heliosdb_lite::{EmbeddedDatabase, network::PgServer};
use std::sync::Arc;
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("info,heliosdb_lite=debug")
        .init();

    // Create in-memory database
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);

    // Create some initial data for testing
    db.execute("CREATE TABLE users (id INTEGER, name TEXT, email TEXT)")?;
    db.execute("INSERT INTO users (id, name, email) VALUES (1, 'Alice', 'alice@example.com')")?;
    db.execute("INSERT INTO users (id, name, email) VALUES (2, 'Bob', 'bob@example.com')")?;
    db.execute("INSERT INTO users (id, name, email) VALUES (3, 'Charlie', 'charlie@example.com')")?;

    println!("HeliosDB Lite PostgreSQL Server");
    println!("================================");
    println!();
    println!("Database initialized with sample data:");
    println!("  Table: users (id, name, email)");
    println!("  Rows: 3");
    println!();
    println!("Starting server on 127.0.0.1:5432...");
    println!();
    println!("Connect with:");
    println!("  psql -h localhost -p 5432 -U postgres");
    println!("  Password: postgres (or any password)");
    println!();
    println!("Try some queries:");
    println!("  SELECT * FROM users;");
    println!("  SELECT name, email FROM users WHERE id = 1;");
    println!("  INSERT INTO users VALUES (4, 'David', 'david@example.com');");
    println!();

    // Create and run server
    let server = PgServer::new("127.0.0.1:5432", db);

    // Run with Ctrl+C shutdown
    let shutdown = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        println!("\nShutdown signal received");
    };

    server.run_with_shutdown(shutdown).await?;

    Ok(())
}
