//! Last-Writer-Wins (LWW) Merge Strategy
//!
//! Resolves conflicts by selecting the change with the latest timestamp.
//! Simple and predictable, but may lose data from concurrent writes.

use super::MergeStrategy;
use crate::replication::multi_primary_sync::ChangeEntry;

/// Last-Writer-Wins strategy
pub struct LastWriterWins;

impl MergeStrategy for LastWriterWins {
    fn name(&self) -> &'static str {
        "last-writer-wins"
    }

    fn resolve(&self, changes: &[ChangeEntry]) -> Option<ChangeEntry> {
        if changes.is_empty() {
            return None;
        }

        // Select the change with the latest timestamp
        changes.iter().max_by_key(|c| c.timestamp).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::multi_primary_sync::ChangeType;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn test_lww_selects_latest() {
        let strategy = LastWriterWins;

        let changes = vec![
            ChangeEntry {
                change_id: Uuid::new_v4(),
                table: "users".to_string(),
                row_id: vec![1],
                change_type: ChangeType::Update,
                data: vec![1],
                vector_clock: HashMap::new(),
                timestamp: chrono::Utc::now() - chrono::Duration::hours(1),
            },
            ChangeEntry {
                change_id: Uuid::new_v4(),
                table: "users".to_string(),
                row_id: vec![1],
                change_type: ChangeType::Update,
                data: vec![2],
                vector_clock: HashMap::new(),
                timestamp: chrono::Utc::now(),
            },
            ChangeEntry {
                change_id: Uuid::new_v4(),
                table: "users".to_string(),
                row_id: vec![1],
                change_type: ChangeType::Update,
                data: vec![3],
                vector_clock: HashMap::new(),
                timestamp: chrono::Utc::now() - chrono::Duration::minutes(30),
            },
        ];

        let winner = strategy.resolve(&changes).unwrap();
        assert_eq!(winner.data, vec![2]); // Second change has latest timestamp
    }

    #[test]
    fn test_lww_empty() {
        let strategy = LastWriterWins;
        assert!(strategy.resolve(&[]).is_none());
    }

    #[test]
    fn test_lww_single() {
        let strategy = LastWriterWins;
        let change = ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![1],
            change_type: ChangeType::Insert,
            data: vec![42],
            vector_clock: HashMap::new(),
            timestamp: chrono::Utc::now(),
        };

        let winner = strategy.resolve(&[change.clone()]).unwrap();
        assert_eq!(winner.change_id, change.change_id);
    }
}
