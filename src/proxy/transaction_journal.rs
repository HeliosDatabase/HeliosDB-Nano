//! Transaction Journal - TR (Transaction Replay)
//!
//! Logs all statements within a transaction for replay after failover.
//! Enables Oracle-grade TAF+TAC merged functionality.

use super::{NodeId, ProxyError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Journal entry for a single statement
#[derive(Debug, Clone)]
pub struct JournalEntry {
    /// Entry sequence number
    pub sequence: u64,
    /// SQL statement text
    pub statement: String,
    /// Bound parameters
    pub parameters: Vec<JournalValue>,
    /// Result checksum (for verification after replay)
    pub result_checksum: Option<u64>,
    /// Number of rows affected
    pub rows_affected: Option<u64>,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Statement type
    pub statement_type: StatementType,
    /// Execution duration (ms)
    pub duration_ms: u64,
}

/// Serializable parameter value
#[derive(Debug, Clone)]
pub enum JournalValue {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<JournalValue>),
}

/// Statement type classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementType {
    /// SELECT query
    Select,
    /// INSERT statement
    Insert,
    /// UPDATE statement
    Update,
    /// DELETE statement
    Delete,
    /// DDL (CREATE, ALTER, DROP)
    Ddl,
    /// Transaction control (BEGIN, COMMIT, ROLLBACK)
    Transaction,
    /// SET statement
    Set,
    /// Other/unknown
    Other,
}

impl StatementType {
    /// Determine statement type from SQL
    pub fn from_sql(sql: &str) -> Self {
        let upper = sql.trim().to_uppercase();
        if upper.starts_with("SELECT") {
            StatementType::Select
        } else if upper.starts_with("INSERT") {
            StatementType::Insert
        } else if upper.starts_with("UPDATE") {
            StatementType::Update
        } else if upper.starts_with("DELETE") {
            StatementType::Delete
        } else if upper.starts_with("CREATE")
            || upper.starts_with("ALTER")
            || upper.starts_with("DROP")
        {
            StatementType::Ddl
        } else if upper.starts_with("BEGIN")
            || upper.starts_with("COMMIT")
            || upper.starts_with("ROLLBACK")
            || upper.starts_with("SAVEPOINT")
        {
            StatementType::Transaction
        } else if upper.starts_with("SET") {
            StatementType::Set
        } else {
            StatementType::Other
        }
    }

    /// Is this a read-only statement?
    pub fn is_read_only(&self) -> bool {
        matches!(self, StatementType::Select)
    }

    /// Is this a mutating statement?
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            StatementType::Insert | StatementType::Update | StatementType::Delete | StatementType::Ddl
        )
    }
}

/// Transaction journal for a single transaction
#[derive(Debug, Clone)]
pub struct TransactionJournalEntry {
    /// Transaction ID
    pub tx_id: Uuid,
    /// Session ID
    pub session_id: Uuid,
    /// Node where transaction started
    pub node_id: NodeId,
    /// Transaction start time
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Start LSN (for WAL synchronization)
    pub start_lsn: u64,
    /// Journal entries
    pub entries: Vec<JournalEntry>,
    /// Current sequence
    pub current_sequence: u64,
    /// Is transaction active
    pub active: bool,
    /// Has mutations
    pub has_mutations: bool,
    /// Savepoints
    pub savepoints: Vec<Savepoint>,
}

/// Savepoint information
#[derive(Debug, Clone)]
pub struct Savepoint {
    /// Savepoint name
    pub name: String,
    /// Sequence at savepoint
    pub sequence: u64,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl TransactionJournalEntry {
    /// Create a new transaction journal entry
    pub fn new(tx_id: Uuid, session_id: Uuid, node_id: NodeId, start_lsn: u64) -> Self {
        Self {
            tx_id,
            session_id,
            node_id,
            started_at: chrono::Utc::now(),
            start_lsn,
            entries: Vec::new(),
            current_sequence: 0,
            active: true,
            has_mutations: false,
            savepoints: Vec::new(),
        }
    }

    /// Add an entry to the journal
    pub fn add_entry(&mut self, entry: JournalEntry) {
        if entry.statement_type.is_mutation() {
            self.has_mutations = true;
        }
        self.current_sequence = entry.sequence;
        self.entries.push(entry);
    }

    /// Create a savepoint
    pub fn create_savepoint(&mut self, name: String) {
        self.savepoints.push(Savepoint {
            name,
            sequence: self.current_sequence,
            created_at: chrono::Utc::now(),
        });
    }

    /// Rollback to savepoint
    pub fn rollback_to_savepoint(&mut self, name: &str) -> Option<u64> {
        if let Some(idx) = self.savepoints.iter().position(|s| s.name == name) {
            let savepoint = &self.savepoints[idx];
            let sequence = savepoint.sequence;

            // Truncate entries after savepoint
            self.entries.retain(|e| e.sequence <= sequence);

            // Remove later savepoints
            self.savepoints.truncate(idx + 1);

            Some(sequence)
        } else {
            None
        }
    }

    /// Get entries for replay
    pub fn entries_for_replay(&self) -> Vec<&JournalEntry> {
        self.entries.iter().collect()
    }

    /// Get only mutation entries
    pub fn mutation_entries(&self) -> Vec<&JournalEntry> {
        self.entries
            .iter()
            .filter(|e| e.statement_type.is_mutation())
            .collect()
    }

    /// Calculate total size of journal
    pub fn total_size(&self) -> usize {
        self.entries
            .iter()
            .map(|e| e.statement.len() + estimate_params_size(&e.parameters))
            .sum()
    }
}

fn estimate_params_size(params: &[JournalValue]) -> usize {
    params
        .iter()
        .map(|p| match p {
            JournalValue::Null => 1,
            JournalValue::Bool(_) => 1,
            JournalValue::Int64(_) => 8,
            JournalValue::Float64(_) => 8,
            JournalValue::Text(s) => s.len(),
            JournalValue::Bytes(b) => b.len(),
            JournalValue::Array(a) => estimate_params_size(a),
        })
        .sum()
}

/// Transaction Journal Manager
pub struct TransactionJournal {
    /// Active transaction journals
    journals: Arc<RwLock<HashMap<Uuid, TransactionJournalEntry>>>,
    /// Maximum entries per journal
    max_entries: usize,
    /// Maximum journal size (bytes)
    max_size: usize,
    /// Whether journaling is enabled
    enabled: bool,
}

impl TransactionJournal {
    /// Create a new transaction journal manager
    pub fn new() -> Self {
        Self {
            journals: Arc::new(RwLock::new(HashMap::new())),
            max_entries: 10000,
            max_size: 64 * 1024 * 1024, // 64MB
            enabled: true,
        }
    }

    /// Configure maximum entries
    pub fn with_max_entries(mut self, max: usize) -> Self {
        self.max_entries = max;
        self
    }

    /// Configure maximum size
    pub fn with_max_size(mut self, max: usize) -> Self {
        self.max_size = max;
        self
    }

    /// Enable or disable journaling
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Start journaling a transaction
    pub async fn begin_transaction(
        &self,
        tx_id: Uuid,
        session_id: Uuid,
        node_id: NodeId,
        start_lsn: u64,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let journal = TransactionJournalEntry::new(tx_id, session_id, node_id, start_lsn);
        self.journals.write().await.insert(tx_id, journal);

        tracing::debug!("Started journaling transaction {:?}", tx_id);
        Ok(())
    }

    /// Log a statement
    pub async fn log_statement(
        &self,
        tx_id: Uuid,
        statement: String,
        parameters: Vec<JournalValue>,
        result_checksum: Option<u64>,
        rows_affected: Option<u64>,
        duration_ms: u64,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut journals = self.journals.write().await;
        let journal = journals.get_mut(&tx_id).ok_or_else(|| {
            ProxyError::Internal(format!("No journal for transaction {:?}", tx_id))
        })?;

        // Check limits
        if journal.entries.len() >= self.max_entries {
            return Err(ProxyError::Internal("Transaction journal entries limit exceeded".to_string()));
        }

        if journal.total_size() >= self.max_size {
            return Err(ProxyError::Internal("Transaction journal size limit exceeded".to_string()));
        }

        let sequence = journal.current_sequence + 1;
        let statement_type = StatementType::from_sql(&statement);

        let entry = JournalEntry {
            sequence,
            statement,
            parameters,
            result_checksum,
            rows_affected,
            timestamp: chrono::Utc::now(),
            statement_type,
            duration_ms,
        };

        journal.add_entry(entry);

        Ok(())
    }

    /// Create a savepoint
    pub async fn create_savepoint(&self, tx_id: Uuid, name: String) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut journals = self.journals.write().await;
        let journal = journals.get_mut(&tx_id).ok_or_else(|| {
            ProxyError::Internal(format!("No journal for transaction {:?}", tx_id))
        })?;

        journal.create_savepoint(name);
        Ok(())
    }

    /// Rollback to savepoint
    pub async fn rollback_to_savepoint(&self, tx_id: Uuid, name: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let mut journals = self.journals.write().await;
        let journal = journals.get_mut(&tx_id).ok_or_else(|| {
            ProxyError::Internal(format!("No journal for transaction {:?}", tx_id))
        })?;

        journal
            .rollback_to_savepoint(name)
            .ok_or_else(|| ProxyError::Internal(format!("Savepoint '{}' not found", name)))?;

        Ok(())
    }

    /// Commit transaction (clear journal)
    pub async fn commit_transaction(&self, tx_id: Uuid) -> Result<()> {
        self.journals.write().await.remove(&tx_id);
        tracing::debug!("Committed and cleared journal for transaction {:?}", tx_id);
        Ok(())
    }

    /// Rollback transaction (clear journal)
    pub async fn rollback_transaction(&self, tx_id: Uuid) -> Result<()> {
        self.journals.write().await.remove(&tx_id);
        tracing::debug!("Rolled back and cleared journal for transaction {:?}", tx_id);
        Ok(())
    }

    /// Get journal for a transaction (for replay)
    pub async fn get_journal(&self, tx_id: &Uuid) -> Option<TransactionJournalEntry> {
        self.journals.read().await.get(tx_id).cloned()
    }

    /// Get active transaction count
    pub async fn active_count(&self) -> usize {
        self.journals.read().await.len()
    }

    /// Get statistics
    pub async fn stats(&self) -> JournalStats {
        let journals = self.journals.read().await;
        let total_entries: usize = journals.values().map(|j| j.entries.len()).sum();
        let total_size: usize = journals.values().map(|j| j.total_size()).sum();

        JournalStats {
            active_transactions: journals.len(),
            total_entries,
            total_size_bytes: total_size,
            enabled: self.enabled,
        }
    }

    /// Get all active transaction journals (for failover replay)
    pub async fn get_all_active(&self) -> Vec<TransactionJournalEntry> {
        self.journals.read().await.values().cloned().collect()
    }

    /// Get the maximum start LSN across all active transactions
    /// Used to determine how far the standby needs to catch up
    pub async fn get_max_start_lsn(&self) -> Option<u64> {
        let journals = self.journals.read().await;
        journals.values().map(|j| j.start_lsn).max()
    }

    /// Get transactions that started on a specific node
    /// Useful for replaying only transactions affected by a node failure
    pub async fn get_transactions_for_node(&self, node_id: NodeId) -> Vec<TransactionJournalEntry> {
        self.journals
            .read()
            .await
            .values()
            .filter(|j| j.node_id == node_id)
            .cloned()
            .collect()
    }
}

impl Default for TransactionJournal {
    fn default() -> Self {
        Self::new()
    }
}

/// Journal statistics
#[derive(Debug, Clone)]
pub struct JournalStats {
    /// Number of active transactions being journaled
    pub active_transactions: usize,
    /// Total journal entries across all transactions
    pub total_entries: usize,
    /// Total size of journals in bytes
    pub total_size_bytes: usize,
    /// Whether journaling is enabled
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_statement_type_detection() {
        assert_eq!(StatementType::from_sql("SELECT * FROM users"), StatementType::Select);
        assert_eq!(StatementType::from_sql("INSERT INTO users VALUES (1)"), StatementType::Insert);
        assert_eq!(StatementType::from_sql("UPDATE users SET name = 'x'"), StatementType::Update);
        assert_eq!(StatementType::from_sql("DELETE FROM users"), StatementType::Delete);
        assert_eq!(StatementType::from_sql("CREATE TABLE foo (id INT)"), StatementType::Ddl);
        assert_eq!(StatementType::from_sql("BEGIN"), StatementType::Transaction);
        assert_eq!(StatementType::from_sql("SET search_path = public"), StatementType::Set);
    }

    #[test]
    fn test_statement_type_properties() {
        assert!(StatementType::Select.is_read_only());
        assert!(!StatementType::Insert.is_read_only());

        assert!(StatementType::Insert.is_mutation());
        assert!(StatementType::Update.is_mutation());
        assert!(!StatementType::Select.is_mutation());
    }

    #[tokio::test]
    async fn test_journal_lifecycle() {
        let journal = TransactionJournal::new();
        let tx_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let node_id = NodeId::new();

        // Begin transaction
        journal.begin_transaction(tx_id, session_id, node_id, 0).await.unwrap();

        // Log statements
        journal.log_statement(
            tx_id,
            "SELECT * FROM users".to_string(),
            vec![],
            Some(12345),
            None,
            10,
        ).await.unwrap();

        journal.log_statement(
            tx_id,
            "INSERT INTO users (name) VALUES ($1)".to_string(),
            vec![JournalValue::Text("test".to_string())],
            None,
            Some(1),
            5,
        ).await.unwrap();

        // Check journal
        let j = journal.get_journal(&tx_id).await.unwrap();
        assert_eq!(j.entries.len(), 2);
        assert!(j.has_mutations);

        // Commit
        journal.commit_transaction(tx_id).await.unwrap();
        assert!(journal.get_journal(&tx_id).await.is_none());
    }

    #[tokio::test]
    async fn test_savepoints() {
        let journal = TransactionJournal::new();
        let tx_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let node_id = NodeId::new();

        journal.begin_transaction(tx_id, session_id, node_id, 0).await.unwrap();

        // Log some statements
        for i in 0..3 {
            journal.log_statement(
                tx_id,
                format!("INSERT INTO t VALUES ({})", i),
                vec![],
                None,
                Some(1),
                1,
            ).await.unwrap();
        }

        // Create savepoint
        journal.create_savepoint(tx_id, "sp1".to_string()).await.unwrap();

        // Log more
        for i in 3..5 {
            journal.log_statement(
                tx_id,
                format!("INSERT INTO t VALUES ({})", i),
                vec![],
                None,
                Some(1),
                1,
            ).await.unwrap();
        }

        let j = journal.get_journal(&tx_id).await.unwrap();
        assert_eq!(j.entries.len(), 5);

        // Rollback to savepoint
        journal.rollback_to_savepoint(tx_id, "sp1").await.unwrap();

        let j = journal.get_journal(&tx_id).await.unwrap();
        assert_eq!(j.entries.len(), 3);
    }

    #[tokio::test]
    async fn test_stats() {
        let journal = TransactionJournal::new();
        let tx_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let node_id = NodeId::new();

        journal.begin_transaction(tx_id, session_id, node_id, 0).await.unwrap();
        journal.log_statement(
            tx_id,
            "SELECT 1".to_string(),
            vec![],
            None,
            None,
            1,
        ).await.unwrap();

        let stats = journal.stats().await;
        assert_eq!(stats.active_transactions, 1);
        assert_eq!(stats.total_entries, 1);
        assert!(stats.enabled);
    }
}
