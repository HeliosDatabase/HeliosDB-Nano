//! Session Migrate - TR (Transaction Replay)
//!
//! Saves and restores session state after failover.
//! Includes SET parameters, timezone, search_path, and custom variables.

use super::{NodeId, ProxyError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Session state information
#[derive(Debug, Clone)]
pub struct SessionState {
    /// Session ID
    pub session_id: Uuid,
    /// User name
    pub user: String,
    /// Database name
    pub database: String,
    /// Application name
    pub application_name: Option<String>,
    /// Client encoding
    pub client_encoding: String,
    /// Server encoding
    pub server_encoding: String,
    /// Timezone
    pub timezone: String,
    /// Search path
    pub search_path: Vec<String>,
    /// DateStyle
    pub datestyle: String,
    /// IntervalStyle
    pub intervalstyle: String,
    /// Custom SET parameters
    pub custom_parameters: HashMap<String, String>,
    /// Session-local temporary tables
    pub temp_tables: Vec<TempTableInfo>,
    /// Prepared statements
    pub prepared_statements: HashMap<String, PreparedStatementInfo>,
    /// Session created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last activity timestamp
    pub last_activity: chrono::DateTime<chrono::Utc>,
    /// Original node
    pub original_node: NodeId,
}

/// Temporary table information
#[derive(Debug, Clone)]
pub struct TempTableInfo {
    /// Table name
    pub name: String,
    /// Schema (usually pg_temp_N)
    pub schema: String,
    /// Column definitions
    pub columns: Vec<ColumnDef>,
    /// Has data that needs migration
    pub has_data: bool,
    /// Row count (if known)
    pub row_count: Option<u64>,
}

/// Column definition
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// Column name
    pub name: String,
    /// Column type
    pub data_type: String,
    /// Is nullable
    pub nullable: bool,
    /// Default value expression
    pub default_expr: Option<String>,
}

/// Prepared statement information
#[derive(Debug, Clone)]
pub struct PreparedStatementInfo {
    /// Statement name
    pub name: String,
    /// SQL query
    pub query: String,
    /// Parameter types
    pub param_types: Vec<String>,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl SessionState {
    /// Create a new session state
    pub fn new(session_id: Uuid, user: String, database: String, node: NodeId) -> Self {
        Self {
            session_id,
            user,
            database,
            application_name: None,
            client_encoding: "UTF8".to_string(),
            server_encoding: "UTF8".to_string(),
            timezone: "UTC".to_string(),
            search_path: vec!["public".to_string()],
            datestyle: "ISO, MDY".to_string(),
            intervalstyle: "postgres".to_string(),
            custom_parameters: HashMap::new(),
            temp_tables: Vec::new(),
            prepared_statements: HashMap::new(),
            created_at: chrono::Utc::now(),
            last_activity: chrono::Utc::now(),
            original_node: node,
        }
    }

    /// Set a custom parameter
    pub fn set_parameter(&mut self, name: String, value: String) {
        // Handle well-known parameters
        match name.to_lowercase().as_str() {
            "timezone" => self.timezone = value,
            "search_path" => {
                self.search_path = value.split(',').map(|s| s.trim().to_string()).collect()
            }
            "client_encoding" => self.client_encoding = value,
            "datestyle" => self.datestyle = value,
            "intervalstyle" => self.intervalstyle = value,
            "application_name" => self.application_name = Some(value),
            _ => {
                self.custom_parameters.insert(name, value);
            }
        }
        self.last_activity = chrono::Utc::now();
    }

    /// Get a parameter value
    pub fn get_parameter(&self, name: &str) -> Option<String> {
        match name.to_lowercase().as_str() {
            "timezone" => Some(self.timezone.clone()),
            "search_path" => Some(self.search_path.join(", ")),
            "client_encoding" => Some(self.client_encoding.clone()),
            "server_encoding" => Some(self.server_encoding.clone()),
            "datestyle" => Some(self.datestyle.clone()),
            "intervalstyle" => Some(self.intervalstyle.clone()),
            "application_name" => self.application_name.clone(),
            _ => self.custom_parameters.get(name).cloned(),
        }
    }

    /// Add a prepared statement
    pub fn add_prepared_statement(&mut self, info: PreparedStatementInfo) {
        self.prepared_statements.insert(info.name.clone(), info);
        self.last_activity = chrono::Utc::now();
    }

    /// Remove a prepared statement
    pub fn remove_prepared_statement(&mut self, name: &str) {
        self.prepared_statements.remove(name);
    }

    /// Add a temp table
    pub fn add_temp_table(&mut self, info: TempTableInfo) {
        self.temp_tables.push(info);
        self.last_activity = chrono::Utc::now();
    }

    /// Generate SET statements to restore session
    pub fn generate_restore_statements(&self) -> Vec<String> {
        let mut statements = Vec::new();

        // Core parameters
        statements.push(format!("SET timezone TO '{}'", self.timezone));
        statements.push(format!(
            "SET search_path TO {}",
            self.search_path.join(", ")
        ));
        statements.push(format!("SET client_encoding TO '{}'", self.client_encoding));
        statements.push(format!("SET datestyle TO '{}'", self.datestyle));
        statements.push(format!("SET intervalstyle TO '{}'", self.intervalstyle));

        if let Some(ref app_name) = self.application_name {
            statements.push(format!("SET application_name TO '{}'", app_name));
        }

        // Custom parameters
        for (name, value) in &self.custom_parameters {
            statements.push(format!("SET {} TO '{}'", name, value));
        }

        // Prepared statements
        for prep in self.prepared_statements.values() {
            if prep.param_types.is_empty() {
                statements.push(format!("PREPARE {} AS {}", prep.name, prep.query));
            } else {
                statements.push(format!(
                    "PREPARE {} ({}) AS {}",
                    prep.name,
                    prep.param_types.join(", "),
                    prep.query
                ));
            }
        }

        statements
    }
}

/// Session migration result
#[derive(Debug, Clone)]
pub struct SessionMigrateResult {
    /// Session ID
    pub session_id: Uuid,
    /// Migration succeeded
    pub success: bool,
    /// Target node
    pub target_node: NodeId,
    /// SET statements executed
    pub parameters_restored: usize,
    /// Prepared statements restored
    pub prepared_statements_restored: usize,
    /// Temp tables (attempted) migration
    pub temp_tables_migrated: usize,
    /// Temp tables that failed to migrate
    pub temp_tables_failed: usize,
    /// Migration time (ms)
    pub duration_ms: u64,
    /// Error (if failed)
    pub error: Option<String>,
}

/// Session Migrate Manager
pub struct SessionMigrate {
    /// Saved session states
    sessions: Arc<RwLock<HashMap<Uuid, SessionState>>>,
    /// Whether session migration is enabled
    enabled: bool,
    /// Migrate temp tables (expensive)
    migrate_temp_tables: bool,
    /// Maximum sessions to track
    max_sessions: usize,
}

impl SessionMigrate {
    /// Create a new session migrate manager
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            enabled: true,
            migrate_temp_tables: false, // Disabled by default (expensive)
            max_sessions: 10000,
        }
    }

    /// Configure max sessions
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max;
        self
    }

    /// Enable/disable temp table migration
    pub fn with_temp_table_migration(mut self, enabled: bool) -> Self {
        self.migrate_temp_tables = enabled;
        self
    }

    /// Enable or disable session migration
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Register a new session
    pub async fn register_session(&self, state: SessionState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let session_id = state.session_id;

        // Check limit
        {
            let sessions = self.sessions.read().await;
            if sessions.len() >= self.max_sessions && !sessions.contains_key(&session_id) {
                return Err(ProxyError::SessionMigration(format!(
                    "Maximum sessions ({}) exceeded",
                    self.max_sessions
                )));
            }
        }

        self.sessions.write().await.insert(session_id, state);
        tracing::debug!("Registered session {:?}", session_id);

        Ok(())
    }

    /// Update session parameter
    pub async fn set_parameter(
        &self,
        session_id: Uuid,
        name: String,
        value: String,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&session_id).ok_or_else(|| {
            ProxyError::SessionMigration(format!("Session {:?} not found", session_id))
        })?;

        session.set_parameter(name, value);
        Ok(())
    }

    /// Add prepared statement to session
    pub async fn add_prepared_statement(
        &self,
        session_id: Uuid,
        info: PreparedStatementInfo,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&session_id).ok_or_else(|| {
            ProxyError::SessionMigration(format!("Session {:?} not found", session_id))
        })?;

        session.add_prepared_statement(info);
        Ok(())
    }

    /// Remove prepared statement from session
    pub async fn remove_prepared_statement(&self, session_id: Uuid, name: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.remove_prepared_statement(name);
        }
        Ok(())
    }

    /// Add temp table to session
    pub async fn add_temp_table(&self, session_id: Uuid, info: TempTableInfo) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&session_id).ok_or_else(|| {
            ProxyError::SessionMigration(format!("Session {:?} not found", session_id))
        })?;

        session.add_temp_table(info);
        Ok(())
    }

    /// Get session state
    pub async fn get_session(&self, session_id: &Uuid) -> Option<SessionState> {
        self.sessions.read().await.get(session_id).cloned()
    }

    /// Close session
    pub async fn close_session(&self, session_id: &Uuid) {
        self.sessions.write().await.remove(session_id);
        tracing::debug!("Closed session {:?}", session_id);
    }

    /// Migrate session to a new node
    pub async fn migrate_session(
        &self,
        session_id: Uuid,
        target_node: NodeId,
    ) -> Result<SessionMigrateResult> {
        let start = std::time::Instant::now();

        let session = self.get_session(&session_id).await.ok_or_else(|| {
            ProxyError::SessionMigration(format!("Session {:?} not found", session_id))
        })?;

        // Generate restore statements
        let statements = session.generate_restore_statements();

        // Execute SET statements
        let mut parameters_restored = 0;
        let mut prepared_statements_restored = 0;

        for stmt in &statements {
            match self.execute_statement(target_node, stmt).await {
                Ok(()) => {
                    if stmt.starts_with("SET ") {
                        parameters_restored += 1;
                    } else if stmt.starts_with("PREPARE ") {
                        prepared_statements_restored += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to execute restore statement: {} - {}", stmt, e);
                }
            }
        }

        // Migrate temp tables if enabled
        let mut temp_tables_migrated = 0;
        let mut temp_tables_failed = 0;

        if self.migrate_temp_tables {
            for table in &session.temp_tables {
                match self.migrate_temp_table(target_node, table).await {
                    Ok(()) => temp_tables_migrated += 1,
                    Err(e) => {
                        temp_tables_failed += 1;
                        tracing::warn!(
                            "Failed to migrate temp table {}: {}",
                            table.name,
                            e
                        );
                    }
                }
            }
        }

        // Update session's node
        {
            let mut sessions = self.sessions.write().await;
            if let Some(s) = sessions.get_mut(&session_id) {
                s.original_node = target_node;
                s.last_activity = chrono::Utc::now();
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "Migrated session {:?} to node {:?}: {} params, {} prepared, {}ms",
            session_id,
            target_node,
            parameters_restored,
            prepared_statements_restored,
            duration_ms
        );

        Ok(SessionMigrateResult {
            session_id,
            success: true,
            target_node,
            parameters_restored,
            prepared_statements_restored,
            temp_tables_migrated,
            temp_tables_failed,
            duration_ms,
            error: None,
        })
    }

    /// Execute a statement on target node (stub)
    async fn execute_statement(&self, _node: NodeId, _stmt: &str) -> Result<()> {
        // TODO: Implement actual statement execution
        // For skeleton, simulate success
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        Ok(())
    }

    /// Migrate a temp table (stub)
    async fn migrate_temp_table(&self, _node: NodeId, table: &TempTableInfo) -> Result<()> {
        // TODO: Implement actual temp table migration
        // 1. CREATE TEMP TABLE on target
        // 2. Copy data if has_data
        // 3. Verify row count

        tracing::debug!("Migrating temp table: {}", table.name);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        Ok(())
    }

    /// Get statistics
    pub async fn stats(&self) -> SessionMigrateStats {
        let sessions = self.sessions.read().await;

        let total_prepared: usize = sessions
            .values()
            .map(|s| s.prepared_statements.len())
            .sum();

        let total_temp_tables: usize = sessions.values().map(|s| s.temp_tables.len()).sum();

        SessionMigrateStats {
            active_sessions: sessions.len(),
            total_prepared_statements: total_prepared,
            total_temp_tables,
            enabled: self.enabled,
            temp_table_migration_enabled: self.migrate_temp_tables,
        }
    }
}

impl Default for SessionMigrate {
    fn default() -> Self {
        Self::new()
    }
}

/// Session migrate statistics
#[derive(Debug, Clone)]
pub struct SessionMigrateStats {
    /// Active sessions tracked
    pub active_sessions: usize,
    /// Total prepared statements across sessions
    pub total_prepared_statements: usize,
    /// Total temp tables across sessions
    pub total_temp_tables: usize,
    /// Whether session migration is enabled
    pub enabled: bool,
    /// Whether temp table migration is enabled
    pub temp_table_migration_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_state_new() {
        let session_id = Uuid::new_v4();
        let node_id = NodeId::new();
        let state = SessionState::new(session_id, "user".to_string(), "db".to_string(), node_id);

        assert_eq!(state.user, "user");
        assert_eq!(state.database, "db");
        assert_eq!(state.timezone, "UTC");
        assert_eq!(state.search_path, vec!["public"]);
    }

    #[test]
    fn test_set_get_parameter() {
        let mut state = SessionState::new(
            Uuid::new_v4(),
            "user".to_string(),
            "db".to_string(),
            NodeId::new(),
        );

        state.set_parameter("timezone".to_string(), "America/New_York".to_string());
        assert_eq!(state.get_parameter("timezone"), Some("America/New_York".to_string()));

        state.set_parameter("custom_param".to_string(), "custom_value".to_string());
        assert_eq!(state.get_parameter("custom_param"), Some("custom_value".to_string()));
    }

    #[test]
    fn test_generate_restore_statements() {
        let mut state = SessionState::new(
            Uuid::new_v4(),
            "user".to_string(),
            "db".to_string(),
            NodeId::new(),
        );

        state.set_parameter("timezone".to_string(), "UTC".to_string());
        state.add_prepared_statement(PreparedStatementInfo {
            name: "my_query".to_string(),
            query: "SELECT * FROM users WHERE id = $1".to_string(),
            param_types: vec!["integer".to_string()],
            created_at: chrono::Utc::now(),
        });

        let statements = state.generate_restore_statements();

        assert!(statements.iter().any(|s| s.contains("timezone")));
        assert!(statements.iter().any(|s| s.contains("PREPARE my_query")));
    }

    #[tokio::test]
    async fn test_register_session() {
        let migrate = SessionMigrate::new();
        let session_id = Uuid::new_v4();
        let state = SessionState::new(session_id, "user".to_string(), "db".to_string(), NodeId::new());

        migrate.register_session(state).await.unwrap();

        let session = migrate.get_session(&session_id).await;
        assert!(session.is_some());
    }

    #[tokio::test]
    async fn test_set_parameter() {
        let migrate = SessionMigrate::new();
        let session_id = Uuid::new_v4();
        let state = SessionState::new(session_id, "user".to_string(), "db".to_string(), NodeId::new());

        migrate.register_session(state).await.unwrap();
        migrate
            .set_parameter(session_id, "timezone".to_string(), "Europe/London".to_string())
            .await
            .unwrap();

        let session = migrate.get_session(&session_id).await.unwrap();
        assert_eq!(session.timezone, "Europe/London");
    }

    #[tokio::test]
    async fn test_migrate_session() {
        let migrate = SessionMigrate::new();
        let session_id = Uuid::new_v4();
        let state = SessionState::new(session_id, "user".to_string(), "db".to_string(), NodeId::new());

        migrate.register_session(state).await.unwrap();

        let target = NodeId::new();
        let result = migrate.migrate_session(session_id, target).await.unwrap();

        assert!(result.success);
        assert!(result.parameters_restored > 0);
    }

    #[tokio::test]
    async fn test_close_session() {
        let migrate = SessionMigrate::new();
        let session_id = Uuid::new_v4();
        let state = SessionState::new(session_id, "user".to_string(), "db".to_string(), NodeId::new());

        migrate.register_session(state).await.unwrap();
        migrate.close_session(&session_id).await;

        assert!(migrate.get_session(&session_id).await.is_none());
    }

    #[tokio::test]
    async fn test_stats() {
        let migrate = SessionMigrate::new();
        let session_id = Uuid::new_v4();
        let mut state = SessionState::new(session_id, "user".to_string(), "db".to_string(), NodeId::new());

        state.add_prepared_statement(PreparedStatementInfo {
            name: "ps1".to_string(),
            query: "SELECT 1".to_string(),
            param_types: vec![],
            created_at: chrono::Utc::now(),
        });

        migrate.register_session(state).await.unwrap();

        let stats = migrate.stats().await;
        assert_eq!(stats.active_sessions, 1);
        assert_eq!(stats.total_prepared_statements, 1);
    }
}
