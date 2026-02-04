//! Dump command implementation

use crate::{EmbeddedDatabase, Result, Error};
use crate::storage::{DumpManager, DumpOptions, DumpMode, DumpCompressionType};
use std::path::PathBuf;
use indicatif::{ProgressBar, ProgressStyle};
use colored::Colorize;

/// Dump command
pub struct DumpCommand {
    /// Output file path
    pub output: PathBuf,
    /// Append mode (incremental)
    pub append: bool,
    /// Compression type
    pub compression: String,
    /// Connection string (for server mode)
    pub connection: Option<String>,
    /// Verbose output
    pub verbose: bool,
    /// Data directory (for embedded mode)
    pub data_dir: Option<PathBuf>,
    /// In-memory mode
    pub memory: bool,
}

impl DumpCommand {
    /// Execute the dump command
    pub fn execute(&self) -> Result<()> {
        if self.verbose {
            println!("{}", "HeliosDB Dump Utility".bold());
            println!();
        }

        // Parse compression type
        let compression = DumpCompressionType::from_str(&self.compression)?;

        // Determine data directory
        let data_dir = if self.memory {
            return Err(Error::config(
                "Cannot dump from in-memory database without data directory".to_string()
            ));
        } else if let Some(ref data_dir) = self.data_dir {
            if self.verbose {
                println!("{} {}", "Database directory:".dimmed(), data_dir.display());
            }
            data_dir.clone()
        } else if let Some(ref _conn) = self.connection {
            return Err(Error::config(
                "Server mode dump not yet implemented. Use --data-dir for embedded mode.".to_string()
            ));
        } else {
            return Err(Error::config(
                "Either --data-dir or --connection must be specified".to_string()
            ));
        };

        // Open database for reading
        let db = EmbeddedDatabase::new(&data_dir)?;

        // Create dump manager
        let dump_manager = DumpManager::new(data_dir, compression);

        // Determine dump mode
        let mode = if self.append {
            DumpMode::Incremental
        } else {
            DumpMode::Full
        };

        // Create dump options
        let options = DumpOptions {
            output_path: self.output.clone(),
            mode,
            compression,
            append: self.append,
            tables: None, // Table filtering not yet implemented in CLI
            verbose: self.verbose,
            connection: self.connection.clone(),
            format: crate::storage::DumpOutputFormat::Binary, // Default to binary for now
        };

        if self.verbose {
            println!();
            println!("{}", "Dump configuration:".bold());
            println!("  Mode: {}", if self.append { "Incremental" } else { "Full" });
            println!("  Compression: {:?}", compression);
            println!("  Output: {}", self.output.display());
            println!();
        }

        // Show progress bar if not verbose
        let progress = if !self.verbose {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.green} {msg}")
                    .map_err(|e| Error::io(format!("Failed to set progress style: {}", e)))?
            );
            pb.set_message("Dumping database...");
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            Some(pb)
        } else {
            None
        };

        // Perform dump
        let report = dump_manager.dump(&options, &db)?;

        // Finish progress bar
        if let Some(pb) = progress {
            pb.finish_with_message("Dump completed");
        }

        // Print report
        println!();
        println!("{}", "Dump completed successfully!".green().bold());
        println!();
        println!("{}", "Summary:".bold());
        println!("  Dump ID: {}", report.dump_id);
        println!("  Tables: {}", report.tables_dumped);
        println!("  Rows: {}", format_number(report.rows_dumped));
        println!("  Size (compressed): {}", format_bytes(report.bytes_written));
        println!("  Size (uncompressed): {}", format_bytes(report.bytes_uncompressed));
        println!("  Compression ratio: {:.1}%", report.compression_ratio * 100.0);
        println!("  Duration: {}", format_duration(report.duration_ms));
        println!();
        println!("Dump file: {}", self.output.display().to_string().cyan());

        Ok(())
    }
}

/// Format number with thousands separators
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Format bytes in human-readable form
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

/// Format duration in human-readable form
fn format_duration(ms: u64) -> String {
    if ms >= 60000 {
        format!("{:.1} min", ms as f64 / 60000.0)
    } else if ms >= 1000 {
        format!("{:.1} sec", ms as f64 / 1000.0)
    } else {
        format!("{} ms", ms)
    }
}
