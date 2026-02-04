//! PostgreSQL server example with extended query protocol support
//!
//! This example demonstrates how to run HeliosDB-Lite as a PostgreSQL-compatible
//! network server with full extended query protocol support (prepared statements).
//!
//! ## Usage
//!
//! ```bash
//! # Start the server
//! cargo run --example postgres_server_extended
//!
//! # In another terminal, connect with psql
//! psql -h 127.0.0.1 -p 5432 -U postgres
//!
//! # Or use Python with psycopg2
//! python3 -c "
//! import psycopg2
//! conn = psycopg2.connect(host='127.0.0.1', port=5432, user='postgres')
//! cur = conn.cursor()
//! cur.execute('SELECT 1')
//! print(cur.fetchone())
//! "
//! ```

use heliosdb_lite::{EmbeddedDatabase, protocol::postgres::{PgServerBuilder, AuthMethod}};
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for debugging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,heliosdb_lite=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Create in-memory database
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);

    // Create sample schema and data
    db.execute("CREATE TABLE users (id INT, name TEXT, email TEXT)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice', 'alice@example.com')")?;
    db.execute("INSERT INTO users VALUES (2, 'Bob', 'bob@example.com')")?;
    db.execute("INSERT INTO users VALUES (3, 'Charlie', 'charlie@example.com')")?;

    db.execute("CREATE TABLE products (id INT, name TEXT, price FLOAT8)")?;
    db.execute("INSERT INTO products VALUES (1, 'Laptop', 999.99)")?;
    db.execute("INSERT INTO products VALUES (2, 'Mouse', 29.99)")?;
    db.execute("INSERT INTO products VALUES (3, 'Keyboard', 79.99)")?;

    tracing::info!("Database initialized with sample data");

    // Build PostgreSQL server
    let server = PgServerBuilder::new()
        .address("127.0.0.1:5432".parse()?)
        .auth_method(AuthMethod::Trust) // No password for development
        .max_connections(100)
        .build(db)?;

    println!("\n=======================================================");
    println!("  HeliosDB-Lite PostgreSQL Server");
    println!("=======================================================");
    println!("  Address:        127.0.0.1:5432");
    println!("  Auth:           Trust (no password)");
    println!("  Protocol:       PostgreSQL Wire Protocol v3.0");
    println!("  Extended Query: ✓ Enabled (prepared statements)");
    println!("=======================================================\n");

    println!("Connect with:");
    println!("  psql -h 127.0.0.1 -p 5432 -U postgres\n");

    println!("Try these queries:");
    println!("  SELECT * FROM users;");
    println!("  SELECT * FROM products WHERE price < 100;");
    println!("  BEGIN; INSERT INTO users VALUES (4, 'David', 'david@example.com'); COMMIT;\n");

    println!("Test prepared statements (Python):");
    println!("  import psycopg2");
    println!("  conn = psycopg2.connect(host='127.0.0.1', port=5432, user='postgres')");
    println!("  cur = conn.cursor()");
    println!("  cur.execute('SELECT * FROM users WHERE id = %s', (1,))");
    println!("  print(cur.fetchone())\n");

    println!("Press Ctrl+C to stop the server\n");

    // Start server (runs until error or Ctrl+C)
    server.serve().await?;

    Ok(())
}
