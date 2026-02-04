//! Merge Strategies - Tier 2 Multi-Primary
//!
//! Pluggable conflict resolution strategies for multi-primary replication.
//!
//! # Available Strategies
//!
//! - **Last-Writer-Wins (LWW)**: Most recent timestamp wins
//! - **First-Writer-Wins (FWW)**: Earliest timestamp wins
//! - **Custom**: User-defined merge functions

pub mod custom;
pub mod fww;
pub mod lww;

use crate::replication::multi_primary_sync::ChangeEntry;
use std::sync::Arc;

/// Trait for merge strategy implementations
pub trait MergeStrategy: Send + Sync {
    /// Strategy name for logging/debugging
    fn name(&self) -> &'static str;

    /// Resolve a conflict between multiple changes
    ///
    /// Returns the winning change, or None if conflict cannot be resolved.
    fn resolve(&self, changes: &[ChangeEntry]) -> Option<ChangeEntry>;

    /// Whether this strategy supports merging (combining changes)
    fn supports_merge(&self) -> bool {
        false
    }

    /// Merge multiple changes into a single combined change
    ///
    /// Only called if `supports_merge()` returns true.
    fn merge(&self, _changes: &[ChangeEntry]) -> Option<ChangeEntry> {
        None
    }
}

/// Type alias for boxed merge strategy
pub type BoxedMergeStrategy = Arc<dyn MergeStrategy>;

/// Create a Last-Writer-Wins strategy
pub fn last_writer_wins() -> BoxedMergeStrategy {
    Arc::new(lww::LastWriterWins)
}

/// Create a First-Writer-Wins strategy
pub fn first_writer_wins() -> BoxedMergeStrategy {
    Arc::new(fww::FirstWriterWins)
}

/// Create a custom merge strategy with a closure
pub fn custom_strategy<F>(name: &'static str, func: F) -> BoxedMergeStrategy
where
    F: Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync + 'static,
{
    Arc::new(custom::CustomStrategy::new(name, func))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::multi_primary_sync::ChangeType;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn make_change(timestamp_offset_secs: i64, data: Vec<u8>) -> ChangeEntry {
        ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "test".to_string(),
            row_id: vec![1],
            change_type: ChangeType::Update,
            data,
            vector_clock: HashMap::new(),
            timestamp: chrono::Utc::now() + chrono::Duration::seconds(timestamp_offset_secs),
        }
    }

    #[test]
    fn test_lww_strategy() {
        let strategy = last_writer_wins();
        assert_eq!(strategy.name(), "last-writer-wins");

        let earlier = make_change(-10, vec![1]);
        let later = make_change(0, vec![2]);

        let winner = strategy.resolve(&[earlier, later.clone()]).unwrap();
        assert_eq!(winner.data, vec![2]);
    }

    #[test]
    fn test_fww_strategy() {
        let strategy = first_writer_wins();
        assert_eq!(strategy.name(), "first-writer-wins");

        let earlier = make_change(-10, vec![1]);
        let later = make_change(0, vec![2]);

        let winner = strategy.resolve(&[earlier.clone(), later]).unwrap();
        assert_eq!(winner.data, vec![1]);
    }

    #[test]
    fn test_custom_strategy() {
        // Custom strategy: pick the one with largest data
        let strategy = custom_strategy("largest-data", |changes| {
            changes.iter().max_by_key(|c| c.data.len()).cloned()
        });

        assert_eq!(strategy.name(), "largest-data");

        let small = make_change(0, vec![1]);
        let large = make_change(-10, vec![1, 2, 3, 4, 5]);

        let winner = strategy.resolve(&[small, large.clone()]).unwrap();
        assert_eq!(winner.data.len(), 5);
    }
}
