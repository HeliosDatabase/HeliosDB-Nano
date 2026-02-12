//! Materialized View SQL Parser
//!
//! Parses SQL for materialized views with Phase 3 options:
//! - CREATE MATERIALIZED VIEW ... WITH (auto_refresh = true, ...)
//! - REFRESH MATERIALIZED VIEW [CONCURRENTLY] ...
//! - DROP MATERIALIZED VIEW [IF EXISTS] ...

use crate::{Result, Error};
use super::super::logical_plan::{LogicalPlan, MaterializedViewOption};

/// Parser for materialized view SQL
pub struct MaterializedViewParser;

impl MaterializedViewParser {
    /// Parse CREATE MATERIALIZED VIEW options
    ///
    /// Syntax:
    /// ```sql
    /// CREATE MATERIALIZED VIEW view_name AS <query>
    /// WITH (
    ///     auto_refresh = true,
    ///     threshold_table_size = '1GB',
    ///     threshold_dml_rate = 100,
    ///     max_cpu_percent = 15,
    ///     lazy_update = true,
    ///     lazy_catchup_window = '1 hour',
    ///     distribution = 'hash(user_id)',
    ///     replication_factor = 3
    /// )
    /// ```
    pub fn parse_mv_options(options_str: &str) -> Result<Vec<MaterializedViewOption>> {
        let mut options = Vec::new();

        // Split by comma and parse key=value pairs
        for pair in options_str.split(',') {
            let parts: Vec<&str> = pair.split('=').map(|s| s.trim()).collect();
            if parts.len() != 2 {
                continue;
            }

            let key = parts.first().map(|s| s.to_lowercase()).unwrap_or_default();
            let value = parts.get(1).map(|s| s.trim_matches('\'').trim_matches('"')).unwrap_or_default();

            match key.as_str() {
                "auto_refresh" => {
                    let enabled = value.to_lowercase() == "true";
                    options.push(MaterializedViewOption::AutoRefresh(enabled));
                }
                "threshold_table_size" => {
                    options.push(MaterializedViewOption::ThresholdTableSize(value.to_string()));
                }
                "threshold_dml_rate" => {
                    let rate = value.parse::<usize>()
                        .map_err(|_| Error::query_execution("Invalid threshold_dml_rate"))?;
                    options.push(MaterializedViewOption::ThresholdDmlRate(rate));
                }
                "max_cpu_percent" => {
                    let percent = value.parse::<f32>()
                        .map_err(|_| Error::query_execution("Invalid max_cpu_percent"))?;
                    options.push(MaterializedViewOption::MaxCpuPercent(percent));
                }
                "lazy_update" => {
                    let enabled = value.to_lowercase() == "true";
                    options.push(MaterializedViewOption::LazyUpdate(enabled));
                }
                "lazy_catchup_window" => {
                    options.push(MaterializedViewOption::LazyCatchupWindow(value.to_string()));
                }
                "distribution" => {
                    options.push(MaterializedViewOption::Distribution(value.to_string()));
                }
                "replication_factor" => {
                    let rf = value.parse::<usize>()
                        .map_err(|_| Error::query_execution("Invalid replication_factor"))?;
                    options.push(MaterializedViewOption::ReplicationFactor(rf));
                }
                _ => {
                    return Err(Error::query_execution(format!("Unknown MV option: {}", key)));
                }
            }
        }

        Ok(options)
    }

    /// Parse CREATE MATERIALIZED VIEW statement
    ///
    /// Returns a placeholder plan - actual query parsing happens separately
    pub fn parse_create_mv(
        name: String,
        query_plan: LogicalPlan,
        options_str: Option<&str>,
        if_not_exists: bool,
    ) -> Result<LogicalPlan> {
        let options = if let Some(opts) = options_str {
            Self::parse_mv_options(opts)?
        } else {
            vec![]
        };

        Ok(LogicalPlan::CreateMaterializedView {
            name,
            query: Box::new(query_plan),
            options,
            if_not_exists,
        })
    }

    /// Parse REFRESH MATERIALIZED VIEW statement
    pub fn parse_refresh_mv(
        name: String,
        concurrent: bool,
        incremental: bool,
    ) -> Result<LogicalPlan> {
        Ok(LogicalPlan::RefreshMaterializedView {
            name,
            concurrent,
            incremental,
        })
    }

    /// Parse DROP MATERIALIZED VIEW statement
    pub fn parse_drop_mv(
        name: String,
        if_exists: bool,
    ) -> Result<LogicalPlan> {
        Ok(LogicalPlan::DropMaterializedView {
            name,
            if_exists,
        })
    }

    /// Detect if SQL contains materialized view syntax
    pub fn contains_mv_syntax(sql: &str) -> bool {
        let upper = sql.to_uppercase();
        upper.contains("MATERIALIZED VIEW") || upper.contains("REFRESH MATERIALIZED")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mv_options() {
        let options_str = "auto_refresh=true, max_cpu_percent=15, threshold_dml_rate=100";
        let options = MaterializedViewParser::parse_mv_options(options_str).unwrap();

        assert_eq!(options.len(), 3);
        assert!(matches!(options[0], MaterializedViewOption::AutoRefresh(true)));
        assert!(matches!(options[1], MaterializedViewOption::MaxCpuPercent(15.0)));
        assert!(matches!(options[2], MaterializedViewOption::ThresholdDmlRate(100)));
    }

    #[test]
    fn test_parse_refresh_mv() {
        let plan = MaterializedViewParser::parse_refresh_mv(
            "user_stats".to_string(),
            true,
            false,
        ).unwrap();

        match plan {
            LogicalPlan::RefreshMaterializedView { name, concurrent, incremental } => {
                assert_eq!(name, "user_stats");
                assert_eq!(concurrent, true);
                assert_eq!(incremental, false);
            }
            _ => panic!("Expected RefreshMaterializedView plan"),
        }
    }

    #[test]
    fn test_parse_drop_mv() {
        let plan = MaterializedViewParser::parse_drop_mv(
            "user_stats".to_string(),
            true,
        ).unwrap();

        match plan {
            LogicalPlan::DropMaterializedView { name, if_exists } => {
                assert_eq!(name, "user_stats");
                assert_eq!(if_exists, true);
            }
            _ => panic!("Expected DropMaterializedView plan"),
        }
    }

    #[test]
    fn test_contains_mv_syntax() {
        assert!(MaterializedViewParser::contains_mv_syntax(
            "CREATE MATERIALIZED VIEW user_stats AS SELECT ..."
        ));
        assert!(MaterializedViewParser::contains_mv_syntax(
            "REFRESH MATERIALIZED VIEW user_stats"
        ));
        assert!(!MaterializedViewParser::contains_mv_syntax(
            "CREATE VIEW user_stats AS SELECT ..."
        ));
    }
}
