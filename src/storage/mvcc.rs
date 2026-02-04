//! Standard MVCC (Multi-Version Concurrency Control) implementation
//!
//! Uses textbook snapshot isolation algorithm (no custom optimizations).

/// Snapshot ID (timestamp)
pub type SnapshotId = u64;

/// MVCC Snapshot
///
/// Represents a consistent view of the database at a point in time.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Snapshot timestamp
    pub id: SnapshotId,
}

impl Snapshot {
    /// Create a new snapshot
    pub fn new(id: SnapshotId) -> Self {
        Self { id }
    }

    /// Check if a version is visible in this snapshot
    ///
    /// Standard MVCC visibility rule: version is visible if its timestamp
    /// is less than or equal to the snapshot timestamp.
    pub fn is_visible(&self, version_timestamp: u64) -> bool {
        version_timestamp <= self.id
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_visibility() {
        let snapshot = Snapshot::new(100);

        // Versions before snapshot are visible
        assert!(snapshot.is_visible(50));
        assert!(snapshot.is_visible(100));

        // Versions after snapshot are not visible
        assert!(!snapshot.is_visible(101));
        assert!(!snapshot.is_visible(200));
    }
}
