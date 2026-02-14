//! Transaction Isolation Integration Tests
//!
//! Verifies READ COMMITTED, REPEATABLE READ, and SERIALIZABLE levels.

#![cfg(test)]

use heliosdb_nano::{EmbeddedDatabase, Result};
use heliosdb_nano::session::IsolationLevel;
use crate::test_helpers::*;
use std::sync::Arc;

#[test]
#[ignore = "TODO: READ COMMITTED visibility not yet fully implemented - session2 should see committed changes"]
fn test_read_committed_visibility() -> Result<()> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    db.execute("CREATE TABLE test (id INT PRIMARY KEY, val TEXT)")?;
    db.execute("INSERT INTO test VALUES (1, 'initial')")?;

    let session1 = db.create_session("user1", IsolationLevel::ReadCommitted)?;
    let session2 = db.create_session("user2", IsolationLevel::ReadCommitted)?;

    db.begin_transaction_for_session(session1)?;
    db.execute_in_session(session1, "UPDATE test SET val = 'updated' WHERE id = 1")?;

    // Session 2 should still see 'initial' (not committed)
    db.begin_transaction_for_session(session2)?;
    let results = db.query_in_session(session2, "SELECT val FROM test WHERE id = 1", &[])?;
    assert_eq!(get_string_value(&results[0], 0), Some("initial".to_string()));

    db.commit_transaction_for_session(session1)?;

    // Session 2 should now see 'updated' (READ COMMITTED sees latest committed)
    // Actually, usually it sees it in the NEXT statement in the same transaction for RC.
    let results = db.query_in_session(session2, "SELECT val FROM test WHERE id = 1", &[])?;
    assert_eq!(get_string_value(&results[0], 0), Some("updated".to_string()));

    db.commit_transaction_for_session(session2)?;
    Ok(())
}

#[test]
fn test_repeatable_read_isolation() -> Result<()> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    db.execute("CREATE TABLE test (id INT PRIMARY KEY, val TEXT)")?;
    db.execute("INSERT INTO test VALUES (1, 'initial')")?;

    let session1 = db.create_session("user1", IsolationLevel::ReadCommitted)?;
    let session2 = db.create_session("user2", IsolationLevel::RepeatableRead)?;

    db.begin_transaction_for_session(session2)?;
    
    // Session 1 updates and commits
    db.execute_in_session(session1, "UPDATE test SET val = 'updated' WHERE id = 1")?;

    // Session 2 should still see 'initial' (consistent snapshot)
    let results = db.query_in_session(session2, "SELECT val FROM test WHERE id = 1", &[])?;
    assert_eq!(get_string_value(&results[0], 0), Some("initial".to_string()));

    db.commit_transaction_for_session(session2)?;
    Ok(())
}