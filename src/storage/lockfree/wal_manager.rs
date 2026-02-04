//! Partitioned WAL with Two-Phase Commit support
//!
//! Provides ACID-safe durability for lock-free ingestion with optional
//! partitioning for linear scalability.
//!
//! # Architecture
//!
//! ```text
//! Single-Partition Transaction:          Cross-Partition Transaction:
//!
//!  ┌─────────────┐                        ┌─────────────┐
//!  │ Transaction │                        │ Transaction │
//!  └──────┬──────┘                        └──────┬──────┘
//!         │                                      │
//!         ▼                                      ▼
//!  ┌─────────────┐                        ┌─────────────┐
//!  │  WAL Part 0 │                        │ Coordinator │
//!  └──────┬──────┘                        └──────┬──────┘
//!         │                                      │
//!         ▼                               ┌──────┴──────┐
//!  ┌─────────────┐                        │   PREPARE   │
//!  │   RocksDB   │                        ▼             ▼
//!  └─────────────┘                 ┌──────────┐  ┌──────────┐
//!                                  │WAL Part 0│  │WAL Part 1│
//!                                  └────┬─────┘  └────┬─────┘
//!                                       │   COMMIT    │
//!                                       ▼             ▼
//!                                  ┌──────────┐  ┌──────────┐
//!                                  │WAL Part 0│  │WAL Part 1│
//!                                  └────┬─────┘  └────┬─────┘
//!                                       │             │
//!                                       └──────┬──────┘
//!                                              ▼
//!                                       ┌─────────────┐
//!                                       │   RocksDB   │
//!                                       └─────────────┘
//! ```

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::config::IngestionSafetyLevel;
use super::write_buffer::{CommitRequest, WriteOp};

/// WAL record types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalRecord {
    /// Single transaction commit (atomic)
    Commit {
        txn_id: u64,
        timestamp: u64,
        operations: Vec<WalOp>,
    },

    /// 2PC Phase 1: Prepare
    Prepare {
        txn_id: u64,
        partition: u16,
        operations: Vec<WalOp>,
    },

    /// 2PC Phase 2: Commit prepared transaction
    CommitPrepared {
        txn_id: u64,
    },

    /// 2PC: Rollback prepared transaction
    RollbackPrepared {
        txn_id: u64,
    },

    /// Checkpoint marker (for recovery)
    Checkpoint {
        timestamp: u64,
        row_id_state: Vec<(String, u64)>,
    },
}

/// Individual WAL operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalOp {
    Insert {
        table: String,
        row_id: u64,
        data: Vec<u8>,
    },
    Update {
        table: String,
        row_id: u64,
        data: Vec<u8>,
    },
    Delete {
        table: String,
        row_id: u64,
    },
}

impl From<&WriteOp> for WalOp {
    fn from(op: &WriteOp) -> Self {
        match op {
            WriteOp::Insert { table, row_id, data } => WalOp::Insert {
                table: table.clone(),
                row_id: *row_id,
                data: data.clone(),
            },
            WriteOp::Update { table, row_id, data } => WalOp::Update {
                table: table.clone(),
                row_id: *row_id,
                data: data.clone(),
            },
            WriteOp::Delete { table, row_id } => WalOp::Delete {
                table: table.clone(),
                row_id: *row_id,
            },
        }
    }
}

/// Single WAL partition
pub struct WalPartition {
    /// Partition ID
    id: u16,
    /// File path
    path: PathBuf,
    /// Writer (buffered)
    writer: Mutex<BufWriter<File>>,
    /// Current LSN (Log Sequence Number)
    lsn: AtomicU64,
    /// Bytes written since last fsync
    unflushed_bytes: AtomicU64,
    /// Records since last fsync
    unflushed_records: AtomicU64,
}

impl WalPartition {
    /// Open or create a WAL partition
    pub fn open(path: impl AsRef<Path>, id: u16) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let lsn = Self::recover_lsn(&path)?;

        Ok(Self {
            id,
            path,
            writer: Mutex::new(BufWriter::with_capacity(64 * 1024, file)),
            lsn: AtomicU64::new(lsn),
            unflushed_bytes: AtomicU64::new(0),
            unflushed_records: AtomicU64::new(0),
        })
    }

    /// Recover LSN from existing file
    fn recover_lsn(path: &Path) -> std::io::Result<u64> {
        // For now, just return 0 if file is new
        // Full implementation would scan file for highest LSN
        if path.exists() {
            let metadata = std::fs::metadata(path)?;
            if metadata.len() > 0 {
                // Simple heuristic: use file size as approximate LSN
                return Ok(metadata.len());
            }
        }
        Ok(0)
    }

    /// Append a record to the WAL
    pub fn append(&self, record: &WalRecord) -> std::io::Result<u64> {
        let serialized = bincode::serialize(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let record_len = serialized.len() as u32;
        let mut writer = self.writer.lock();

        // Write length prefix + record
        writer.write_all(&record_len.to_le_bytes())?;
        writer.write_all(&serialized)?;

        let lsn = self.lsn.fetch_add(1, Ordering::SeqCst) + 1;
        self.unflushed_bytes.fetch_add(serialized.len() as u64 + 4, Ordering::Relaxed);
        self.unflushed_records.fetch_add(1, Ordering::Relaxed);

        Ok(lsn)
    }

    /// Sync (fsync) the WAL to disk
    pub fn sync(&self) -> std::io::Result<()> {
        let mut writer = self.writer.lock();
        writer.flush()?;
        writer.get_ref().sync_all()?;
        self.unflushed_bytes.store(0, Ordering::Relaxed);
        self.unflushed_records.store(0, Ordering::Relaxed);
        Ok(())
    }

    /// Get unflushed stats
    pub fn unflushed_stats(&self) -> (u64, u64) {
        (
            self.unflushed_bytes.load(Ordering::Relaxed),
            self.unflushed_records.load(Ordering::Relaxed),
        )
    }

    /// Get current LSN
    pub fn lsn(&self) -> u64 {
        self.lsn.load(Ordering::Acquire)
    }
}

/// Partitioned WAL manager
pub struct PartitionedWalManager {
    /// WAL partitions
    partitions: Vec<Arc<WalPartition>>,
    /// Base directory
    base_path: PathBuf,
    /// Safety level
    safety_level: IngestionSafetyLevel,
    /// Global timestamp for MVCC
    global_timestamp: AtomicU64,
    /// Pending 2PC transactions: txn_id -> (partitions involved, prepared count)
    pending_2pc: RwLock<HashMap<u64, TwoPcState>>,
    /// Enabled flag
    enabled: AtomicBool,
}

/// State for 2PC transaction
#[derive(Debug)]
struct TwoPcState {
    /// Partitions involved
    partitions: HashSet<u16>,
    /// Partitions that have prepared
    prepared: HashSet<u16>,
    /// Operations per partition
    operations: HashMap<u16, Vec<WalOp>>,
}

impl PartitionedWalManager {
    /// Create a new partitioned WAL manager
    pub fn new(
        base_path: impl AsRef<Path>,
        partition_count: usize,
        safety_level: IngestionSafetyLevel,
    ) -> std::io::Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        std::fs::create_dir_all(&base_path)?;

        let enabled = safety_level.use_wal();
        let mut partitions = Vec::with_capacity(partition_count);

        if enabled {
            for i in 0..partition_count {
                let path = base_path.join(format!("wal_{:04}.log", i));
                let partition = WalPartition::open(&path, i as u16)?;
                partitions.push(Arc::new(partition));
            }
        }

        Ok(Self {
            partitions,
            base_path,
            safety_level,
            global_timestamp: AtomicU64::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            ),
            pending_2pc: RwLock::new(HashMap::new()),
            enabled: AtomicBool::new(enabled),
        })
    }

    /// Check if WAL is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Get partition for a table/row combination
    fn partition_for(&self, table: &str, row_id: u64) -> u16 {
        if self.partitions.is_empty() {
            return 0;
        }
        // Simple hash combining table name and row_id
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        table.hash(&mut hasher);
        row_id.hash(&mut hasher);
        let hash = hasher.finish();
        (hash % self.partitions.len() as u64) as u16
    }

    /// Get next global timestamp
    pub fn next_timestamp(&self) -> u64 {
        self.global_timestamp.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Commit a single-partition transaction (fast path)
    pub fn commit_single(
        &self,
        txn_id: u64,
        operations: &[WriteOp],
        sync: bool,
    ) -> std::io::Result<u64> {
        if !self.is_enabled() || self.partitions.is_empty() {
            return Ok(self.next_timestamp());
        }

        // Determine partition (all ops should be same partition for fast path)
        let partition_id = if let Some(first) = operations.first() {
            self.partition_for(first.table(), first.row_id())
        } else {
            return Ok(self.next_timestamp());
        };

        let timestamp = self.next_timestamp();
        let wal_ops: Vec<WalOp> = operations.iter().map(|op| op.into()).collect();

        let record = WalRecord::Commit {
            txn_id,
            timestamp,
            operations: wal_ops,
        };

        let partition = &self.partitions[partition_id as usize];
        partition.append(&record)?;

        if sync {
            partition.sync()?;
        }

        Ok(timestamp)
    }

    /// Write a commit record with pre-converted WalOps
    pub fn write_commit(
        &self,
        txn_id: u64,
        timestamp: u64,
        operations: Vec<WalOp>,
    ) -> std::io::Result<()> {
        if !self.is_enabled() || self.partitions.is_empty() {
            return Ok(());
        }

        // Determine partition from first operation
        let partition_id = if let Some(first) = operations.first() {
            let (table, row_id) = match first {
                WalOp::Insert { table, row_id, .. } => (table.as_str(), *row_id),
                WalOp::Update { table, row_id, .. } => (table.as_str(), *row_id),
                WalOp::Delete { table, row_id } => (table.as_str(), *row_id),
            };
            self.partition_for(table, row_id)
        } else {
            return Ok(());
        };

        let record = WalRecord::Commit {
            txn_id,
            timestamp,
            operations,
        };

        let partition = &self.partitions[partition_id as usize];
        partition.append(&record)?;

        Ok(())
    }

    /// Start 2PC for cross-partition transaction
    pub fn prepare_2pc(
        &self,
        txn_id: u64,
        operations: &[WriteOp],
    ) -> std::io::Result<()> {
        if !self.is_enabled() || self.partitions.is_empty() {
            return Ok(());
        }

        // Group operations by partition
        let mut partition_ops: HashMap<u16, Vec<WalOp>> = HashMap::new();
        for op in operations {
            let partition_id = self.partition_for(op.table(), op.row_id());
            partition_ops
                .entry(partition_id)
                .or_insert_with(Vec::new)
                .push(op.into());
        }

        // Record 2PC state
        let partitions: HashSet<u16> = partition_ops.keys().copied().collect();
        let state = TwoPcState {
            partitions: partitions.clone(),
            prepared: HashSet::new(),
            operations: partition_ops.clone(),
        };
        self.pending_2pc.write().insert(txn_id, state);

        // Phase 1: Prepare on all partitions
        for (partition_id, ops) in partition_ops {
            let record = WalRecord::Prepare {
                txn_id,
                partition: partition_id,
                operations: ops,
            };
            self.partitions[partition_id as usize].append(&record)?;
            self.partitions[partition_id as usize].sync()?; // Must sync prepare

            // Update prepared set
            if let Some(state) = self.pending_2pc.write().get_mut(&txn_id) {
                state.prepared.insert(partition_id);
            }
        }

        Ok(())
    }

    /// Commit a prepared 2PC transaction
    pub fn commit_2pc(&self, txn_id: u64) -> std::io::Result<u64> {
        let timestamp = self.next_timestamp();

        if !self.is_enabled() || self.partitions.is_empty() {
            self.pending_2pc.write().remove(&txn_id);
            return Ok(timestamp);
        }

        // Get partitions involved
        let partitions = {
            let pending = self.pending_2pc.read();
            pending.get(&txn_id).map(|s| s.partitions.clone())
        };

        if let Some(partitions) = partitions {
            // Phase 2: Commit on all partitions
            let record = WalRecord::CommitPrepared { txn_id };
            for partition_id in partitions {
                self.partitions[partition_id as usize].append(&record)?;
            }

            // Sync all partitions
            for partition in &self.partitions {
                partition.sync()?;
            }
        }

        self.pending_2pc.write().remove(&txn_id);
        Ok(timestamp)
    }

    /// Rollback a 2PC transaction
    pub fn rollback_2pc(&self, txn_id: u64) -> std::io::Result<()> {
        if !self.is_enabled() || self.partitions.is_empty() {
            self.pending_2pc.write().remove(&txn_id);
            return Ok(());
        }

        let partitions = {
            let pending = self.pending_2pc.read();
            pending.get(&txn_id).map(|s| s.prepared.clone())
        };

        if let Some(partitions) = partitions {
            let record = WalRecord::RollbackPrepared { txn_id };
            for partition_id in partitions {
                self.partitions[partition_id as usize].append(&record)?;
            }
        }

        self.pending_2pc.write().remove(&txn_id);
        Ok(())
    }

    /// Commit batch of transactions (for batched mode)
    pub fn commit_batch(
        &self,
        requests: &[CommitRequest],
        sync: bool,
    ) -> std::io::Result<u64> {
        if !self.is_enabled() || self.partitions.is_empty() {
            return Ok(self.next_timestamp());
        }

        let timestamp = self.next_timestamp();

        // Group all operations by partition
        let mut partition_records: HashMap<u16, Vec<WalRecord>> = HashMap::new();

        for request in requests {
            // Check if single partition (fast path) or multi-partition (2PC)
            let mut partitions_used = HashSet::new();
            for op in &request.operations {
                let part = self.partition_for(op.table(), op.row_id());
                partitions_used.insert(part);
            }

            if partitions_used.len() == 1 {
                // Single partition - direct commit
                let partition_id = *partitions_used.iter().next().unwrap();
                let wal_ops: Vec<WalOp> = request.operations.iter().map(|op| op.into()).collect();
                let record = WalRecord::Commit {
                    txn_id: request.txn_id,
                    timestamp,
                    operations: wal_ops,
                };
                partition_records
                    .entry(partition_id)
                    .or_insert_with(Vec::new)
                    .push(record);
            } else {
                // Multi-partition - need 2PC
                // For batched mode, we group prepare+commit
                let wal_ops: Vec<WalOp> = request.operations.iter().map(|op| op.into()).collect();
                for &partition_id in &partitions_used {
                    // Filter ops for this partition
                    let part_ops: Vec<WalOp> = request
                        .operations
                        .iter()
                        .filter(|op| self.partition_for(op.table(), op.row_id()) == partition_id)
                        .map(|op| op.into())
                        .collect();

                    let prepare = WalRecord::Prepare {
                        txn_id: request.txn_id,
                        partition: partition_id,
                        operations: part_ops,
                    };
                    partition_records
                        .entry(partition_id)
                        .or_insert_with(Vec::new)
                        .push(prepare);
                }

                // Add commit record to all partitions
                for &partition_id in &partitions_used {
                    let commit = WalRecord::CommitPrepared { txn_id: request.txn_id };
                    partition_records
                        .entry(partition_id)
                        .or_insert_with(Vec::new)
                        .push(commit);
                }
            }
        }

        // Write records to each partition
        for (partition_id, records) in partition_records {
            let partition = &self.partitions[partition_id as usize];
            for record in records {
                partition.append(&record)?;
            }
        }

        // Sync if required
        if sync {
            for partition in &self.partitions {
                partition.sync()?;
            }
        }

        Ok(timestamp)
    }

    /// Write checkpoint
    pub fn checkpoint(&self, row_id_state: Vec<(String, u64)>) -> std::io::Result<()> {
        if !self.is_enabled() || self.partitions.is_empty() {
            return Ok(());
        }

        let record = WalRecord::Checkpoint {
            timestamp: self.global_timestamp.load(Ordering::Acquire),
            row_id_state,
        };

        // Write checkpoint to all partitions
        for partition in &self.partitions {
            partition.append(&record)?;
            partition.sync()?;
        }

        Ok(())
    }

    /// Sync all partitions
    pub fn sync_all(&self) -> std::io::Result<()> {
        for partition in &self.partitions {
            partition.sync()?;
        }
        Ok(())
    }

    /// Get total unflushed bytes across all partitions
    pub fn unflushed_bytes(&self) -> u64 {
        self.partitions
            .iter()
            .map(|p| p.unflushed_stats().0)
            .sum()
    }
}

/// WAL recovery for crash recovery
pub struct WalRecovery {
    base_path: PathBuf,
}

impl WalRecovery {
    pub fn new(base_path: impl AsRef<Path>) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
        }
    }

    /// Recover row ID state from WAL
    pub fn recover_row_ids(&self) -> std::io::Result<HashMap<String, u64>> {
        let mut row_ids: HashMap<String, u64> = HashMap::new();

        // Find all WAL files
        let entries = std::fs::read_dir(&self.base_path)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "log").unwrap_or(false) {
                self.scan_wal_file(&path, &mut row_ids)?;
            }
        }

        Ok(row_ids)
    }

    fn scan_wal_file(
        &self,
        path: &Path,
        row_ids: &mut HashMap<String, u64>,
    ) -> std::io::Result<()> {
        use std::io::Read;

        let mut file = File::open(path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        let mut pos = 0;
        while pos + 4 <= buffer.len() {
            // Read length prefix
            let len = u32::from_le_bytes([
                buffer[pos],
                buffer[pos + 1],
                buffer[pos + 2],
                buffer[pos + 3],
            ]) as usize;
            pos += 4;

            if pos + len > buffer.len() {
                break; // Incomplete record
            }

            // Deserialize record
            if let Ok(record) = bincode::deserialize::<WalRecord>(&buffer[pos..pos + len]) {
                self.process_record_for_recovery(record, row_ids);
            }

            pos += len;
        }

        Ok(())
    }

    fn process_record_for_recovery(
        &self,
        record: WalRecord,
        row_ids: &mut HashMap<String, u64>,
    ) {
        match record {
            WalRecord::Commit { operations, .. } => {
                for op in operations {
                    self.update_row_id_from_op(&op, row_ids);
                }
            }
            WalRecord::Prepare { operations, .. } => {
                for op in operations {
                    self.update_row_id_from_op(&op, row_ids);
                }
            }
            WalRecord::Checkpoint { row_id_state, .. } => {
                // Checkpoint supersedes previous state
                for (table, max_id) in row_id_state {
                    let entry = row_ids.entry(table).or_insert(0);
                    *entry = (*entry).max(max_id);
                }
            }
            _ => {}
        }
    }

    fn update_row_id_from_op(&self, op: &WalOp, row_ids: &mut HashMap<String, u64>) {
        let (table, row_id) = match op {
            WalOp::Insert { table, row_id, .. } => (table, *row_id),
            WalOp::Update { table, row_id, .. } => (table, *row_id),
            WalOp::Delete { table, row_id } => (table, *row_id),
        };

        let entry = row_ids.entry(table.clone()).or_insert(0);
        *entry = (*entry).max(row_id);
    }

    /// Recover incomplete 2PC transactions
    pub fn recover_2pc(&self) -> std::io::Result<(Vec<u64>, Vec<u64>)> {
        let mut prepared: HashMap<u64, HashSet<u16>> = HashMap::new();
        let mut committed: HashSet<u64> = HashSet::new();
        let mut rolled_back: HashSet<u64> = HashSet::new();

        // Scan all WAL files
        let entries = std::fs::read_dir(&self.base_path)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "log").unwrap_or(false) {
                self.scan_2pc_state(&path, &mut prepared, &mut committed, &mut rolled_back)?;
            }
        }

        // Find incomplete transactions (prepared but not committed/rolled back)
        let to_rollback: Vec<u64> = prepared
            .keys()
            .filter(|txn_id| !committed.contains(txn_id) && !rolled_back.contains(txn_id))
            .copied()
            .collect();

        let to_complete: Vec<u64> = prepared
            .keys()
            .filter(|txn_id| committed.contains(txn_id))
            .copied()
            .collect();

        Ok((to_complete, to_rollback))
    }

    fn scan_2pc_state(
        &self,
        path: &Path,
        prepared: &mut HashMap<u64, HashSet<u16>>,
        committed: &mut HashSet<u64>,
        rolled_back: &mut HashSet<u64>,
    ) -> std::io::Result<()> {
        use std::io::Read;

        let mut file = File::open(path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        let mut pos = 0;
        while pos + 4 <= buffer.len() {
            let len = u32::from_le_bytes([
                buffer[pos],
                buffer[pos + 1],
                buffer[pos + 2],
                buffer[pos + 3],
            ]) as usize;
            pos += 4;

            if pos + len > buffer.len() {
                break;
            }

            if let Ok(record) = bincode::deserialize::<WalRecord>(&buffer[pos..pos + len]) {
                match record {
                    WalRecord::Prepare { txn_id, partition, .. } => {
                        prepared
                            .entry(txn_id)
                            .or_insert_with(HashSet::new)
                            .insert(partition);
                    }
                    WalRecord::CommitPrepared { txn_id } => {
                        committed.insert(txn_id);
                    }
                    WalRecord::RollbackPrepared { txn_id } => {
                        rolled_back.insert(txn_id);
                    }
                    _ => {}
                }
            }

            pos += len;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_single_partition_commit() {
        let dir = tempdir().unwrap();
        let manager = PartitionedWalManager::new(
            dir.path(),
            1,
            IngestionSafetyLevel::Full,
        ).unwrap();

        let ops = vec![
            WriteOp::Insert {
                table: "test".to_string(),
                row_id: 1,
                data: vec![1, 2, 3],
            },
        ];

        let ts = manager.commit_single(1, &ops, true).unwrap();
        assert!(ts > 0);
    }

    #[test]
    fn test_disabled_wal() {
        let dir = tempdir().unwrap();
        let manager = PartitionedWalManager::new(
            dir.path(),
            1,
            IngestionSafetyLevel::Unsafe {
                disable_wal: true,
                checkpoint_interval_secs: 0,
            },
        ).unwrap();

        assert!(!manager.is_enabled());
    }
}
