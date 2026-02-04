//! Model Context Protocol (MCP) Server for HeliosDB-Lite
//!
//! Provides MCP server implementation for Claude and other AI assistants.
//!
//! # Configuration
//!
//! Add to your Claude config:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "heliosdb": {
//!       "command": "heliosdb-lite",
//!       "args": ["mcp-server", "--db", "./mydb"],
//!       "env": {}
//!     }
//!   }
//! }
//! ```
//!
//! # Available Tools
//!
//! - `heliosdb_query` - Execute SQL queries
//! - `heliosdb_schema` - Get table schemas
//! - `heliosdb_create_table` - Create tables
//! - `heliosdb_insert` - Insert data
//! - `heliosdb_branch` - Manage branches
//! - `heliosdb_search` - Semantic vector search

pub mod server;
pub mod tools;
pub mod protocol;

pub use server::McpServer;
