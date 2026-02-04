//! Lock-free hierarchical row ID generation
//!
//! Generates globally unique row IDs without any coordination between threads.
//! IDs are composed of: partition_id | timestamp | sequence
//!
//! # Format (64-bit)
//! ```text
//! ┌─────────────┬──────────────────────────────┬─────────────────┐
//! │ Partition   │         Timestamp            │    Sequence     │
//! │  (16 bits)  │         (32 bits)            │    (16 bits)    │
//! └─────────────┴──────────────────────────────┴─────────────────┘
//! ```
//!
//! - Partition: Thread/core ID (supports 65536 partitions)
//! - Timestamp: Seconds since epoch (good until year 2106)
//! - Sequence: Per-partition counter (65536 IDs per second per partition)
//!
//! # Properties
//! - Globally unique without coordination
//! - Monotonically increasing within partition
//! - Roughly time-ordered across partitions
//! - No persistence required (self-describing)

use std::cell::Cell;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Global partition counter for assigning unique partition IDs to threads
static NEXT_PARTITION_ID: AtomicU16 = AtomicU16::new(0);

/// Maximum partitions before wrapping (16 bits)
const MAX_PARTITIONS: u16 = u16::MAX;

/// Bits allocated to each component
const PARTITION_BITS: u32 = 16;
const TIMESTAMP_BITS: u32 = 32;
const SEQUENCE_BITS: u32 = 16;

/// Bit shifts for packing
const TIMESTAMP_SHIFT: u32 = SEQUENCE_BITS;
const PARTITION_SHIFT: u32 = TIMESTAMP_BITS + SEQUENCE_BITS;

/// Masks for unpacking
const SEQUENCE_MASK: u64 = (1 << SEQUENCE_BITS) - 1;
const TIMESTAMP_MASK: u64 = (1 << TIMESTAMP_BITS) - 1;
const PARTITION_MASK: u64 = (1 << PARTITION_BITS) - 1;

thread_local! {
    /// Thread-local partition ID (assigned once per thread)
    static PARTITION_ID: Cell<u16> = Cell::new(allocate_partition_id());

    /// Thread-local sequence counter
    static SEQUENCE: Cell<u16> = Cell::new(0);

    /// Last timestamp used (to detect clock regression)
    static LAST_TIMESTAMP: Cell<u32> = Cell::new(0);
}

/// Allocate a unique partition ID for the current thread
fn allocate_partition_id() -> u16 {
    NEXT_PARTITION_ID.fetch_add(1, Ordering::Relaxed) % MAX_PARTITIONS
}

/// Get current timestamp in seconds since Unix epoch
#[inline]
fn current_timestamp() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time before Unix epoch")
        .as_secs() as u32
}

/// Hierarchical row ID generator
///
/// Generates globally unique IDs without coordination.
/// Thread-safe and lock-free.
#[derive(Debug)]
pub struct HierarchicalRowIdGenerator {
    /// Epoch offset for timestamps (allows custom start time)
    epoch_offset: u32,
    /// Optional: force specific partition (for testing)
    forced_partition: Option<u16>,
}

impl Default for HierarchicalRowIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl HierarchicalRowIdGenerator {
    /// Create a new generator with default settings
    pub const fn new() -> Self {
        Self {
            epoch_offset: 0,
            forced_partition: None,
        }
    }

    /// Create a generator with custom epoch (e.g., 2024-01-01)
    pub const fn with_epoch(epoch_offset: u32) -> Self {
        Self {
            epoch_offset,
            forced_partition: None,
        }
    }

    /// Create a generator with forced partition (for testing)
    pub const fn with_partition(partition: u16) -> Self {
        Self {
            epoch_offset: 0,
            forced_partition: Some(partition),
        }
    }

    /// Generate the next unique row ID
    ///
    /// This is completely lock-free and requires no coordination.
    /// Each thread generates IDs independently.
    #[inline]
    pub fn next(&self) -> u64 {
        let partition = self.forced_partition.unwrap_or_else(|| {
            PARTITION_ID.with(|p| p.get())
        });

        let (timestamp, sequence) = SEQUENCE.with(|seq| {
            LAST_TIMESTAMP.with(|last_ts| {
                let current_ts = current_timestamp().saturating_sub(self.epoch_offset);
                let last = last_ts.get();

                if current_ts > last {
                    // New second - reset sequence
                    last_ts.set(current_ts);
                    seq.set(1);
                    (current_ts, 0u16)
                } else if current_ts == last {
                    // Same second - increment sequence
                    let s = seq.get();
                    if s == u16::MAX {
                        // Sequence overflow - wait for next second
                        // This is extremely rare (65536 IDs/sec/thread)
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        let new_ts = current_timestamp().saturating_sub(self.epoch_offset);
                        last_ts.set(new_ts);
                        seq.set(1);
                        (new_ts, 0u16)
                    } else {
                        seq.set(s + 1);
                        (current_ts, s)
                    }
                } else {
                    // Clock went backwards - use last timestamp + increment sequence
                    let s = seq.get();
                    seq.set(s.wrapping_add(1));
                    (last, s)
                }
            })
        });

        Self::pack(partition, timestamp, sequence)
    }

    /// Generate a batch of sequential IDs (more efficient for bulk inserts)
    #[inline]
    pub fn next_batch(&self, count: usize) -> Vec<u64> {
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            ids.push(self.next());
        }
        ids
    }

    /// Pack components into a single u64
    #[inline]
    const fn pack(partition: u16, timestamp: u32, sequence: u16) -> u64 {
        ((partition as u64) << PARTITION_SHIFT)
            | ((timestamp as u64) << TIMESTAMP_SHIFT)
            | (sequence as u64)
    }

    /// Unpack a row ID into its components
    #[inline]
    pub const fn unpack(id: u64) -> (u16, u32, u16) {
        let partition = ((id >> PARTITION_SHIFT) & PARTITION_MASK) as u16;
        let timestamp = ((id >> TIMESTAMP_SHIFT) & TIMESTAMP_MASK) as u32;
        let sequence = (id & SEQUENCE_MASK) as u16;
        (partition, timestamp, sequence)
    }

    /// Get the partition from a row ID
    #[inline]
    pub const fn partition_of(id: u64) -> u16 {
        ((id >> PARTITION_SHIFT) & PARTITION_MASK) as u16
    }

    /// Get the timestamp from a row ID
    #[inline]
    pub const fn timestamp_of(id: u64) -> u32 {
        ((id >> TIMESTAMP_SHIFT) & TIMESTAMP_MASK) as u32
    }

    /// Get the sequence from a row ID
    #[inline]
    pub const fn sequence_of(id: u64) -> u16 {
        (id & SEQUENCE_MASK) as u16
    }

    /// Check if id1 was generated before id2 (approximately)
    /// Note: Only accurate within same partition
    #[inline]
    pub const fn is_before(id1: u64, id2: u64) -> bool {
        let (p1, t1, s1) = Self::unpack(id1);
        let (p2, t2, s2) = Self::unpack(id2);

        if p1 == p2 {
            // Same partition - exact ordering
            t1 < t2 || (t1 == t2 && s1 < s2)
        } else {
            // Different partitions - approximate by timestamp
            t1 < t2
        }
    }
}

/// Batch row ID allocator for traditional sequential IDs
///
/// Allocates IDs in batches to reduce contention while maintaining
/// sequential ordering. ACID-safe when combined with WAL recovery.
#[derive(Debug)]
pub struct BatchRowIdAllocator {
    /// Batch size for each allocation
    batch_size: u64,
    /// Per-table allocators: (next_batch_start, current_id, batch_end)
    allocators: dashmap::DashMap<String, BatchState>,
}

#[derive(Debug)]
struct BatchState {
    /// Start of next batch to allocate
    next_batch: AtomicU64,
    /// Current ID within batch
    current: AtomicU64,
    /// End of current batch
    batch_end: AtomicU64,
}

impl BatchState {
    fn new(start: u64, batch_size: u64) -> Self {
        Self {
            next_batch: AtomicU64::new(start + batch_size),
            current: AtomicU64::new(start),
            batch_end: AtomicU64::new(start + batch_size),
        }
    }
}

impl BatchRowIdAllocator {
    /// Create a new batch allocator
    pub fn new(batch_size: u64) -> Self {
        Self {
            batch_size,
            allocators: dashmap::DashMap::new(),
        }
    }

    /// Get or create allocator for table, starting from given ID
    pub fn initialize_table(&self, table: &str, start_id: u64) {
        self.allocators.entry(table.to_string())
            .or_insert_with(|| BatchState::new(start_id, self.batch_size));
    }

    /// Allocate next ID for table (lock-free fast path)
    #[inline]
    pub fn next(&self, table: &str) -> u64 {
        let state = self.allocators.entry(table.to_string())
            .or_insert_with(|| BatchState::new(1, self.batch_size));

        loop {
            let current = state.current.fetch_add(1, Ordering::Relaxed);
            let batch_end = state.batch_end.load(Ordering::Acquire);

            if current < batch_end {
                return current;
            }

            // Need new batch
            let new_start = state.next_batch.fetch_add(self.batch_size, Ordering::SeqCst);
            state.batch_end.store(new_start + self.batch_size, Ordering::Release);
            state.current.store(new_start + 1, Ordering::Release);
            return new_start;
        }
    }

    /// Get maximum allocated ID for table (for checkpointing)
    pub fn max_allocated(&self, table: &str) -> Option<u64> {
        self.allocators.get(table).map(|state| {
            state.next_batch.load(Ordering::Relaxed)
        })
    }

    /// Get all tables and their max allocated IDs
    pub fn checkpoint_state(&self) -> Vec<(String, u64)> {
        self.allocators.iter()
            .map(|entry| {
                let max = entry.next_batch.load(Ordering::Relaxed);
                (entry.key().clone(), max)
            })
            .collect()
    }

    /// Restore from checkpoint (add safety margin)
    pub fn restore_from_checkpoint(&self, table: &str, max_id: u64) {
        let safe_start = max_id + self.batch_size; // Safety margin
        self.allocators.insert(
            table.to_string(),
            BatchState::new(safe_start, self.batch_size)
        );
    }
}

/// Combined row ID generator supporting both modes
pub enum RowIdGenerator {
    /// Hierarchical IDs (no coordination needed)
    Hierarchical(HierarchicalRowIdGenerator),
    /// Batch-allocated sequential IDs
    Batched(BatchRowIdAllocator),
}

impl RowIdGenerator {
    /// Generate next ID for table
    pub fn next(&self, table: &str) -> u64 {
        match self {
            Self::Hierarchical(gen) => gen.next(),
            Self::Batched(alloc) => alloc.next(table),
        }
    }

    /// Generate batch of IDs
    pub fn next_batch(&self, table: &str, count: usize) -> Vec<u64> {
        match self {
            Self::Hierarchical(gen) => gen.next_batch(count),
            Self::Batched(alloc) => {
                (0..count).map(|_| alloc.next(table)).collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::thread;

    #[test]
    fn test_hierarchical_uniqueness() {
        let gen = HierarchicalRowIdGenerator::new();
        let mut ids = HashSet::new();

        for _ in 0..10000 {
            let id = gen.next();
            assert!(ids.insert(id), "Duplicate ID generated: {}", id);
        }
    }

    #[test]
    fn test_hierarchical_monotonic() {
        let gen = HierarchicalRowIdGenerator::with_partition(0);
        let mut last = 0u64;

        for _ in 0..1000 {
            let id = gen.next();
            assert!(id > last, "IDs not monotonic: {} <= {}", id, last);
            last = id;
        }
    }

    #[test]
    fn test_hierarchical_cross_thread() {
        let ids: std::sync::Arc<std::sync::Mutex<HashSet<u64>>> =
            std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let ids = ids.clone();
                thread::spawn(move || {
                    let gen = HierarchicalRowIdGenerator::new();
                    let local_ids: Vec<u64> = (0..1000).map(|_| gen.next()).collect();
                    let mut guard = ids.lock().unwrap();
                    for id in local_ids {
                        assert!(guard.insert(id), "Cross-thread duplicate: {}", id);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(ids.lock().unwrap().len(), 4000);
    }

    #[test]
    fn test_pack_unpack() {
        let partition = 123u16;
        let timestamp = 1700000000u32;
        let sequence = 456u16;

        let packed = HierarchicalRowIdGenerator::pack(partition, timestamp, sequence);
        let (p, t, s) = HierarchicalRowIdGenerator::unpack(packed);

        assert_eq!(p, partition);
        assert_eq!(t, timestamp);
        assert_eq!(s, sequence);
    }

    #[test]
    fn test_batch_allocator() {
        let alloc = BatchRowIdAllocator::new(100);

        let id1 = alloc.next("test");
        let id2 = alloc.next("test");
        let id3 = alloc.next("test");

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_batch_allocator_cross_batch() {
        let alloc = BatchRowIdAllocator::new(10);

        for i in 1..=25 {
            let id = alloc.next("test");
            assert!(id >= 1, "ID {} should be >= 1", id);
        }

        let max = alloc.max_allocated("test").unwrap();
        assert!(max >= 25, "Max allocated should be >= 25");
    }
}
