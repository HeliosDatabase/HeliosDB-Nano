//! Database Branching SQL Parser
//!
//! Parses SQL extensions for database branching:
//! - CREATE DATABASE BRANCH <name> FROM <parent> AS OF <point>
//! - DROP DATABASE BRANCH [IF EXISTS] <name>
//! - MERGE DATABASE BRANCH <source> INTO <target>

use crate::{Result, Error};
use super::super::logical_plan::{
    LogicalPlan, AsOfClause, BranchOption, MergeOption,
    ConflictResolution,
};

/// Parser for database branching SQL
pub struct BranchingParser;

impl BranchingParser {
    /// Parse CREATE DATABASE BRANCH statement
    ///
    /// Syntax:
    /// ```sql
    /// CREATE DATABASE BRANCH <branch_name>
    /// FROM {CURRENT | <parent_branch>}
    /// AS OF {NOW | TIMESTAMP <ts> | TRANSACTION <txn> | SCN <scn>}
    /// [WITH (<option> = <value>, ...)]
    /// ```
    pub fn parse_create_branch(
        branch_name: String,
        parent: Option<String>,
        as_of_str: &str,
        options_str: Option<&str>,
    ) -> Result<LogicalPlan> {
        // Parse AS OF clause
        let as_of = Self::parse_as_of_clause(as_of_str)?;

        // Parse options
        let options = if let Some(opts) = options_str {
            Self::parse_branch_options(opts)?
        } else {
            vec![]
        };

        Ok(LogicalPlan::CreateBranch {
            branch_name,
            parent,
            as_of,
            options,
        })
    }

    /// Parse AS OF clause
    fn parse_as_of_clause(as_of_str: &str) -> Result<AsOfClause> {
        // Trim and remove trailing semicolon if present
        let trimmed = as_of_str.trim().trim_end_matches(';').to_uppercase();

        if trimmed == "NOW" {
            return Ok(AsOfClause::Now);
        }

        // Parse TIMESTAMP '2025-11-15 06:00:00'
        if trimmed.starts_with("TIMESTAMP") {
            let ts_str = as_of_str
                .trim()
                .strip_prefix("TIMESTAMP")
                .or_else(|| as_of_str.trim().strip_prefix("timestamp"))
                .ok_or_else(|| Error::query_execution("Invalid TIMESTAMP syntax"))?
                .trim()
                .trim_matches('\'')
                .trim_matches('"');
            return Ok(AsOfClause::Timestamp(ts_str.to_string()));
        }

        // Parse TRANSACTION 987654
        if trimmed.starts_with("TRANSACTION") {
            let txn_str = trimmed
                .strip_prefix("TRANSACTION")
                .ok_or_else(|| Error::query_execution("Invalid TRANSACTION syntax"))?
                .trim();
            let txn_id = txn_str.parse::<u64>()
                .map_err(|_| Error::query_execution("Invalid transaction ID"))?;
            return Ok(AsOfClause::Transaction(txn_id));
        }

        // Parse SCN 123456789
        if trimmed.starts_with("SCN") {
            let scn_str = trimmed
                .strip_prefix("SCN")
                .ok_or_else(|| Error::query_execution("Invalid SCN syntax"))?
                .trim();
            let scn = scn_str.parse::<u64>()
                .map_err(|_| Error::query_execution("Invalid SCN"))?;
            return Ok(AsOfClause::Scn(scn));
        }

        Err(Error::query_execution(format!(
            "Invalid AS OF clause: {}. Expected NOW, TIMESTAMP, TRANSACTION, or SCN",
            as_of_str
        )))
    }

    /// Parse branch options from WITH clause
    fn parse_branch_options(options_str: &str) -> Result<Vec<BranchOption>> {
        let mut options = Vec::new();

        // Simple parsing - split by comma and parse key=value pairs
        for pair in options_str.split(',') {
            let parts: Vec<&str> = pair.split('=').map(|s| s.trim()).collect();
            if parts.len() != 2 {
                continue;
            }

            let key = parts.first().map(|s| s.to_lowercase()).unwrap_or_default();
            let value = parts.get(1).map(|s| s.trim_matches('\'').trim_matches('"')).unwrap_or_default();

            match key.as_str() {
                "replication_factor" => {
                    let rf = value.parse::<usize>()
                        .map_err(|_| Error::query_execution("Invalid replication_factor"))?;
                    options.push(BranchOption::ReplicationFactor(rf));
                }
                "region" => {
                    options.push(BranchOption::Region(value.to_string()));
                }
                _ => {
                    return Err(Error::query_execution(format!("Unknown branch option: {}", key)));
                }
            }
        }

        Ok(options)
    }

    /// Parse DROP DATABASE BRANCH statement
    ///
    /// Syntax:
    /// ```sql
    /// DROP DATABASE BRANCH [IF EXISTS] <branch_name>
    /// ```
    pub fn parse_drop_branch(
        branch_name: String,
        if_exists: bool,
    ) -> Result<LogicalPlan> {
        Ok(LogicalPlan::DropBranch {
            branch_name,
            if_exists,
        })
    }

    /// Parse MERGE DATABASE BRANCH statement
    ///
    /// Syntax:
    /// ```sql
    /// MERGE DATABASE BRANCH <source> INTO <target>
    /// [WITH (<option> = <value>, ...)]
    /// ```
    pub fn parse_merge_branch(
        source: String,
        target: String,
        options_str: Option<&str>,
    ) -> Result<LogicalPlan> {
        let options = if let Some(opts) = options_str {
            Self::parse_merge_options(opts)?
        } else {
            vec![]
        };

        Ok(LogicalPlan::MergeBranch {
            source,
            target,
            options,
        })
    }

    /// Parse merge options
    fn parse_merge_options(options_str: &str) -> Result<Vec<MergeOption>> {
        let mut options = Vec::new();

        for pair in options_str.split(',') {
            let parts: Vec<&str> = pair.split('=').map(|s| s.trim()).collect();
            if parts.len() != 2 {
                continue;
            }

            let key = parts.first().map(|s| s.to_lowercase()).unwrap_or_default();
            let value = parts.get(1).map(|s| s.trim_matches('\'').trim_matches('"')).unwrap_or_default();

            match key.as_str() {
                "conflict_resolution" => {
                    let resolution = match value.to_lowercase().as_str() {
                        "branch_wins" => ConflictResolution::BranchWins,
                        "target_wins" => ConflictResolution::TargetWins,
                        "fail" => ConflictResolution::Fail,
                        _ => return Err(Error::query_execution(
                            format!("Invalid conflict_resolution: {}", value)
                        )),
                    };
                    options.push(MergeOption::ConflictResolution(resolution));
                }
                "delete_branch_after" => {
                    let delete = value.to_lowercase() == "true";
                    options.push(MergeOption::DeleteBranchAfter(delete));
                }
                _ => {
                    return Err(Error::query_execution(format!("Unknown merge option: {}", key)));
                }
            }
        }

        Ok(options)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_as_of_now() {
        let clause = BranchingParser::parse_as_of_clause("NOW").unwrap();
        assert_eq!(clause, AsOfClause::Now);
    }

    #[test]
    fn test_parse_as_of_timestamp() {
        let clause = BranchingParser::parse_as_of_clause("TIMESTAMP '2025-11-15 06:00:00'").unwrap();
        assert_eq!(clause, AsOfClause::Timestamp("2025-11-15 06:00:00".to_string()));
    }

    #[test]
    fn test_parse_as_of_transaction() {
        let clause = BranchingParser::parse_as_of_clause("TRANSACTION 987654").unwrap();
        assert_eq!(clause, AsOfClause::Transaction(987654));
    }

    #[test]
    fn test_parse_as_of_scn() {
        let clause = BranchingParser::parse_as_of_clause("SCN 123456789").unwrap();
        assert_eq!(clause, AsOfClause::Scn(123456789));
    }

    #[test]
    fn test_parse_create_branch() {
        let plan = BranchingParser::parse_create_branch(
            "test".to_string(),
            None, // CURRENT
            "NOW",
            None,
        ).unwrap();

        match plan {
            LogicalPlan::CreateBranch { branch_name, parent, as_of, .. } => {
                assert_eq!(branch_name, "test");
                assert_eq!(parent, None);
                assert_eq!(as_of, AsOfClause::Now);
            }
            _ => panic!("Expected CreateBranch plan"),
        }
    }

    #[test]
    fn test_parse_drop_branch() {
        let plan = BranchingParser::parse_drop_branch("test".to_string(), true).unwrap();

        match plan {
            LogicalPlan::DropBranch { branch_name, if_exists } => {
                assert_eq!(branch_name, "test");
                assert_eq!(if_exists, true);
            }
            _ => panic!("Expected DropBranch plan"),
        }
    }

    #[test]
    fn test_parse_merge_branch() {
        let plan = BranchingParser::parse_merge_branch(
            "staging".to_string(),
            "main".to_string(),
            Some("conflict_resolution='branch_wins'"),
        ).unwrap();

        match plan {
            LogicalPlan::MergeBranch { source, target, options } => {
                assert_eq!(source, "staging");
                assert_eq!(target, "main");
                assert_eq!(options.len(), 1);
            }
            _ => panic!("Expected MergeBranch plan"),
        }
    }
}
