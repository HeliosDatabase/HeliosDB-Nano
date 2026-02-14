//! Comprehensive server mode integration tests
//!
//! Tests for PostgreSQL-compatible server mode deployment.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_postgres::NoTls;
use heliosdb_nano::{EmbeddedDatabase, protocol::postgres::server::{PgServer, PgServerConfig}};

// Run with: cargo test --test server_mode_integration_test --lib

async fn setup_test_server() -> Result<(String, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    // Pick a random port (simplistic approach, might conflict but retries handled by OS usually)
    // Using port 0 lets OS pick, but we need to know which one it picked.
    // For simplicity in this test environment, we'll try to find a free port or use a range.
    // Actually, `PgServerConfig` takes a SocketAddr.
    
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let port = addr.port();
    
    // We drop the listener so the server can bind to it (small race condition window)
    drop(listener);
    
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    let config = PgServerConfig::with_address(addr);
    
    let server = PgServer::new(config, db)?;
    
    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("Server error: {}", e);
        }
    });
    
    // Give server a moment to start
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    Ok((format!("host=127.0.0.1 port={} user=postgres dbname=postgres", port), handle))
}

#[tokio::test]
#[ignore = "Server mode integration test - requires PostgreSQL wire protocol fixes"]
async fn test_server_connection() {
    let (conn_string, _handle) = setup_test_server().await.expect("Failed to setup server");
    
    let (client, connection) = tokio_postgres::connect(&conn_string, NoTls).await
        .expect("Failed to connect to server");
        
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    
    let rows = client.query("SELECT 1", &[]).await.expect("Failed to execute query");
    assert_eq!(rows.len(), 1);
    let value: i32 = rows[0].get(0);
    assert_eq!(value, 1);
}

#[tokio::test]
#[ignore = "Server mode integration test - stack overflow issue"]
async fn test_server_crud_operations() {
    let (conn_string, _handle) = setup_test_server().await.expect("Failed to setup server");
    
    let (client, connection) = tokio_postgres::connect(&conn_string, NoTls).await
        .expect("Failed to connect");
        
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    
    // Create Table
    client.execute("CREATE TABLE users (id INT, name TEXT)", &[]).await.expect("Create table failed");
    
    // Insert
    client.execute("INSERT INTO users VALUES ($1, $2)", &[&1i32, &"Alice"]).await.expect("Insert failed");
    
    // Select
    let rows = client.query("SELECT name FROM users WHERE id = $1", &[&1i32]).await.expect("Select failed");
    assert_eq!(rows.len(), 1);
    let name: String = rows[0].get(0);
    assert_eq!(name, "Alice");
    
    // Update
    client.execute("UPDATE users SET name = $1 WHERE id = $2", &[&"Bob", &1i32]).await.expect("Update failed");
    let rows = client.query("SELECT name FROM users WHERE id = $1", &[&1i32]).await.expect("Select after update failed");
    assert_eq!(rows[0].get::<_, String>(0), "Bob");
    
    // Delete
    client.execute("DELETE FROM users WHERE id = $1", &[&1i32]).await.expect("Delete failed");
    let rows = client.query("SELECT * FROM users", &[]).await.expect("Select after delete failed");
    assert_eq!(rows.len(), 0);
}

#[tokio::test]
#[ignore = "Server mode integration test - requires PostgreSQL wire protocol fixes"]
async fn test_server_transaction_handling() {
    let (conn_string, _handle) = setup_test_server().await.expect("Failed to setup server");
    
    let (mut client, connection) = tokio_postgres::connect(&conn_string, NoTls).await
        .expect("Failed to connect");
        
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    
    client.execute("CREATE TABLE accounts (id INT, balance INT)", &[]).await.unwrap();
    client.execute("INSERT INTO accounts VALUES (1, 100)", &[]).await.unwrap();
    
    // Test Rollback
    let tx = client.transaction().await.unwrap();
    tx.execute("UPDATE accounts SET balance = 200 WHERE id = 1", &[]).await.unwrap();
    tx.rollback().await.unwrap();
    
    let rows = client.query("SELECT balance FROM accounts WHERE id = 1", &[]).await.unwrap();
    let balance: i32 = rows[0].get(0);
    assert_eq!(balance, 100); // Should remain 100
    
    // Test Commit
    let tx = client.transaction().await.unwrap();
    tx.execute("UPDATE accounts SET balance = 300 WHERE id = 1", &[]).await.unwrap();
    tx.commit().await.unwrap();
    
    let rows = client.query("SELECT balance FROM accounts WHERE id = 1", &[]).await.unwrap();
    let balance: i32 = rows[0].get(0);
    assert_eq!(balance, 300); // Should be 300
}

#[tokio::test]
#[ignore = "Server mode integration test - requires PostgreSQL wire protocol fixes"]
async fn test_server_concurrent_clients() {
    let (conn_string, _handle) = setup_test_server().await.expect("Failed to setup server");
    let conn_string = Arc::new(conn_string);
    
    // Initialize DB
    {
        let (client, connection) = tokio_postgres::connect(&conn_string, NoTls).await.unwrap();
        tokio::spawn(async move { if let Err(e) = connection.await { eprintln!("{}", e); } });
        client.execute("CREATE TABLE concurrent (id INT PRIMARY KEY)", &[]).await.unwrap();
    }
    
    let mut handles = vec![];
    
    for i in 0..10 {
        let cs = conn_string.clone();
        handles.push(tokio::spawn(async move {
            let (client, connection) = tokio_postgres::connect(&cs, NoTls).await.unwrap();
            tokio::spawn(async move { if let Err(e) = connection.await { eprintln!("{}", e); } });
            
            client.execute("INSERT INTO concurrent VALUES ($1)", &[&(i as i32)]).await.unwrap();
        }));
    }
    
    for h in handles {
        h.await.unwrap();
    }
    
    // Verify count
    let (client, connection) = tokio_postgres::connect(&conn_string, NoTls).await.unwrap();
    tokio::spawn(async move { if let Err(e) = connection.await { eprintln!("{}", e); } });
    
    let rows = client.query("SELECT COUNT(*) FROM concurrent", &[]).await.unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 10);
}