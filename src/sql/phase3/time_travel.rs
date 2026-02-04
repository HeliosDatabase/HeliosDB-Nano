//! Time-Travel SQL Parser
//!
//! Parses SQL extensions for time-travel queries:
//! - SELECT ... FROM table AS OF TIMESTAMP '2025-11-15 06:00:00'
//! - SELECT ... FROM table AS OF TRANSACTION 987654
//! - SELECT ... FROM table AS OF SCN 123456789
//! - SELECT ... FROM table VERSIONS BETWEEN ...

use crate::{Result, Error};
use super::super::logical_plan::AsOfClause;

/// Parser for time-travel SQL clauses
pub struct TimeTravelParser;

impl TimeTravelParser {
    /// Parse AS OF clause from table reference
    ///
    /// Called when parser encounters: FROM table AS OF <clause>
    pub fn parse_as_of_clause(clause_str: &str) -> Result<AsOfClause> {
        let trimmed = clause_str.trim().to_uppercase();

        if trimmed == "NOW" {
            return Ok(AsOfClause::Now);
        }

        // Parse TIMESTAMP '2025-11-15 06:00:00'
        if trimmed.starts_with("TIMESTAMP") {
            let ts_str = clause_str
                .trim()
                .strip_prefix("TIMESTAMP")
                .or_else(|| clause_str.trim().strip_prefix("timestamp"))
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
            clause_str
        )))
    }

    /// Check if a query contains time-travel syntax
    ///
    /// Looks for keywords: AS OF, VERSIONS BETWEEN
    pub fn contains_time_travel_syntax(sql: &str) -> bool {
        let upper = sql.to_uppercase();
        upper.contains("AS OF") || upper.contains("VERSIONS BETWEEN")
    }

    /// Extract AS OF clause from SQL query
    ///
    /// Returns the clause string if found, None otherwise
    pub fn extract_as_of_from_sql(sql: &str) -> Option<String> {
        let upper = sql.to_uppercase();

        if let Some(pos) = upper.find("AS OF") {
            // Find the end of the AS OF clause (next keyword or end of string)
            let start = pos + 5; // "AS OF".len()
            let remainder = &sql[start..];

            // Find next SQL keyword that ends the clause
            let keywords = ["WHERE", "GROUP BY", "ORDER BY", "LIMIT", "JOIN", "AND", "OR", ";"];
            let mut end = remainder.len();

            for keyword in &keywords {
                if let Some(kw_pos) = remainder.to_uppercase().find(keyword) {
                    if kw_pos < end {
                        end = kw_pos;
                    }
                }
            }

            let clause = remainder[..end].trim();
            return Some(clause.to_string());
        }

        None
    }

    /// Parse VERSIONS BETWEEN clause
    ///
    /// Syntax:
    /// ```sql
    /// VERSIONS BETWEEN TIMESTAMP '2025-11-15 06:00:00'
    ///          AND TIMESTAMP '2025-11-15 07:00:00'
    /// ```
    ///
    /// Returns (start_clause, end_clause)
    pub fn parse_versions_between(clause_str: &str) -> Result<(AsOfClause, AsOfClause)> {
        let upper = clause_str.to_uppercase();

        // Find AND separator
        let and_pos = upper.find(" AND ")
            .ok_or_else(|| Error::query_execution("VERSIONS BETWEEN requires AND"))?;

        let start_str = &clause_str[..and_pos].trim();
        let end_str = &clause_str[and_pos + 5..].trim(); // " AND ".len() = 5

        let start_clause = Self::parse_as_of_clause(start_str)?;
        let end_clause = Self::parse_as_of_clause(end_str)?;

        Ok((start_clause, end_clause))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_as_of_now() {
        let clause = TimeTravelParser::parse_as_of_clause("NOW").unwrap();
        assert_eq!(clause, AsOfClause::Now);
    }

    #[test]
    fn test_parse_as_of_timestamp() {
        let clause = TimeTravelParser::parse_as_of_clause("TIMESTAMP '2025-11-15 06:00:00'").unwrap();
        assert_eq!(clause, AsOfClause::Timestamp("2025-11-15 06:00:00".to_string()));
    }

    #[test]
    fn test_contains_time_travel_syntax() {
        assert!(TimeTravelParser::contains_time_travel_syntax(
            "SELECT * FROM orders AS OF TIMESTAMP '2025-11-15 06:00:00'"
        ));
        assert!(TimeTravelParser::contains_time_travel_syntax(
            "SELECT * FROM orders VERSIONS BETWEEN TIMESTAMP '2025-11-15 06:00:00' AND NOW"
        ));
        assert!(!TimeTravelParser::contains_time_travel_syntax(
            "SELECT * FROM orders WHERE id = 1"
        ));
    }

    #[test]
    fn test_extract_as_of_from_sql() {
        let sql = "SELECT * FROM orders AS OF TIMESTAMP '2025-11-15 06:00:00' WHERE id = 1";
        let clause = TimeTravelParser::extract_as_of_from_sql(sql).unwrap();
        assert!(clause.contains("TIMESTAMP"));
        assert!(clause.contains("2025-11-15 06:00:00"));
    }

    #[test]
    fn test_parse_versions_between() {
        let clause_str = "TIMESTAMP '2025-11-15 06:00:00' AND TIMESTAMP '2025-11-15 07:00:00'";
        let (start, end) = TimeTravelParser::parse_versions_between(clause_str).unwrap();

        assert_eq!(start, AsOfClause::Timestamp("2025-11-15 06:00:00".to_string()));
        assert_eq!(end, AsOfClause::Timestamp("2025-11-15 07:00:00".to_string()));
    }
}
