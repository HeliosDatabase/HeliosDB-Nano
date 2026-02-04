//! Session Management Integration Tests
//!
//! Verifies creation, destruction, and metadata for user sessions.

#![cfg(test)]

use heliosdb_lite::{EmbeddedDatabase, Result};
use heliosdb_lite::session::IsolationLevel;

#[test]
fn test_session_lifecycle() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create session
    let session_id = db.create_session("alice", IsolationLevel::ReadCommitted)?;
    
    // Use session
    db.execute_in_session(session_id, "CREATE TABLE test (id INT)")?;
    
    // Destroy session
    db.destroy_session(session_id)?;
    
    // Using destroyed session should fail
    let result = db.execute_in_session(session_id, "INSERT INTO test VALUES (1)");
    assert!(result.is_err());

    Ok(())
}

#[test]
fn test_multiple_sessions_same_user() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    let s1 = db.create_session("alice", IsolationLevel::ReadCommitted)?;
    let s2 = db.create_session("alice", IsolationLevel::Serializable)?;

    assert_ne!(s1, s2);

    db.destroy_session(s1)?;
    db.destroy_session(s2)?;

    Ok(())
}