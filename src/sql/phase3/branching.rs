//! Database Branching SQL Parser
//!
//! Parses SQL extensions for database branching:
//! - `CREATE DATABASE BRANCH <name> FROM <parent> AS OF <point>`
//! - `DROP DATABASE BRANCH [IF EXISTS] <name>`
//! - `MERGE DATABASE BRANCH <source> INTO <target>`

use super::super::logical_plan::{AsOfClause, BranchOption, ConflictResolution, LogicalPlan, MergeOption};
use crate::{Error, Result};

/// v10.7: the metadata key a parsed `expires_in` travels under, in
/// milliseconds, until CREATE BRANCH turns it into an absolute deadline.
pub const EXPIRES_IN_MS: &str = "expires_in_ms";

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
        if_not_exists: bool,
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
            if_not_exists,
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
            let txn_id = txn_str
                .parse::<u64>()
                .map_err(|_| Error::query_execution("Invalid transaction ID"))?;
            return Ok(AsOfClause::Transaction(txn_id));
        }

        // Parse SCN 123456789
        if trimmed.starts_with("SCN") {
            let scn_str = trimmed
                .strip_prefix("SCN")
                .ok_or_else(|| Error::query_execution("Invalid SCN syntax"))?
                .trim();
            let scn = scn_str
                .parse::<u64>()
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
        let options_str = Self::strip_option_parens(options_str);

        for (key, value) in Self::split_option_pairs("CREATE BRANCH", options_str)? {
            let value = value.as_str();

            match key.as_str() {
                "replication_factor" => {
                    let rf = value
                        .parse::<usize>()
                        .map_err(|_| Error::query_execution("Invalid replication_factor"))?;
                    options.push(BranchOption::ReplicationFactor(rf));
                }
                "region" => {
                    options.push(BranchOption::Region(value.to_string()));
                }
                // v10.7: a lifetime. Stored relative here and turned into an
                // absolute wall-clock deadline when the branch is created.
                "expires_in" => {
                    let ms = Self::parse_expires_in(value)?;
                    options.push(BranchOption::Metadata(EXPIRES_IN_MS.to_string(), ms.to_string()));
                }
                "label" => {
                    options.push(BranchOption::Metadata(
                        crate::storage::BRANCH_LABEL.to_string(),
                        value.to_string(),
                    ));
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
    pub fn parse_drop_branch(branch_name: String, if_exists: bool) -> Result<LogicalPlan> {
        Ok(LogicalPlan::DropBranch { branch_name, if_exists })
    }

    /// Parse MERGE DATABASE BRANCH statement
    ///
    /// Syntax:
    /// ```sql
    /// MERGE DATABASE BRANCH <source> INTO <target>
    /// [WITH (<option> = <value>, ...)]
    /// ```
    pub fn parse_merge_branch(source: String, target: String, options_str: Option<&str>) -> Result<LogicalPlan> {
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

    /// Strip one layer of wrapping parentheses from a `WITH (...)` option list.
    ///
    /// The statement parsers hand these functions the raw text following `WITH`,
    /// which includes the delimiters. Without this the first key parses as
    /// `"(conflict_resolution"` and every option list is rejected as unknown —
    /// so `WITH (...)` was unusable on **every** branch statement. Applied here,
    /// in the option parsers themselves, rather than at each capture site: there
    /// is more than one capture site and they drifted apart once already.
    fn strip_option_parens(options_str: &str) -> &str {
        let trimmed = options_str.trim();
        trimmed
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or(trimmed)
            .trim()
    }

    /// v10.1: split a WITH option list into `(key, value)` pairs.
    ///
    /// Quote-aware: `,` and `=` inside `'…'` or `"…"` belong to the value, so
    /// `region = 'a=b'` is one pair. A pair that is not exactly one key, one
    /// `=` and one value is a 42601 syntax error — `WITH (bogus)` and
    /// `WITH (replication_factor)` used to be skipped silently.
    fn split_option_pairs(statement: &str, list: &str) -> Result<Vec<(String, String)>> {
        let malformed = |pair: &str| {
            Error::sql_parse(format!(
                "{statement}: malformed option \"{}\": expected name = value",
                pair.trim()
            ))
        };
        let mut pieces: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut quote: Option<char> = None;
        for c in list.chars() {
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                    current.push(c);
                }
                None if c == '\'' || c == '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                None if c == ',' => pieces.push(std::mem::take(&mut current)),
                None => current.push(c),
            }
        }
        if quote.is_some() {
            return Err(Error::sql_parse(format!(
                "{statement}: unterminated quoted option value"
            )));
        }
        pieces.push(current);

        let mut pairs = Vec::with_capacity(pieces.len());
        for piece in &pieces {
            // The first `=` outside quotes.
            let mut quote: Option<char> = None;
            let mut split_at = None;
            for (i, c) in piece.char_indices() {
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None if c == '\'' || c == '"' => quote = Some(c),
                    None if c == '=' => {
                        split_at = Some(i);
                        break;
                    }
                    None => {}
                }
            }
            let Some(at) = split_at else {
                return Err(malformed(piece));
            };
            let key = piece.get(..at).unwrap_or("").trim();
            let raw_value = piece.get(at + 1..).unwrap_or("").trim();
            if key.is_empty() || raw_value.is_empty() || key.contains(char::is_whitespace) {
                return Err(malformed(piece));
            }
            let value = if raw_value.len() >= 2
                && ((raw_value.starts_with('\'') && raw_value.ends_with('\''))
                    || (raw_value.starts_with('"') && raw_value.ends_with('"')))
            {
                raw_value.get(1..raw_value.len() - 1).unwrap_or("").to_string()
            } else if raw_value.contains(['\'', '"', '=']) || raw_value.contains(char::is_whitespace) {
                return Err(malformed(piece));
            } else {
                raw_value.to_string()
            };
            pairs.push((key.to_lowercase(), value));
        }
        Ok(pairs)
    }

    /// v10.1: a boolean option takes true/false, on/off or 1/0 — anything
    /// else is an error (`delete_branch_after = yes` used to mean false).
    fn parse_option_bool(key: &str, value: &str) -> Result<bool> {
        match value.to_ascii_lowercase().as_str() {
            "true" | "on" | "1" => Ok(true),
            "false" | "off" | "0" => Ok(false),
            _ => Err(Error::sql_parse(format!(
                "invalid value for boolean option {key}: \"{value}\" (expected true or false)"
            ))),
        }
    }

    /// v10.7: `expires_in = '<n>s|m|h|d'` in milliseconds; positive, or a
    /// 42601 error (a typo must never create a branch that never expires).
    fn parse_expires_in(value: &str) -> Result<i64> {
        let bad = || {
            Error::sql_parse(format!(
                "invalid expires_in '{value}': expected a positive number followed by s, m, h or d (e.g. '2h')"
            ))
        };
        let value = value.trim();
        let unit = value.chars().last().ok_or_else(bad)?;
        let factor: i64 = match unit.to_ascii_lowercase() {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => return Err(bad()),
        };
        let number: i64 = value
            .get(..value.len() - unit.len_utf8())
            .unwrap_or("")
            .trim()
            .parse()
            .map_err(|_| bad())?;
        if number <= 0 {
            return Err(bad());
        }
        number.checked_mul(factor).ok_or_else(bad)
    }

    /// Parse merge options
    fn parse_merge_options(options_str: &str) -> Result<Vec<MergeOption>> {
        let mut options = Vec::new();
        let options_str = Self::strip_option_parens(options_str);

        for (key, value) in Self::split_option_pairs("MERGE BRANCH", options_str)? {
            let value = value.as_str();

            match key.as_str() {
                "conflict_resolution" => {
                    let resolution = match value.to_lowercase().as_str() {
                        "branch_wins" => ConflictResolution::BranchWins,
                        "target_wins" => ConflictResolution::TargetWins,
                        "fail" => ConflictResolution::Fail,
                        _ => {
                            return Err(Error::query_execution(format!(
                                "Invalid conflict_resolution: {}",
                                value
                            )))
                        }
                    };
                    options.push(MergeOption::ConflictResolution(resolution));
                }
                "delete_branch_after" => {
                    let delete = Self::parse_option_bool(&key, value)?;
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
            false,
        )
        .unwrap();

        match plan {
            LogicalPlan::CreateBranch {
                branch_name,
                parent,
                as_of,
                ..
            } => {
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
        )
        .unwrap();

        match plan {
            LogicalPlan::MergeBranch {
                source,
                target,
                options,
            } => {
                assert_eq!(source, "staging");
                assert_eq!(target, "main");
                assert_eq!(options.len(), 1);
            }
            _ => panic!("Expected MergeBranch plan"),
        }
    }
}
