//! First-Writer-Wins (FWW) Merge Strategy
//!
//! Resolves conflicts by selecting the change with the earliest timestamp.
//! Useful when the original write should take precedence over later modifications.

use super::MergeStrategy;
use crate::replication::multi_primary_sync::ChangeEntry;

/// First-Writer-Wins strategy
pub struct FirstWriterWins;

impl MergeStrategy for FirstWriterWins {
    fn name(&self) -> &'static str {
        "first-writer-wins"
    }

    fn resolve(&self, changes: &[ChangeEntry]) -> Option<ChangeEntry> {
        if changes.is_empty() {
            return None;
        }

        // Select the change with the earliest timestamp
        changes.iter().min_by_key(|c| c.timestamp).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::multi_primary_sync::ChangeType;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn test_fww_selects_earliest() {
        let strategy = FirstWriterWins;

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
        assert_eq!(winner.data, vec![1]); // First change has earliest timestamp
    }

    #[test]
    fn test_fww_empty() {
        let strategy = FirstWriterWins;
        assert!(strategy.resolve(&[]).is_none());
    }

    #[test]
    fn test_fww_single() {
        let strategy = FirstWriterWins;
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
