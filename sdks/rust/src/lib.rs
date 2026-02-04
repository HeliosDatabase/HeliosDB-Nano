//! HeliosDB Rust Client
//!
//! A Rust client library for HeliosDB-Lite, the AI-native embedded database.
//!
//! # Example
//!
//! ```rust,no_run
//! use heliosdb_client::{Client, ClientConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), heliosdb_client::Error> {
//!     let client = Client::new(ClientConfig {
//!         base_url: "http://localhost:8080".to_string(),
//!         api_key: Some("your-api-key".to_string()),
//!         ..Default::default()
//!     })?;
//!
//!     // Execute SQL query
//!     let result = client.query("SELECT * FROM users WHERE id = $1", &[&1]).await?;
//!     println!("{:?}", result.rows);
//!
//!     // Vector search
//!     let results = client.vector_search("documents", "hello world")
//!         .top_k(10)
//!         .min_score(0.7)
//!         .execute()
//!         .await?;
//!
//!     Ok(())
//! }
//! ```

mod client;
mod error;
mod types;

pub use client::*;
pub use error::*;
pub use types::*;
