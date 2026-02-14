//! PostgreSQL SSL/TLS Server Example
//!
//! This example demonstrates how to set up a PostgreSQL-compatible server
//! with SSL/TLS encryption support.
//!
//! ## Usage
//!
//! 1. Generate test certificates (or use existing ones):
//!    ```bash
//!    mkdir -p certs
//!    openssl req -x509 -newkey rsa:2048 -nodes \
//!      -keyout certs/server.key \
//!      -out certs/server.crt \
//!      -days 365 \
//!      -subj "/CN=localhost"
//!    ```
//!
//! 2. Run the server:
//!    ```bash
//!    cargo run --example postgres_server_ssl
//!    ```
//!
//! 3. Connect with psql using SSL:
//!    ```bash
//!    # Require SSL connection
//!    psql "sslmode=require host=127.0.0.1 port=5432 user=postgres dbname=heliosdb"
//!
//!    # Allow SSL but don't require it
//!    psql "sslmode=prefer host=127.0.0.1 port=5432 user=postgres dbname=heliosdb"
//!
//!    # Disable SSL
//!    psql "sslmode=disable host=127.0.0.1 port=5432 user=postgres dbname=heliosdb"
//!    ```
//!
//! ## SSL Modes
//!
//! - `Disable`: SSL connections are disabled
//! - `Allow`: Accept both SSL and non-SSL connections
//! - `Prefer`: Prefer SSL but allow non-SSL fallback
//! - `Require`: Require SSL connections (no fallback)
//! - `VerifyCA`: Require SSL and verify client certificate against CA
//! - `VerifyFull`: Require SSL and verify client certificate with hostname

use heliosdb_nano::{EmbeddedDatabase, Result};
use heliosdb_nano::protocol::postgres::{
    PgServerBuilder, SslConfig, SslMode, CertificateManager,
    AuthMethod, AuthManager, InMemoryPasswordStore, SharedPasswordStore, PasswordStore
};
use std::sync::Arc;
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("info,heliosdb_nano=debug")
        .init();

    println!("HeliosDB-Lite PostgreSQL SSL/TLS Server");
    println!("========================================\n");

    // Setup test certificates (auto-generates if not present)
    println!("Setting up SSL/TLS certificates...");
    let (cert_path, key_path) = CertificateManager::setup_test_certs()?;
    println!("  Certificate: {}", cert_path);
    println!("  Private Key: {}\n", key_path);

    // Create database
    println!("Creating in-memory database...");
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    println!("Database created\n");

    // Create sample data
    println!("Creating sample data...");
    db.execute(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL,
            email TEXT NOT NULL
        )"
    )?;

    db.execute(
        "INSERT INTO users (id, username, email) VALUES
        (1, 'alice', 'alice@example.com'),
        (2, 'bob', 'bob@example.com'),
        (3, 'charlie', 'charlie@example.com')"
    )?;
    println!("Sample data created\n");

    // Configure SSL modes (choose one)

    // Option 1: Allow mode (accepts both SSL and non-SSL)
    let ssl_config = SslConfig::new(
        SslMode::Allow,
        &cert_path,
        &key_path,
    );
    println!("SSL Mode: Allow (accepts both SSL and non-SSL connections)");

    // Option 2: Require mode (SSL required)
    // let ssl_config = SslConfig::new(
    //     SslMode::Require,
    //     &cert_path,
    //     &key_path,
    // );
    // println!("SSL Mode: Require (SSL connections only)");

    // Option 3: Prefer mode (prefers SSL but allows fallback)
    // let ssl_config = SslConfig::new(
    //     SslMode::Prefer,
    //     &cert_path,
    //     &key_path,
    // );
    // println!("SSL Mode: Prefer (prefers SSL but allows non-SSL)");

    // Configure Authentication
    println!("\nConfiguring authentication...");

    // Option 1: Trust mode (no password) - for development only
    // let auth_method = AuthMethod::Trust;

    // Option 2: SCRAM-SHA-256 (recommended for production)
    let mut password_store = InMemoryPasswordStore::new();
    password_store.add_user("postgres", "postgres").unwrap();
    password_store.add_user("admin", "secure_password").unwrap();
    password_store.add_user("alice", "alice123").unwrap();

    let shared_store = SharedPasswordStore::new(password_store);
    let auth_manager = AuthManager::with_password_store(AuthMethod::ScramSha256, shared_store);

    println!("  Authentication: SCRAM-SHA-256");
    println!("  Users configured: postgres, admin, alice");

    // Build server with SSL and SCRAM authentication
    let addr: std::net::SocketAddr = "127.0.0.1:5432".parse()
        .map_err(|e| heliosdb_nano::Error::config(format!("Invalid address: {}", e)))?;
    let server = PgServerBuilder::new()
        .address(addr)
        .auth_manager(auth_manager)
        .ssl_config(ssl_config)
        .build(db)?;

    println!("\nServer Configuration:");
    println!("  Address: {}", server.config().address);
    println!("  Authentication: {:?}", server.config().auth_method);
    println!("  SSL Enabled: {}", server.config().ssl_config.is_some());
    if let Some(ref ssl_cfg) = server.config().ssl_config {
        println!("  SSL Mode: {:?}", ssl_cfg.mode);
    }

    println!("\n{}", "=".repeat(50));
    println!("Server is ready for connections!");
    println!("{}", "=".repeat(50));
    println!("\nConnect with psql using SCRAM-SHA-256 authentication:");
    println!("\n  # With SSL and authentication:");
    println!("  psql \"sslmode=require host=127.0.0.1 port=5432 user=postgres password=postgres dbname=heliosdb\"");
    println!("\n  # Alternative users:");
    println!("  psql \"sslmode=require host=127.0.0.1 port=5432 user=admin password=secure_password dbname=heliosdb\"");
    println!("  psql \"sslmode=require host=127.0.0.1 port=5432 user=alice password=alice123 dbname=heliosdb\"");
    println!("\n  # Interactive password prompt:");
    println!("  psql \"sslmode=require host=127.0.0.1 port=5432 user=postgres dbname=heliosdb\"");
    println!("\nExample queries:");
    println!("  SELECT * FROM users;");
    println!("  SELECT * FROM users WHERE id = 1;");
    println!("\nSecurity Features:");
    println!("  ✓ SSL/TLS encryption");
    println!("  ✓ SCRAM-SHA-256 authentication (RFC 7677)");
    println!("  ✓ Salted password hashing (PBKDF2-HMAC-SHA-256)");
    println!("  ✓ No plaintext password storage");
    println!("  ✓ Timing attack resistance");
    println!("\nPress Ctrl+C to stop the server\n");

    // Start server (blocks until error or shutdown)
    server.serve().await
}
