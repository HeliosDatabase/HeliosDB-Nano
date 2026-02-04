//! System tables for session tracking and monitoring
//!
//! Provides system tables and views for tracking active database sessions,
//! compatible with both PostgreSQL and Oracle conventions.

use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use crate::error::{Result, Error};

/// Session state enumeration
#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Active,
    Idle,
    IdleInTransaction,
}

impl SessionState {
    pub fn as_str(&self) -> &str {
        match self {
            SessionState::Active => "active",
            SessionState::Idle => "idle",
            SessionState::IdleInTransaction => "idle_in_transaction",
        }
    }
}

/// Protocol type for the session
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolType {
    PostgreSQL,
    Oracle,
}

impl ProtocolType {
    pub fn as_str(&self) -> &str {
        match self {
            ProtocolType::PostgreSQL => "PostgreSQL",
            ProtocolType::Oracle => "Oracle",
        }
    }
}

/// Session information
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: i64,
    pub protocol: ProtocolType,
    pub username: String,
    pub client_address: String,
    pub client_port: i32,
    pub connect_time: i64,
    pub last_activity: i64,
    pub current_query: Option<String>,
    pub state: SessionState,
}

impl SessionInfo {
    /// Create a new session
    pub fn new(
        session_id: i64,
        protocol: ProtocolType,
        username: String,
        client_address: String,
        client_port: i32,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        SessionInfo {
            session_id,
            protocol,
            username,
            client_address,
            client_port,
            connect_time: now,
            last_activity: now,
            current_query: None,
            state: SessionState::Idle,
        }
    }

    /// Update the current query and mark as active
    pub fn set_query(&mut self, query: String) {
        self.current_query = Some(query);
        self.state = SessionState::Active;
        self.update_activity();
    }

    /// Clear current query and mark as idle
    pub fn clear_query(&mut self) {
        self.current_query = None;
        self.state = SessionState::Idle;
        self.update_activity();
    }

    /// Update last activity timestamp
    pub fn update_activity(&mut self) {
        self.last_activity = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
    }
}

/// Session registry for tracking active sessions
pub struct SessionRegistry {
    sessions: Arc<RwLock<HashMap<i64, SessionInfo>>>,
    next_session_id: Arc<RwLock<i64>>,
}

impl SessionRegistry {
    /// Create a new session registry
    pub fn new() -> Self {
        SessionRegistry {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            next_session_id: Arc::new(RwLock::new(1)),
        }
    }

    /// Register a new session
    pub fn register_session(
        &self,
        protocol: ProtocolType,
        username: String,
        client_address: String,
        client_port: i32,
    ) -> Result<i64> {
        let mut next_id = self.next_session_id.write()
            .map_err(|e| Error::Generic(format!("Failed to acquire session ID lock: {}", e)))?;

        let session_id = *next_id;
        *next_id += 1;

        let session = SessionInfo::new(
            session_id,
            protocol,
            username,
            client_address,
            client_port,
        );

        let mut sessions = self.sessions.write()
            .map_err(|e| Error::Generic(format!("Failed to acquire sessions lock: {}", e)))?;

        sessions.insert(session_id, session);

        Ok(session_id)
    }

    /// Unregister a session
    pub fn unregister_session(&self, session_id: i64) -> Result<()> {
        let mut sessions = self.sessions.write()
            .map_err(|e| Error::Generic(format!("Failed to acquire sessions lock: {}", e)))?;

        sessions.remove(&session_id);
        Ok(())
    }

    /// Update session query
    pub fn update_session_query(&self, session_id: i64, query: String) -> Result<()> {
        let mut sessions = self.sessions.write()
            .map_err(|e| Error::Generic(format!("Failed to acquire sessions lock: {}", e)))?;

        if let Some(session) = sessions.get_mut(&session_id) {
            session.set_query(query);
        }

        Ok(())
    }

    /// Clear session query
    pub fn clear_session_query(&self, session_id: i64) -> Result<()> {
        let mut sessions = self.sessions.write()
            .map_err(|e| Error::Generic(format!("Failed to acquire sessions lock: {}", e)))?;

        if let Some(session) = sessions.get_mut(&session_id) {
            session.clear_query();
        }

        Ok(())
    }

    /// Get all sessions
    pub fn get_all_sessions(&self) -> Result<Vec<SessionInfo>> {
        let sessions = self.sessions.read()
            .map_err(|e| Error::Generic(format!("Failed to acquire sessions lock: {}", e)))?;

        Ok(sessions.values().cloned().collect())
    }

    /// Get sessions filtered by protocol
    pub fn get_sessions_by_protocol(&self, protocol: ProtocolType) -> Result<Vec<SessionInfo>> {
        let sessions = self.sessions.read()
            .map_err(|e| Error::Generic(format!("Failed to acquire sessions lock: {}", e)))?;

        Ok(sessions.values()
            .filter(|s| s.protocol == protocol)
            .cloned()
            .collect())
    }

    /// Get a specific session
    pub fn get_session(&self, session_id: i64) -> Result<Option<SessionInfo>> {
        let sessions = self.sessions.read()
            .map_err(|e| Error::Generic(format!("Failed to acquire sessions lock: {}", e)))?;

        Ok(sessions.get(&session_id).cloned())
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// System table definitions
pub struct SystemTables;

impl SystemTables {
    /// Get the helios_sessions table schema
    pub fn helios_sessions_schema() -> &'static str {
        r#"
        CREATE TABLE helios_sessions (
            session_id INT8 PRIMARY KEY,
            protocol TEXT NOT NULL,
            username TEXT NOT NULL,
            client_address TEXT NOT NULL,
            client_port INT4 NOT NULL,
            connect_time TIMESTAMP NOT NULL,
            last_activity TIMESTAMP NOT NULL,
            current_query TEXT,
            state TEXT NOT NULL
        )
        "#
    }

    /// Get the pg_stat_activity view definition (PostgreSQL compatibility)
    pub fn pg_stat_activity_view() -> &'static str {
        r#"
        CREATE VIEW pg_stat_activity AS
        SELECT
            session_id AS pid,
            username AS usename,
            'heliosdb' AS datname,
            client_address AS client_addr,
            client_port,
            connect_time AS backend_start,
            last_activity AS state_change,
            current_query AS query,
            state,
            protocol AS application_name
        FROM helios_sessions
        WHERE protocol = 'PostgreSQL'
        "#
    }

    /// Get the v$session view definition (Oracle compatibility)
    pub fn v_session_view() -> &'static str {
        r#"
        CREATE VIEW v$session AS
        SELECT
            session_id AS sid,
            session_id AS serial#,
            username,
            state AS status,
            client_address AS machine,
            protocol AS program,
            connect_time AS logon_time,
            last_activity AS last_call_et,
            current_query AS sql_text,
            CASE
                WHEN state = 'active' THEN 'ACTIVE'
                WHEN state = 'idle' THEN 'INACTIVE'
                ELSE 'SNIPED'
            END AS status
        FROM helios_sessions
        WHERE protocol = 'Oracle'
        "#
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let session = SessionInfo::new(
            1,
            ProtocolType::PostgreSQL,
            "test_user".to_string(),
            "127.0.0.1".to_string(),
            5432,
        );

        assert_eq!(session.session_id, 1);
        assert_eq!(session.protocol, ProtocolType::PostgreSQL);
        assert_eq!(session.username, "test_user");
        assert_eq!(session.state, SessionState::Idle);
    }

    #[test]
    fn test_session_registry() {
        let registry = SessionRegistry::new();

        // Register sessions
        let session_id1 = registry.register_session(
            ProtocolType::PostgreSQL,
            "user1".to_string(),
            "127.0.0.1".to_string(),
            5432,
        ).unwrap();

        let session_id2 = registry.register_session(
            ProtocolType::Oracle,
            "user2".to_string(),
            "127.0.0.1".to_string(),
            1521,
        ).unwrap();

        // Verify sessions are registered
        assert_eq!(session_id1, 1);
        assert_eq!(session_id2, 2);

        let all_sessions = registry.get_all_sessions().unwrap();
        assert_eq!(all_sessions.len(), 2);

        // Test protocol filtering
        let pg_sessions = registry.get_sessions_by_protocol(ProtocolType::PostgreSQL).unwrap();
        assert_eq!(pg_sessions.len(), 1);
        assert_eq!(pg_sessions[0].username, "user1");

        // Test query update
        registry.update_session_query(session_id1, "SELECT * FROM test".to_string()).unwrap();
        let session = registry.get_session(session_id1).unwrap().unwrap();
        assert_eq!(session.state, SessionState::Active);
        assert!(session.current_query.is_some());

        // Test query clear
        registry.clear_session_query(session_id1).unwrap();
        let session = registry.get_session(session_id1).unwrap().unwrap();
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.current_query.is_none());

        // Test unregister
        registry.unregister_session(session_id1).unwrap();
        let all_sessions = registry.get_all_sessions().unwrap();
        assert_eq!(all_sessions.len(), 1);
    }

    #[test]
    fn test_session_state_transitions() {
        let mut session = SessionInfo::new(
            1,
            ProtocolType::PostgreSQL,
            "test_user".to_string(),
            "127.0.0.1".to_string(),
            5432,
        );

        // Initial state
        assert_eq!(session.state, SessionState::Idle);

        // Set query
        session.set_query("SELECT 1".to_string());
        assert_eq!(session.state, SessionState::Active);
        assert!(session.current_query.is_some());

        // Clear query
        session.clear_query();
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.current_query.is_none());
    }
}
