//! SQL SET and SHOW command implementation
//!
//! Provides session-level and global settings management for HeliosDB Nano.

use crate::{Result, Error};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Setting value types
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    /// Boolean value (on/off, true/false, yes/no, 1/0)
    Boolean(bool),
    /// Integer value
    Integer(i64),
    /// String value
    String(String),
    /// Duration in milliseconds
    Duration(u64),
}

impl SettingValue {
    /// Convert to boolean
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SettingValue::Boolean(b) => Some(*b),
            SettingValue::Integer(i) => Some(*i != 0),
            SettingValue::String(s) => {
                match s.to_lowercase().as_str() {
                    "on" | "true" | "yes" | "1" => Some(true),
                    "off" | "false" | "no" | "0" => Some(false),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Convert to integer
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SettingValue::Integer(i) => Some(*i),
            SettingValue::Boolean(b) => Some(if *b { 1 } else { 0 }),
            SettingValue::String(s) => s.parse().ok(),
            SettingValue::Duration(d) => Some(*d as i64),
        }
    }

    /// Convert to string
    pub fn as_string(&self) -> String {
        match self {
            SettingValue::Boolean(b) => if *b { "on" } else { "off" }.to_string(),
            SettingValue::Integer(i) => i.to_string(),
            SettingValue::String(s) => s.clone(),
            SettingValue::Duration(d) => format!("{}ms", d),
        }
    }

    /// Convert to duration (milliseconds)
    pub fn as_duration_ms(&self) -> Option<u64> {
        match self {
            SettingValue::Duration(d) => Some(*d),
            SettingValue::Integer(i) => Some(*i as u64),
            SettingValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }
}

/// Parse setting value from string
pub fn parse_setting_value(s: &str) -> SettingValue {
    // Try boolean
    match s.to_lowercase().as_str() {
        "on" | "true" | "yes" => return SettingValue::Boolean(true),
        "off" | "false" | "no" => return SettingValue::Boolean(false),
        _ => {}
    }

    // Try integer
    if let Ok(i) = s.parse::<i64>() {
        return SettingValue::Integer(i);
    }

    // Default to string
    SettingValue::String(s.to_string())
}

/// Session settings manager
#[derive(Debug, Clone)]
pub struct SessionSettings {
    settings: Arc<RwLock<HashMap<String, SettingValue>>>,
}

impl SessionSettings {
    /// Create new session settings with defaults
    pub fn new() -> Self {
        let mut settings = HashMap::new();

        // Query execution settings
        settings.insert("statement_timeout".to_string(), SettingValue::Duration(0)); // 0 = unlimited
        settings.insert("query_timeout".to_string(), SettingValue::Duration(0)); // 0 = unlimited

        // Optimizer settings
        settings.insert("optimizer".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_seqscan".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_indexscan".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_hashjoin".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_mergejoin".to_string(), SettingValue::Boolean(true));
        settings.insert("enable_nestloop".to_string(), SettingValue::Boolean(true));

        // Memory settings
        settings.insert("work_mem".to_string(), SettingValue::Integer(4096)); // KB
        settings.insert("shared_buffers".to_string(), SettingValue::Integer(131072)); // KB (128MB)

        // Transaction settings
        settings.insert("transaction_isolation".to_string(),
            SettingValue::String("READ COMMITTED".to_string()));
        settings.insert("transaction_read_only".to_string(), SettingValue::Boolean(false));

        // Time-travel settings
        settings.insert("time_travel_enabled".to_string(), SettingValue::Boolean(true));

        // Compression settings
        settings.insert("default_compression".to_string(), SettingValue::String("zstd".to_string()));
        settings.insert("compression_level".to_string(), SettingValue::Integer(3));

        // Vector settings
        settings.insert("vector_index_type".to_string(), SettingValue::String("hnsw".to_string()));
        settings.insert("hnsw_ef_construction".to_string(), SettingValue::Integer(200));
        settings.insert("hnsw_m".to_string(), SettingValue::Integer(16));

        // Materialized view settings
        settings.insert("mv_auto_refresh".to_string(), SettingValue::Boolean(false));
        settings.insert("mv_max_cpu_percent".to_string(), SettingValue::Integer(15));

        // SMFI (Self-Maintaining Filter Index) settings
        settings.insert("smfi_enabled".to_string(), SettingValue::Boolean(true));
        settings.insert("smfi_tracking_enabled".to_string(), SettingValue::Boolean(true));
        settings.insert("smfi_bulk_load_threshold".to_string(), SettingValue::Integer(10000));

        // Bulk loading performance settings
        settings.insert("bulk_load_mode".to_string(), SettingValue::Boolean(false));
        settings.insert("smfi_parallel_enabled".to_string(), SettingValue::Boolean(true));
        settings.insert("smfi_max_cpu_percent".to_string(), SettingValue::Integer(15));
        settings.insert("smfi_delta_threshold".to_string(), SettingValue::Integer(1000));
        settings.insert("smfi_parallel_threshold".to_string(), SettingValue::Integer(10000));
        settings.insert("smfi_max_workers".to_string(), SettingValue::Integer(8));

        // Display settings
        settings.insert("client_encoding".to_string(), SettingValue::String("UTF8".to_string()));
        settings.insert("datestyle".to_string(), SettingValue::String("ISO, MDY".to_string()));
        settings.insert("timezone".to_string(), SettingValue::String("UTC".to_string()));

        // Server info (read-only)
        settings.insert("server_version".to_string(),
            SettingValue::String(env!("CARGO_PKG_VERSION").to_string()));
        settings.insert("server_encoding".to_string(), SettingValue::String("UTF8".to_string()));

        Self {
            settings: Arc::new(RwLock::new(settings)),
        }
    }

    /// Set a setting value
    pub fn set(&self, name: &str, value: SettingValue) -> Result<()> {
        let normalized_name = name.to_lowercase();

        // Check if setting is read-only
        if Self::is_read_only(&normalized_name) {
            return Err(Error::query_execution(format!(
                "Setting '{}' is read-only", name
            )));
        }

        // Validate setting value
        Self::validate_setting(&normalized_name, &value)?;

        let mut settings = self.settings.write()
            .map_err(|e| Error::Generic(format!("Failed to acquire settings lock: {}", e)))?;

        settings.insert(normalized_name, value);
        Ok(())
    }

    /// Get a setting value
    pub fn get(&self, name: &str) -> Option<SettingValue> {
        let normalized_name = name.to_lowercase();
        let settings = self.settings.read().ok()?;
        settings.get(&normalized_name).cloned()
    }

    /// Get all settings
    pub fn get_all(&self) -> HashMap<String, SettingValue> {
        self.settings.read()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Check if a setting is read-only
    fn is_read_only(name: &str) -> bool {
        matches!(name,
            "server_version" | "server_encoding" | "max_connections" | "port"
        )
    }

    /// Validate setting value
    fn validate_setting(name: &str, value: &SettingValue) -> Result<()> {
        match name {
            "transaction_isolation" => {
                if let Some(s) = match value {
                    SettingValue::String(s) => Some(s.as_str()),
                    _ => None,
                } {
                    let upper = s.to_uppercase();
                    if !matches!(upper.as_str(),
                        "READ UNCOMMITTED" | "READ COMMITTED" |
                        "REPEATABLE READ" | "SERIALIZABLE"
                    ) {
                        return Err(Error::query_execution(format!(
                            "Invalid transaction isolation level: {}", s
                        )));
                    }
                }
            }
            "default_compression" => {
                if let Some(s) = match value {
                    SettingValue::String(s) => Some(s.as_str()),
                    _ => None,
                } {
                    let lower = s.to_lowercase();
                    if !matches!(lower.as_str(), "none" | "zstd" | "lz4") {
                        return Err(Error::query_execution(format!(
                            "Invalid compression type: {}", s
                        )));
                    }
                }
            }
            "vector_index_type" => {
                if let Some(s) = match value {
                    SettingValue::String(s) => Some(s.as_str()),
                    _ => None,
                } {
                    let lower = s.to_lowercase();
                    if !matches!(lower.as_str(), "hnsw" | "flat" | "ivf") {
                        return Err(Error::query_execution(format!(
                            "Invalid vector index type: {}", s
                        )));
                    }
                }
            }
            "work_mem" | "shared_buffers" => {
                if let Some(val) = value.as_i64() {
                    if val < 0 {
                        return Err(Error::query_execution(format!(
                            "{} must be non-negative", name
                        )));
                    }
                }
            }
            "mv_max_cpu_percent" => {
                if let Some(val) = value.as_i64() {
                    if !(1..=100).contains(&val) {
                        return Err(Error::query_execution(
                            "mv_max_cpu_percent must be between 1 and 100".to_string()
                        ));
                    }
                }
            }
            "compression_level" => {
                if let Some(val) = value.as_i64() {
                    if !(1..=22).contains(&val) {
                        return Err(Error::query_execution(
                            "compression_level must be between 1 and 22".to_string()
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Reset a setting to default
    pub fn reset(&self, name: &str) -> Result<()> {
        let normalized_name = name.to_lowercase();

        if Self::is_read_only(&normalized_name) {
            return Err(Error::query_execution(format!(
                "Setting '{}' is read-only", name
            )));
        }

        // Get default value
        let default_settings = Self::new();
        if let Some(default_value) = default_settings.get(&normalized_name) {
            self.set(&normalized_name, default_value)?;
            Ok(())
        } else {
            Err(Error::query_execution(format!(
                "Unknown setting: {}", name
            )))
        }
    }

    /// Get statement timeout as Duration (None = unlimited)
    pub fn statement_timeout(&self) -> Option<Duration> {
        self.get("statement_timeout")
            .and_then(|v| v.as_duration_ms())
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis)
    }

    /// Get query timeout as Duration (None = unlimited)
    pub fn query_timeout(&self) -> Option<Duration> {
        self.get("query_timeout")
            .and_then(|v| v.as_duration_ms())
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis)
    }

    /// Check if optimizer is enabled
    pub fn optimizer_enabled(&self) -> bool {
        self.get("optimizer")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    /// Check if time-travel is enabled
    pub fn time_travel_enabled(&self) -> bool {
        self.get("time_travel_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_setting_value_parsing() {
        assert_eq!(parse_setting_value("on").as_bool(), Some(true));
        assert_eq!(parse_setting_value("off").as_bool(), Some(false));
        assert_eq!(parse_setting_value("123").as_i64(), Some(123));
        assert_eq!(parse_setting_value("hello").as_string(), "hello");
    }

    #[test]
    fn test_session_settings() {
        let settings = SessionSettings::new();

        // Test default values
        assert!(settings.optimizer_enabled());

        // Test set/get
        settings.set("optimizer", SettingValue::Boolean(false)).unwrap();
        assert!(!settings.optimizer_enabled());

        // Test read-only
        let result = settings.set("server_version", SettingValue::String("1.0".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn test_setting_validation() {
        let settings = SessionSettings::new();

        // Valid isolation level
        settings.set("transaction_isolation",
            SettingValue::String("SERIALIZABLE".to_string())).unwrap();

        // Invalid isolation level
        let result = settings.set("transaction_isolation",
            SettingValue::String("INVALID".to_string()));
        assert!(result.is_err());

        // Valid compression
        settings.set("default_compression",
            SettingValue::String("zstd".to_string())).unwrap();

        // Invalid compression
        let result = settings.set("default_compression",
            SettingValue::String("invalid".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn test_reset_setting() {
        let settings = SessionSettings::new();

        // Change a setting
        settings.set("optimizer", SettingValue::Boolean(false)).unwrap();
        assert!(!settings.optimizer_enabled());

        // Reset it
        settings.reset("optimizer").unwrap();
        assert!(settings.optimizer_enabled());
    }
}
