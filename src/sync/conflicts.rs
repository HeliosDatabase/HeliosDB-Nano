//! Conflict detection and resolution

use super::{RowId, VectorClock};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Conflict representation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub id: Uuid,
    pub table: String,
    pub row_id: RowId,
    pub conflict_type: ConflictType,
    pub client_version: Vec<u8>,
    pub server_version: Vec<u8>,
    pub resolution: ConflictResolution,
}

/// Conflict types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConflictType {
    ConcurrentUpdate, // Both sides modified same row
    DeleteUpdate,     // One deleted, other updated
    UniqueViolation,  // Primary key or unique constraint
}

/// Conflict resolution strategies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConflictResolution {
    UseClient,  // Client wins
    UseServer,  // Server wins
    Merge,      // Automatic merge
    Manual,     // Requires manual intervention
}

/// Conflict manager
pub struct ConflictManager {
    resolution_strategy: ConflictResolution,
}

impl ConflictManager {
    pub fn new(strategy: ConflictResolution) -> Self {
        Self {
            resolution_strategy: strategy,
        }
    }

    /// Get the current resolution strategy
    pub fn strategy(&self) -> &ConflictResolution {
        &self.resolution_strategy
    }

    /// Detect conflict between two vector clocks
    pub fn detect_conflict(
        &self,
        client_clock: &VectorClock,
        server_clock: &VectorClock,
    ) -> bool {
        client_clock.conflicts_with(server_clock)
    }

    /// Resolve conflict automatically
    pub fn resolve(&self, conflict: &Conflict) -> Result<Vec<u8>, String> {
        match conflict.resolution {
            ConflictResolution::UseClient => Ok(conflict.client_version.clone()),
            ConflictResolution::UseServer => Ok(conflict.server_version.clone()),
            ConflictResolution::Merge => self.auto_merge(conflict),
            ConflictResolution::Manual => Err("Manual resolution required".to_string()),
        }
    }

    /// Automatic merge with field-level conflict resolution
    ///
    /// This implements a field-level merge strategy for concurrent updates:
    /// - Deserializes both versions as JSON objects
    /// - Merges fields: non-conflicting fields are preserved from both sides
    /// - For conflicting fields (same key, different values), uses server version
    /// - Returns the merged result as serialized bytes
    fn auto_merge(&self, conflict: &Conflict) -> Result<Vec<u8>, String> {
        match conflict.conflict_type {
            ConflictType::ConcurrentUpdate => {
                // Try field-level merge for JSON data
                self.field_level_merge(&conflict.client_version, &conflict.server_version)
            }
            ConflictType::DeleteUpdate => {
                // If one side deleted, respect the delete
                Ok(vec![]) // Empty = deleted
            }
            ConflictType::UniqueViolation => Err("Cannot auto-merge unique violations".to_string()),
        }
    }

    /// Perform field-level merge of two data versions
    ///
    /// Attempts to deserialize both versions as JSON and merge fields.
    /// Falls back to server version if JSON parsing fails.
    fn field_level_merge(&self, client_data: &[u8], server_data: &[u8]) -> Result<Vec<u8>, String> {
        // If either is empty, return the non-empty one
        if client_data.is_empty() {
            return Ok(server_data.to_vec());
        }
        if server_data.is_empty() {
            return Ok(client_data.to_vec());
        }

        // Try to parse both as JSON for field-level merge
        let client_json: Result<serde_json::Value, _> = serde_json::from_slice(client_data);
        let server_json: Result<serde_json::Value, _> = serde_json::from_slice(server_data);

        match (client_json, server_json) {
            (Ok(serde_json::Value::Object(mut client_obj)), Ok(serde_json::Value::Object(server_obj))) => {
                // Field-level merge: combine both objects
                // Server values take precedence for conflicting keys
                for (key, server_value) in server_obj {
                    match client_obj.get(&key) {
                        Some(client_value) if client_value != &server_value => {
                            // Conflicting field: use server version (last-write-wins per field)
                            client_obj.insert(key, server_value);
                        }
                        None => {
                            // Server has a field that client doesn't: include it
                            client_obj.insert(key, server_value);
                        }
                        _ => {
                            // Same value or client-only field: keep client's value
                        }
                    }
                }

                // Serialize merged result
                serde_json::to_vec(&serde_json::Value::Object(client_obj))
                    .map_err(|e| format!("Failed to serialize merged data: {}", e))
            }
            _ => {
                // Not JSON objects, fall back to server version
                Ok(server_data.to_vec())
            }
        }
    }

    /// Merge two values with a custom merge function
    ///
    /// Allows callers to provide custom merge logic for specific value types.
    pub fn merge_with_custom<F>(&self, conflict: &Conflict, merge_fn: F) -> Result<Vec<u8>, String>
    where
        F: Fn(&[u8], &[u8]) -> Result<Vec<u8>, String>,
    {
        merge_fn(&conflict.client_version, &conflict.server_version)
    }
}

impl Default for ConflictManager {
    fn default() -> Self {
        Self::new(ConflictResolution::UseServer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_conflict_detection() {
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut client_clock = VectorClock::new();
        client_clock.increment(node1);

        let mut server_clock = VectorClock::new();
        server_clock.increment(node2);

        let manager = ConflictManager::default();
        assert!(manager.detect_conflict(&client_clock, &server_clock));
    }

    #[test]
    fn test_conflict_resolution_use_client() {
        let conflict = Conflict {
            id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![1],
            conflict_type: ConflictType::ConcurrentUpdate,
            client_version: vec![1, 2, 3],
            server_version: vec![4, 5, 6],
            resolution: ConflictResolution::UseClient,
        };

        let manager = ConflictManager::new(ConflictResolution::UseClient);
        let resolved = manager.resolve(&conflict).unwrap();
        assert_eq!(resolved, vec![1, 2, 3]);
    }

    #[test]
    fn test_conflict_resolution_use_server() {
        let conflict = Conflict {
            id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![1],
            conflict_type: ConflictType::ConcurrentUpdate,
            client_version: vec![1, 2, 3],
            server_version: vec![4, 5, 6],
            resolution: ConflictResolution::UseServer,
        };

        let manager = ConflictManager::new(ConflictResolution::UseServer);
        let resolved = manager.resolve(&conflict).unwrap();
        assert_eq!(resolved, vec![4, 5, 6]);
    }
}
