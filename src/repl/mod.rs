//! Interactive REPL for HeliosDB Lite
//!
//! Provides a SQLite/PostgreSQL-like interactive shell with:
//! - Multi-line SQL editing
//! - Command history
//! - Auto-completion
//! - Meta commands (\d, \dt, \q, etc.)
//! - Pretty-printed results

mod shell;
mod completer;
mod formatter;
mod commands;
mod help_manager;

pub use shell::ReplShell;
pub use commands::MetaCommand;
pub use help_manager::HelpManager;
pub use commands::MetaCommandResult;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};


/// REPL configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplConfig {
    /// Enable query timing display
    #[serde(default = "default_show_timing")]
    pub show_timing: bool,
    /// History file path
    #[serde(default = "default_history_path")]
    pub history_path: Option<String>,
    /// Maximum history entries
    #[serde(default = "default_max_history")]
    pub max_history: usize,
    /// Path to configuration file (for reload support)
    #[serde(skip)]
    pub config_path: Option<PathBuf>,
    /// Output format: table, json, csv
    #[serde(default = "default_output_format")]
    pub output_format: OutputFormat,
    /// Show row count after queries
    #[serde(default = "default_show_row_count")]
    pub show_row_count: bool,
    /// Auto-commit mode (commit each statement)
    #[serde(default = "default_auto_commit")]
    pub auto_commit: bool,
    /// Null display string
    #[serde(default = "default_null_display")]
    pub null_display: String,
    /// Maximum column width for display
    #[serde(default = "default_max_column_width")]
    pub max_column_width: usize,
}

fn default_show_timing() -> bool { true }
fn default_history_path() -> Option<String> { Some(".heliosdb_history".to_string()) }
fn default_max_history() -> usize { 1000 }
fn default_output_format() -> OutputFormat { OutputFormat::Table }
fn default_show_row_count() -> bool { true }
fn default_auto_commit() -> bool { true }
fn default_null_display() -> String { "NULL".to_string() }
fn default_max_column_width() -> usize { 50 }

/// Output format for query results
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// ASCII table format (default)
    Table,
    /// JSON format
    Json,
    /// CSV format
    Csv,
    /// Vertical format (one row per line)
    Vertical,
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            show_timing: default_show_timing(),
            history_path: default_history_path(),
            max_history: default_max_history(),
            config_path: None,
            output_format: default_output_format(),
            show_row_count: default_show_row_count(),
            auto_commit: default_auto_commit(),
            null_display: default_null_display(),
            max_column_width: default_max_column_width(),
        }
    }
}

impl ReplConfig {
    /// Create a new REPL config with a config file path
    pub fn with_config_path(mut self, path: impl AsRef<Path>) -> Self {
        self.config_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Load REPL configuration from a TOML file
    ///
    /// The file can contain a `[repl]` section with REPL-specific settings,
    /// or the settings can be at the root level.
    pub fn from_file(path: impl AsRef<Path>) -> crate::Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;

        // Try to parse as a full config with [repl] section first
        #[derive(Deserialize)]
        struct ConfigWrapper {
            #[serde(default)]
            repl: Option<ReplConfig>,
        }

        let mut config = if let Ok(wrapper) = toml::from_str::<ConfigWrapper>(&content) {
            if let Some(repl_config) = wrapper.repl {
                repl_config
            } else {
                // No [repl] section, try parsing as direct ReplConfig
                toml::from_str::<ReplConfig>(&content)
                    .map_err(|e| crate::Error::config(format!("Failed to parse REPL config: {}", e)))?
            }
        } else {
            // Try to parse as direct ReplConfig
            toml::from_str::<ReplConfig>(&content)
                .map_err(|e| crate::Error::config(format!("Failed to parse REPL config: {}", e)))?
        };

        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    /// Reload configuration from the stored config path
    ///
    /// Returns the new config if a config path was set and the file is readable,
    /// otherwise returns an error.
    pub fn reload(&self) -> crate::Result<Self> {
        match &self.config_path {
            Some(path) => Self::from_file(path),
            None => Err(crate::Error::config(
                "No configuration file path set. Start REPL with --config to enable reload."
            )),
        }
    }

    /// Save configuration to a file
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> crate::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::config(format!("Failed to serialize REPL config: {}", e)))?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_repl_config_default() {
        let config = ReplConfig::default();
        assert!(config.show_timing);
        assert_eq!(config.history_path, Some(".heliosdb_history".to_string()));
        assert_eq!(config.max_history, 1000);
        assert!(config.config_path.is_none());
        assert_eq!(config.output_format, OutputFormat::Table);
        assert!(config.show_row_count);
        assert!(config.auto_commit);
        assert_eq!(config.null_display, "NULL");
        assert_eq!(config.max_column_width, 50);
    }

    #[test]
    fn test_repl_config_serialization() {
        let config = ReplConfig::default();
        let toml_str = toml::to_string(&config).expect("Failed to serialize config");

        assert!(toml_str.contains("show_timing = true"));
        assert!(toml_str.contains("output_format = \"table\""));
        assert!(toml_str.contains("show_row_count = true"));
    }

    #[test]
    fn test_repl_config_from_file() {
        let toml_content = r#"
            show_timing = false
            max_history = 500
            output_format = "json"
            show_row_count = false
            auto_commit = false
            null_display = "<null>"
            max_column_width = 100
        "#;

        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        temp_file.write_all(toml_content.as_bytes()).expect("Failed to write temp file");

        let config = ReplConfig::from_file(temp_file.path()).expect("Failed to load config");

        assert!(!config.show_timing);
        assert_eq!(config.max_history, 500);
        assert_eq!(config.output_format, OutputFormat::Json);
        assert!(!config.show_row_count);
        assert!(!config.auto_commit);
        assert_eq!(config.null_display, "<null>");
        assert_eq!(config.max_column_width, 100);
        assert!(config.config_path.is_some());
    }

    #[test]
    fn test_repl_config_from_file_with_repl_section() {
        let toml_content = r#"
            [storage]
            memory_only = true

            [repl]
            show_timing = false
            output_format = "csv"
            max_column_width = 200
        "#;

        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        temp_file.write_all(toml_content.as_bytes()).expect("Failed to write temp file");

        let config = ReplConfig::from_file(temp_file.path()).expect("Failed to load config");

        assert!(!config.show_timing);
        assert_eq!(config.output_format, OutputFormat::Csv);
        assert_eq!(config.max_column_width, 200);
    }

    #[test]
    fn test_repl_config_reload() {
        let toml_content = r#"
            show_timing = true
            output_format = "table"
        "#;

        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        temp_file.write_all(toml_content.as_bytes()).expect("Failed to write temp file");

        let config = ReplConfig::from_file(temp_file.path()).expect("Failed to load config");
        assert!(config.show_timing);

        // Modify the file
        let new_content = r#"
            show_timing = false
            output_format = "json"
        "#;
        std::fs::write(temp_file.path(), new_content).expect("Failed to update temp file");

        // Reload config
        let reloaded = config.reload().expect("Failed to reload config");

        assert!(!reloaded.show_timing);
        assert_eq!(reloaded.output_format, OutputFormat::Json);
    }

    #[test]
    fn test_repl_config_reload_no_path() {
        let config = ReplConfig::default();
        let result = config.reload();

        assert!(result.is_err());
    }

    #[test]
    fn test_repl_config_save_to_file() {
        let mut config = ReplConfig::default();
        config.show_timing = false;
        config.output_format = OutputFormat::Csv;
        config.max_column_width = 150;

        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        config.save_to_file(temp_file.path()).expect("Failed to save config");

        // Load it back
        let loaded = ReplConfig::from_file(temp_file.path()).expect("Failed to load config");

        assert!(!loaded.show_timing);
        assert_eq!(loaded.output_format, OutputFormat::Csv);
        assert_eq!(loaded.max_column_width, 150);
    }

    #[test]
    fn test_output_format_variants() {
        assert_eq!(OutputFormat::Table, OutputFormat::Table);
        assert_eq!(OutputFormat::Json, OutputFormat::Json);
        assert_eq!(OutputFormat::Csv, OutputFormat::Csv);
        assert_eq!(OutputFormat::Vertical, OutputFormat::Vertical);
    }

    #[test]
    fn test_config_with_path() {
        let config = ReplConfig::default()
            .with_config_path("/some/path/config.toml");

        assert_eq!(
            config.config_path,
            Some(PathBuf::from("/some/path/config.toml"))
        );
    }
}
