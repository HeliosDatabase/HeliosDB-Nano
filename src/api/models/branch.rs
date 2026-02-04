//! Branch DTOs (Data Transfer Objects)
//!
//! Request and response models for branch operations.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Request to create a new branch
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateBranchRequest {
    /// Name of the new branch
    pub name: String,

    /// Parent branch name (defaults to "main" if not specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,

    /// Snapshot ID to branch from (defaults to current if not specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<u64>,

    /// Branch options
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<BranchOptionsDto>,
}

/// Branch creation/merge options
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BranchOptionsDto {
    /// Replication factor (for distributed mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replication_factor: Option<usize>,

    /// Region hint (for distributed mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Custom metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

/// Response for a single branch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchResponse {
    /// Branch name
    pub name: String,

    /// Branch ID
    pub branch_id: u64,

    /// Parent branch ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<u64>,

    /// Creation timestamp
    pub created_at: u64,

    /// Snapshot at branch point
    pub created_from_snapshot: u64,

    /// Current state
    pub state: BranchStateDto,

    /// Statistics
    pub stats: BranchStatsDto,

    /// Options
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<BranchOptionsDto>,
}

/// Branch state
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum BranchStateDto {
    /// Branch is active
    Active,

    /// Branch has been merged
    Merged {
        /// Target branch ID
        into_branch: u64,
        /// Merge timestamp
        at_timestamp: u64,
    },

    /// Branch has been dropped
    Dropped {
        /// Drop timestamp
        at_timestamp: u64,
    },
}

/// Branch statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchStatsDto {
    /// Number of modified keys in this branch
    pub modified_keys: u64,

    /// Approximate storage size (bytes)
    pub storage_bytes: u64,

    /// Number of commits in this branch
    pub commit_count: u64,

    /// Last activity timestamp
    pub last_modified: u64,
}

/// Response for listing branches
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchListResponse {
    /// List of branches
    pub branches: Vec<BranchResponse>,

    /// Total count
    pub total: usize,
}

/// Request to merge branches
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeBranchRequest {
    /// Target branch name to merge into
    pub target: String,

    /// Merge strategy
    #[serde(default)]
    pub strategy: MergeStrategyDto,
}

/// Merge strategy for resolving conflicts
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategyDto {
    /// Automatically resolve conflicts (prefer source)
    Auto,

    /// Fail on any conflict
    Manual,

    /// Always prefer source branch changes
    Theirs,

    /// Always prefer target branch changes
    Ours,
}

impl Default for MergeStrategyDto {
    fn default() -> Self {
        Self::Auto
    }
}

/// Response for merge operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeBranchResponse {
    /// Merge commit timestamp
    pub merge_timestamp: u64,

    /// Number of keys merged
    pub merged_keys: usize,

    /// Conflicts detected
    pub conflicts: Vec<MergeConflictDto>,

    /// Whether merge was completed
    pub completed: bool,

    /// Message describing the result
    pub message: String,
}

/// Conflict information for a key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflictDto {
    /// Key that has conflict
    pub key: String,

    /// Value in merge base (common ancestor)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_value: Option<String>,

    /// Value in source branch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_value: Option<String>,

    /// Value in target branch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_value: Option<String>,

    /// Timestamp in source
    pub source_timestamp: u64,

    /// Timestamp in target
    pub target_timestamp: u64,
}

// Conversion implementations

impl From<crate::storage::BranchState> for BranchStateDto {
    fn from(state: crate::storage::BranchState) -> Self {
        match state {
            crate::storage::BranchState::Active => BranchStateDto::Active,
            crate::storage::BranchState::Merged { into_branch, at_timestamp } => {
                BranchStateDto::Merged { into_branch, at_timestamp }
            }
            crate::storage::BranchState::Dropped { at_timestamp } => {
                BranchStateDto::Dropped { at_timestamp }
            }
        }
    }
}

impl From<crate::storage::BranchStats> for BranchStatsDto {
    fn from(stats: crate::storage::BranchStats) -> Self {
        BranchStatsDto {
            modified_keys: stats.modified_keys,
            storage_bytes: stats.storage_bytes,
            commit_count: stats.commit_count,
            last_modified: stats.last_modified,
        }
    }
}

impl From<crate::storage::BranchMetadata> for BranchResponse {
    fn from(metadata: crate::storage::BranchMetadata) -> Self {
        BranchResponse {
            name: metadata.name,
            branch_id: metadata.branch_id,
            parent_id: metadata.parent_id,
            created_at: metadata.created_at,
            created_from_snapshot: metadata.created_from_snapshot,
            state: metadata.state.into(),
            stats: metadata.stats.into(),
            options: Some(BranchOptionsDto {
                replication_factor: metadata.options.replication_factor,
                region: metadata.options.region,
                metadata: if metadata.options.metadata.is_empty() {
                    None
                } else {
                    Some(metadata.options.metadata)
                },
            }),
        }
    }
}

impl From<MergeStrategyDto> for crate::storage::MergeStrategy {
    fn from(strategy: MergeStrategyDto) -> Self {
        match strategy {
            MergeStrategyDto::Auto => crate::storage::MergeStrategy::Auto,
            MergeStrategyDto::Manual => crate::storage::MergeStrategy::Manual,
            MergeStrategyDto::Theirs => crate::storage::MergeStrategy::Theirs,
            MergeStrategyDto::Ours => crate::storage::MergeStrategy::Ours,
        }
    }
}

impl From<BranchOptionsDto> for crate::storage::BranchOptions {
    fn from(options: BranchOptionsDto) -> Self {
        crate::storage::BranchOptions {
            replication_factor: options.replication_factor,
            region: options.region,
            metadata: options.metadata.unwrap_or_default(),
            git_link: None,
        }
    }
}

impl From<crate::storage::MergeConflict> for MergeConflictDto {
    fn from(conflict: crate::storage::MergeConflict) -> Self {
        MergeConflictDto {
            key: conflict.key,
            base_value: conflict.base_value.map(|v| STANDARD.encode(&v)),
            source_value: conflict.source_value.map(|v| STANDARD.encode(&v)),
            target_value: conflict.target_value.map(|v| STANDARD.encode(&v)),
            source_timestamp: conflict.source_timestamp,
            target_timestamp: conflict.target_timestamp,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_create_branch_request_serialization() {
        let request = CreateBranchRequest {
            name: "dev".to_string(),
            parent: Some("main".to_string()),
            snapshot_id: Some(100),
            options: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: CreateBranchRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "dev");
        assert_eq!(deserialized.parent, Some("main".to_string()));
        assert_eq!(deserialized.snapshot_id, Some(100));
    }

    #[test]
    fn test_merge_strategy_serialization() {
        let strategies = vec![
            MergeStrategyDto::Auto,
            MergeStrategyDto::Manual,
            MergeStrategyDto::Theirs,
            MergeStrategyDto::Ours,
        ];

        for strategy in strategies {
            let json = serde_json::to_string(&strategy).unwrap();
            let deserialized: MergeStrategyDto = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, strategy);
        }
    }

    #[test]
    fn test_branch_state_dto_conversion() {
        let active = crate::storage::BranchState::Active;
        let dto: BranchStateDto = active.into();
        assert!(matches!(dto, BranchStateDto::Active));

        let merged = crate::storage::BranchState::Merged {
            into_branch: 1,
            at_timestamp: 100,
        };
        let dto: BranchStateDto = merged.into();
        assert!(matches!(dto, BranchStateDto::Merged { .. }));
    }
}
