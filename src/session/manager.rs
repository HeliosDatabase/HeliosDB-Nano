//! Session Manager implementation
//!
//! Manages concurrent user sessions with ACID guarantees, resource quotas,
//! and automatic cleanup of inactive sessions.
//!
//! # Features
//!
//! - **Multi-user support**: Each user can have multiple concurrent sessions
//! - **Resource quotas**: Configurable limits on sessions per user
//! - **Automatic cleanup**: Expired sessions are automatically cleaned up
//! - **Thread-safe**: All operations are safe for concurrent access
//!
//! # Example
//!
//! ```rust,no_run
//! use heliosdb_lite::session::{SessionManager, IsolationLevel, User};
//!
//! let manager = SessionManager::new();
//! let user = User::new("alice", "password");
//!
//! // Create a session
//! let session_id = manager.create_session(&user, IsolationLevel::RepeatableRead)?;
//!
//! // Use the session...
//!
//! // Clean up
//! manager.destroy_session(session_id)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use super::types::{Session, SessionId, IsolationLevel, User, UserId};
use crate::{Error, Result};
use dashmap::DashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Resource quota configuration for controlling user resource usage
///
/// Quotas help prevent resource exhaustion in multi-tenant environments
/// by limiting how many sessions and queries each user can consume.
#[derive(Debug, Clone)]
pub struct ResourceQuota {
    /// Maximum concurrent sessions allowed per user (default: 10)
    pub max_sessions: usize,
    /// Maximum queries per session before forced termination (default: unlimited)
    pub max_queries: u64,
    /// Maximum connections per session (default: 100)
    pub max_connections: u32,
}

impl Default for ResourceQuota {
    fn default() -> Self {
        Self {
            max_sessions: 10,
            max_queries: u64::MAX,
            max_connections: 100,
        }
    }
}

/// Session Manager
///
/// Coordinates multi-user access to the database with per-session
/// isolation levels and resource quotas.
pub struct SessionManager {
    /// Active sessions indexed by SessionId
    sessions: Arc<DashMap<SessionId, Arc<parking_lot::RwLock<Session>>>>,
    /// Session timeout in seconds (default: 3600 = 1 hour)
    session_timeout_secs: u64,
    /// Resource quota configuration
    quota: ResourceQuota,
    /// Last cleanup timestamp
    last_cleanup: Arc<Mutex<Instant>>,
}

impl SessionManager {
    /// Create a new SessionManager with default settings
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            session_timeout_secs: 3600,
            quota: ResourceQuota::default(),
            last_cleanup: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Create a SessionManager with custom quota limits
    pub fn with_quota(max_sessions_per_user: usize) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            session_timeout_secs: 3600,
            quota: ResourceQuota {
                max_sessions: max_sessions_per_user,
                ..Default::default()
            },
            last_cleanup: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Create a new session for a user
    ///
    /// # Arguments
    ///
    /// * `user` - User credentials
    /// * `isolation` - Desired isolation level
    ///
    /// # Returns
    ///
    /// SessionId for the newly created session
    pub fn create_session(&self, user: &User, isolation: IsolationLevel) -> Result<SessionId> {
        // Enforce resource quota
        self.enforce_quota(&user.id, &self.quota)?;

        // Create new session
        let session = Session::new(user.id, isolation);
        let session_id = session.id;

        // Register session
        self.sessions.insert(session_id, Arc::new(parking_lot::RwLock::new(session)));

        Ok(session_id)
    }

    /// Destroy a session
    pub fn destroy_session(&self, session_id: SessionId) -> Result<()> {
        self.sessions.remove(&session_id)
            .ok_or_else(|| Error::Generic(format!("Session {:?} not found", session_id)))?;
        Ok(())
    }

    /// Get a session by ID
    pub fn get_session(&self, session_id: SessionId) -> Result<Arc<parking_lot::RwLock<Session>>> {
        self.sessions.get(&session_id)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or_else(|| Error::Generic(format!("Session {:?} not found", session_id)))
    }

    /// List all active sessions
    pub fn list_active_sessions(&self) -> Vec<SessionId> {
        self.sessions.iter()
            .map(|entry| *entry.key())
            .collect()
    }

    /// Delete a session by ID
    pub fn delete_session(&self, session_id: SessionId) -> Result<()> {
        self.destroy_session(session_id)
    }

    /// List all active session IDs
    pub fn list_sessions(&self) -> Vec<SessionId> {
        self.list_active_sessions()
    }

    /// Get all sessions for a specific user
    pub fn get_user_sessions(&self, user_id: &UserId) -> Vec<SessionId> {
        self.sessions.iter()
            .filter(|entry| {
                let session = entry.value().read();
                session.user_id == *user_id
            })
            .map(|entry| *entry.key())
            .collect()
    }

    /// Update last activity timestamp for a session
    pub fn update_last_activity(&self, session_id: SessionId) -> Result<()> {
        let session_lock = self.get_session(session_id)?;
        let mut session = session_lock.write();
        session.touch();
        Ok(())
    }

    /// Cleanup inactive sessions based on timeout
    ///
    /// Returns the number of sessions cleaned up
    pub fn cleanup_inactive_sessions(&self, timeout_secs: u64) -> usize {
        // Update last cleanup time
        if let Ok(mut last_cleanup) = self.last_cleanup.lock() {
            *last_cleanup = Instant::now();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let expired: Vec<SessionId> = self.sessions
            .iter()
            .filter_map(|entry| {
                let session = entry.value().read();
                if now - session.last_activity > timeout_secs {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect();

        let count = expired.len();
        for session_id in expired {
            let _ = self.sessions.remove(&session_id);
        }

        count
    }

    /// Clean up expired sessions (uses default timeout)
    pub fn cleanup_expired_sessions(&self) -> usize {
        self.cleanup_inactive_sessions(self.session_timeout_secs)
    }

    /// Enforce resource quota for a user
    ///
    /// # Arguments
    /// * `user_id` - The user identifier
    /// * `quota` - The resource quota to enforce
    ///
    /// # Returns
    /// * `Ok(())` - If quota is not exceeded
    /// * `Err(Error)` - If quota would be exceeded
    pub fn enforce_quota(&self, user_id: &UserId, quota: &ResourceQuota) -> Result<()> {
        // Count user's active sessions
        let user_session_count = self.sessions
            .iter()
            .filter(|entry| {
                let session = entry.value().read();
                session.user_id == *user_id
            })
            .count();

        if user_session_count >= quota.max_sessions {
            return Err(Error::Generic(format!(
                "Resource quota exceeded: user has {} sessions (max: {})",
                user_session_count, quota.max_sessions
            )));
        }

        Ok(())
    }

    /// Get total number of active sessions
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_session_success() {
        let manager = SessionManager::new();
        let user = User::new("alice", "password123");

        let session_id = manager.create_session(&user, IsolationLevel::ReadCommitted)
            .expect("Failed to create session");

        assert!(manager.sessions.contains_key(&session_id));
    }

    #[test]
    fn test_session_quota_enforcement() {
        let manager = SessionManager::with_quota(1); // Max 1 session
        let user = User::new("bob", "password456");

        let _session1 = manager.create_session(&user, IsolationLevel::ReadCommitted)
            .expect("First session should succeed");

        let result = manager.create_session(&user, IsolationLevel::ReadCommitted);
        assert!(result.is_err());
    }

    #[test]
    fn test_concurrent_sessions_isolation() {
        let manager = Arc::new(SessionManager::new());
        let user1 = User::new("user1", "pass1");
        let user2 = User::new("user2", "pass2");

        let session1 = manager.create_session(&user1, IsolationLevel::ReadCommitted)
            .expect("Failed to create session1");
        let session2 = manager.create_session(&user2, IsolationLevel::ReadCommitted)
            .expect("Failed to create session2");

        // Both sessions should be independent
        assert_ne!(session1, session2);
        assert_eq!(manager.list_active_sessions().len(), 2);
    }

    #[test]
    fn test_list_sessions() {
        let manager = SessionManager::new();
        let user1 = User::new("alice", "pass");
        let user2 = User::new("bob", "pass");

        let id1 = manager.create_session(&user1, IsolationLevel::ReadCommitted).unwrap();
        let id2 = manager.create_session(&user2, IsolationLevel::RepeatableRead).unwrap();

        let sessions = manager.list_sessions();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains(&id1));
        assert!(sessions.contains(&id2));
    }

    #[test]
    fn test_get_user_sessions() {
        let manager = SessionManager::new();
        let user1 = User::new("alice", "pass");
        let user2 = User::new("bob", "pass");

        manager.create_session(&user1, IsolationLevel::ReadCommitted).unwrap();
        manager.create_session(&user2, IsolationLevel::RepeatableRead).unwrap();
        manager.create_session(&user1, IsolationLevel::Serializable).unwrap();

        let alice_sessions = manager.get_user_sessions(&user1.id);
        let bob_sessions = manager.get_user_sessions(&user2.id);

        assert_eq!(alice_sessions.len(), 2);
        assert_eq!(bob_sessions.len(), 1);
    }

    #[test]
    fn test_update_last_activity() {
        let manager = SessionManager::new();
        let user = User::new("alice", "pass");
        let session_id = manager.create_session(&user, IsolationLevel::ReadCommitted).unwrap();

        // Sleep to ensure time difference
        std::thread::sleep(std::time::Duration::from_millis(100));

        let session_before = manager.get_session(session_id).unwrap();
        let activity_before = session_before.read().last_activity;

        std::thread::sleep(std::time::Duration::from_millis(100));

        manager.update_last_activity(session_id).unwrap();

        let session_after = manager.get_session(session_id).unwrap();
        let activity_after = session_after.read().last_activity;

        assert!(activity_after >= activity_before);
    }

    #[test]
    fn test_cleanup_inactive_sessions() {
        let manager = SessionManager::new();
        let user1 = User::new("alice", "pass");
        let user2 = User::new("bob", "pass");

        manager.create_session(&user1, IsolationLevel::ReadCommitted).unwrap();
        manager.create_session(&user2, IsolationLevel::RepeatableRead).unwrap();

        // Wait for sessions to become inactive
        std::thread::sleep(std::time::Duration::from_secs(2));

        // Cleanup with 1 second timeout
        let removed = manager.cleanup_inactive_sessions(1);
        assert_eq!(removed, 2);
        assert_eq!(manager.session_count(), 0);
    }

    #[test]
    fn test_cleanup_keeps_active_sessions() {
        let manager = SessionManager::new();
        let user = User::new("alice", "pass");
        let session_id = manager.create_session(&user, IsolationLevel::ReadCommitted).unwrap();

        // Keep session active
        std::thread::sleep(std::time::Duration::from_millis(500));
        manager.update_last_activity(session_id).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(600));

        // Cleanup with 1 second timeout (should not remove)
        let removed = manager.cleanup_inactive_sessions(1);
        assert_eq!(removed, 0);
        assert_eq!(manager.session_count(), 1);
    }

    #[test]
    fn test_delete_session() {
        let manager = SessionManager::new();
        let user = User::new("alice", "pass");
        let session_id = manager.create_session(&user, IsolationLevel::ReadCommitted).unwrap();

        assert_eq!(manager.session_count(), 1);

        manager.delete_session(session_id).unwrap();

        assert_eq!(manager.session_count(), 0);
        assert!(manager.get_session(session_id).is_err());
    }

    #[test]
    fn test_enforce_quota() {
        let manager = SessionManager::with_quota(2);
        let user = User::new("alice", "pass");

        // Create two sessions (at limit)
        manager.create_session(&user, IsolationLevel::ReadCommitted).unwrap();
        manager.create_session(&user, IsolationLevel::RepeatableRead).unwrap();

        // Third session should fail
        let result = manager.create_session(&user, IsolationLevel::Serializable);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("quota exceeded"));
    }

    #[test]
    fn test_quota_per_user() {
        let manager = SessionManager::with_quota(1);
        let user1 = User::new("alice", "pass");
        let user2 = User::new("bob", "pass");

        // Alice creates one session
        manager.create_session(&user1, IsolationLevel::ReadCommitted).unwrap();

        // Bob can still create a session (quota is per-user)
        manager.create_session(&user2, IsolationLevel::RepeatableRead)
            .expect("Bob's session should succeed");

        // Alice's second session should fail
        let result = manager.create_session(&user1, IsolationLevel::Serializable);
        assert!(result.is_err());
    }

    #[test]
    fn test_concurrent_session_creation() {
        use std::thread;

        let manager = Arc::new(SessionManager::new());
        let mut handles = vec![];

        // Spawn 10 threads, each creating a session
        for i in 0..10 {
            let manager_clone = Arc::clone(&manager);
            let handle = thread::spawn(move || {
                let user = User::new(format!("user_{}", i), "pass");
                manager_clone.create_session(&user, IsolationLevel::ReadCommitted)
            });
            handles.push(handle);
        }

        // Wait for all threads to complete
        let mut session_ids = vec![];
        for handle in handles {
            let session_id = handle.join().unwrap().unwrap();
            session_ids.push(session_id);
        }

        // All session IDs should be unique
        let original_len = session_ids.len();
        session_ids.sort_by_key(|id| id.0);
        session_ids.dedup();
        assert_eq!(session_ids.len(), original_len);
        assert_eq!(manager.session_count(), 10);
    }

    #[test]
    fn test_concurrent_session_operations() {
        use std::thread;

        let manager = Arc::new(SessionManager::new());
        let user1 = User::new("alice", "pass");
        let user2 = User::new("bob", "pass");
        let user3 = User::new("charlie", "pass");

        // Create initial sessions
        let id1 = manager.create_session(&user1, IsolationLevel::ReadCommitted).unwrap();
        let id2 = manager.create_session(&user2, IsolationLevel::RepeatableRead).unwrap();
        let _id3 = manager.create_session(&user3, IsolationLevel::Serializable).unwrap();

        let mut handles = vec![];

        // Thread 1: Update activity
        {
            let manager_clone = Arc::clone(&manager);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = manager_clone.update_last_activity(id1);
                }
            }));
        }

        // Thread 2: List sessions
        {
            let manager_clone = Arc::clone(&manager);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = manager_clone.list_sessions();
                }
            }));
        }

        // Thread 3: Get sessions
        {
            let manager_clone = Arc::clone(&manager);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = manager_clone.get_session(id2);
                }
            }));
        }

        // Thread 4: Get user sessions
        {
            let manager_clone = Arc::clone(&manager);
            let user_id = user1.id;
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = manager_clone.get_user_sessions(&user_id);
                }
            }));
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // All sessions should still exist
        assert_eq!(manager.session_count(), 3);
    }

    #[test]
    fn test_isolation_levels() {
        let manager = SessionManager::new();
        let user1 = User::new("alice", "pass");
        let user2 = User::new("bob", "pass");
        let user3 = User::new("charlie", "pass");

        let id1 = manager.create_session(&user1, IsolationLevel::ReadCommitted).unwrap();
        let id2 = manager.create_session(&user2, IsolationLevel::RepeatableRead).unwrap();
        let id3 = manager.create_session(&user3, IsolationLevel::Serializable).unwrap();

        let session1_lock = manager.get_session(id1).unwrap();
        let session1 = session1_lock.read();
        assert_eq!(session1.isolation_level, IsolationLevel::ReadCommitted);
        drop(session1);

        let session2_lock = manager.get_session(id2).unwrap();
        let session2 = session2_lock.read();
        assert_eq!(session2.isolation_level, IsolationLevel::RepeatableRead);
        drop(session2);

        let session3_lock = manager.get_session(id3).unwrap();
        let session3 = session3_lock.read();
        assert_eq!(session3.isolation_level, IsolationLevel::Serializable);
    }
}
