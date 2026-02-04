//! Custom Merge Strategy
//!
//! Allows users to define their own conflict resolution logic via closures.
//! Supports both selection (pick one winner) and merging (combine changes).

use super::MergeStrategy;
use crate::replication::multi_primary_sync::ChangeEntry;
use std::sync::Arc;

/// Custom resolve function type
pub type ResolveFn = Arc<dyn Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync>;

/// Custom merge function type (combines changes rather than picking one)
pub type MergeFn = Arc<dyn Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync>;

/// Custom strategy with user-defined resolution logic
pub struct CustomStrategy {
    /// Strategy name
    name: &'static str,
    /// Resolution function
    resolve_fn: ResolveFn,
    /// Optional merge function
    merge_fn: Option<MergeFn>,
}

impl CustomStrategy {
    /// Create a new custom strategy with a resolve function
    pub fn new<F>(name: &'static str, resolve_fn: F) -> Self
    where
        F: Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync + 'static,
    {
        Self {
            name,
            resolve_fn: Arc::new(resolve_fn),
            merge_fn: None,
        }
    }

    /// Create a custom strategy with both resolve and merge functions
    pub fn with_merge<R, M>(name: &'static str, resolve_fn: R, merge_fn: M) -> Self
    where
        R: Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync + 'static,
        M: Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync + 'static,
    {
        Self {
            name,
            resolve_fn: Arc::new(resolve_fn),
            merge_fn: Some(Arc::new(merge_fn)),
        }
    }
}

impl MergeStrategy for CustomStrategy {
    fn name(&self) -> &'static str {
        self.name
    }

    fn resolve(&self, changes: &[ChangeEntry]) -> Option<ChangeEntry> {
        (self.resolve_fn)(changes)
    }

    fn supports_merge(&self) -> bool {
        self.merge_fn.is_some()
    }

    fn merge(&self, changes: &[ChangeEntry]) -> Option<ChangeEntry> {
        self.merge_fn.as_ref().and_then(|f| f(changes))
    }
}

/// Builder for creating custom strategies with common patterns
pub struct CustomStrategyBuilder {
    name: &'static str,
    resolve_fn: Option<ResolveFn>,
    merge_fn: Option<MergeFn>,
}

impl CustomStrategyBuilder {
    /// Start building a custom strategy
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            resolve_fn: None,
            merge_fn: None,
        }
    }

    /// Set the resolve function
    pub fn resolve<F>(mut self, f: F) -> Self
    where
        F: Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync + 'static,
    {
        self.resolve_fn = Some(Arc::new(f));
        self
    }

    /// Set the merge function
    pub fn merge<F>(mut self, f: F) -> Self
    where
        F: Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync + 'static,
    {
        self.merge_fn = Some(Arc::new(f));
        self
    }

    /// Build the custom strategy
    pub fn build(self) -> CustomStrategy {
        let resolve_fn = self.resolve_fn.unwrap_or_else(|| {
            Arc::new(|changes: &[ChangeEntry]| changes.first().cloned())
        });

        CustomStrategy {
            name: self.name,
            resolve_fn,
            merge_fn: self.merge_fn,
        }
    }
}

/// Predefined custom strategies for common use cases
pub mod presets {
    use super::*;
    use crate::replication::multi_primary_sync::ChangeType;

    /// Strategy that picks the change with the largest data payload
    pub fn largest_data() -> CustomStrategy {
        CustomStrategy::new("largest-data", |changes| {
            changes.iter().max_by_key(|c| c.data.len()).cloned()
        })
    }

    /// Strategy that picks the change with the smallest data payload
    pub fn smallest_data() -> CustomStrategy {
        CustomStrategy::new("smallest-data", |changes| {
            changes.iter().min_by_key(|c| c.data.len()).cloned()
        })
    }

    /// Strategy that prefers deletes over other operations
    pub fn prefer_deletes() -> CustomStrategy {
        CustomStrategy::new("prefer-deletes", |changes| {
            // If any change is a delete, pick the first delete
            changes
                .iter()
                .find(|c| c.change_type == ChangeType::Delete)
                .cloned()
                .or_else(|| changes.iter().max_by_key(|c| c.timestamp).cloned())
        })
    }

    /// Strategy that prefers inserts/updates over deletes
    pub fn prefer_updates() -> CustomStrategy {
        CustomStrategy::new("prefer-updates", |changes| {
            // Pick first non-delete, or latest if all deletes
            changes
                .iter()
                .find(|c| c.change_type != ChangeType::Delete)
                .cloned()
                .or_else(|| changes.iter().max_by_key(|c| c.timestamp).cloned())
        })
    }

    /// Strategy based on node priority (requires vector clock entries)
    pub fn node_priority(priority_order: Vec<uuid::Uuid>) -> CustomStrategy {
        CustomStrategy::new("node-priority", move |changes| {
            for priority_node in &priority_order {
                for change in changes {
                    if change.vector_clock.contains_key(priority_node) {
                        return Some(change.clone());
                    }
                }
            }
            // Fall back to LWW if no priority node found
            changes.iter().max_by_key(|c| c.timestamp).cloned()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::multi_primary_sync::ChangeType;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn make_change(data: Vec<u8>, change_type: ChangeType) -> ChangeEntry {
        ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "test".to_string(),
            row_id: vec![1],
            change_type,
            data,
            vector_clock: HashMap::new(),
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_custom_strategy() {
        let strategy = CustomStrategy::new("test", |changes| {
            changes.iter().max_by_key(|c| c.data.len()).cloned()
        });

        let small = make_change(vec![1], ChangeType::Update);
        let large = make_change(vec![1, 2, 3, 4, 5], ChangeType::Update);

        let winner = strategy.resolve(&[small, large.clone()]).unwrap();
        assert_eq!(winner.data.len(), 5);
    }

    #[test]
    fn test_strategy_with_merge() {
        let strategy = CustomStrategy::with_merge(
            "sum-merge",
            |changes| changes.first().cloned(),
            |changes| {
                // Merge by summing all data bytes
                let sum: u8 = changes.iter().flat_map(|c| &c.data).sum();
                let mut merged = changes.first()?.clone();
                merged.data = vec![sum];
                Some(merged)
            },
        );

        assert!(strategy.supports_merge());

        let a = make_change(vec![10], ChangeType::Update);
        let b = make_change(vec![20], ChangeType::Update);

        let merged = strategy.merge(&[a, b]).unwrap();
        assert_eq!(merged.data, vec![30]);
    }

    #[test]
    fn test_builder() {
        let strategy = CustomStrategyBuilder::new("builder-test")
            .resolve(|changes| changes.last().cloned())
            .build();

        let a = make_change(vec![1], ChangeType::Update);
        let b = make_change(vec![2], ChangeType::Update);

        let winner = strategy.resolve(&[a, b.clone()]).unwrap();
        assert_eq!(winner.data, vec![2]); // Last one
    }

    #[test]
    fn test_preset_largest_data() {
        let strategy = presets::largest_data();

        let small = make_change(vec![1], ChangeType::Update);
        let large = make_change(vec![1, 2, 3, 4, 5], ChangeType::Update);

        let winner = strategy.resolve(&[small, large]).unwrap();
        assert_eq!(winner.data.len(), 5);
    }

    #[test]
    fn test_preset_prefer_deletes() {
        let strategy = presets::prefer_deletes();

        let update = make_change(vec![1], ChangeType::Update);
        let delete = make_change(vec![], ChangeType::Delete);

        let winner = strategy.resolve(&[update, delete]).unwrap();
        assert_eq!(winner.change_type, ChangeType::Delete);
    }

    #[test]
    fn test_preset_prefer_updates() {
        let strategy = presets::prefer_updates();

        let update = make_change(vec![1], ChangeType::Update);
        let delete = make_change(vec![], ChangeType::Delete);

        let winner = strategy.resolve(&[delete, update]).unwrap();
        assert_eq!(winner.change_type, ChangeType::Update);
    }
}
