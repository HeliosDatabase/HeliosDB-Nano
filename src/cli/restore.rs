//! Restore command implementation

use crate::{EmbeddedDatabase, Result, Error};
use crate::storage::{DumpManager, RestoreOptions, DumpCompressionType};
use std::path::PathBuf;
use indicatif::{ProgressBar, ProgressStyle};
use colored::Colorize;

/// Restore command
pub struct RestoreCommand {
    /// Input dump file path
    pub input: PathBuf,
    /// Target data directory
    pub target: Option<PathBuf>,
    /// Verify dump integrity
    pub verify: bool,
    /// Connection string (for server mode)
    pub connection: Option<String>,
    /// Verbose output
    pub verbose: bool,
}

impl RestoreCommand {
    /// Execute the restore command
    pub fn execute(&self) -> Result<()> {
        if self.verbose {
            println!("{}", "HeliosDB Restore Utility".bold());
            println!();
        }

        // Verify input file exists
        if !self.input.exists() {
            return Err(Error::io(format!(
                "Dump file not found: {}",
                self.input.display()
            )));
        }

        // Determine target directory
        let target_dir = self.target.clone().unwrap_or_else(|| {
            PathBuf::from("./heliosdb-data-restored")
        });

        if self.verbose {
            println!("{} {}", "Input file:".dimmed(), self.input.display());
            println!("{} {}", "Target directory:".dimmed(), target_dir.display());
            println!();
        }

        // Create target directory if it doesn't exist
        if !target_dir.exists() {
            std::fs::create_dir_all(&target_dir)
                .map_err(|e| Error::io(format!("Failed to create target directory: {}", e)))?;
        }

        // Open database
        let mut db = if let Some(ref _conn) = self.connection {
            return Err(Error::config(
                "Server mode restore not yet implemented. Use --target for embedded mode.".to_string()
            ));
        } else {
            if self.verbose {
                println!("{}", "Opening target database...".dimmed());
            }
            EmbeddedDatabase::new(&target_dir)?
        };

        // Create dump manager
        let dump_manager = DumpManager::new(target_dir.clone(), DumpCompressionType::Zstd);

        // Create restore options
        let options = RestoreOptions {
            input_path: self.input.clone(),
            target: Some(target_dir.clone()),
            tables: None,
            verify: self.verify,
            verbose: self.verbose,
            connection: self.connection.clone(),
        };

        if self.verbose {
            println!();
            println!("{}", "Restore configuration:".bold());
            println!("  Verify: {}", self.verify);
            println!("  Input: {}", self.input.display());
            println!("  Target: {}", target_dir.display());
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
            pb.set_message("Restoring database...");
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            Some(pb)
        } else {
            None
        };

        // Perform restore
        let report = dump_manager.restore(&options, &mut db)?;

        // Finish progress bar
        if let Some(pb) = progress {
            pb.finish_with_message("Restore completed");
        }

        // Print report
        println!();
        println!("{}", "Restore completed successfully!".green().bold());
        println!();
        println!("{}", "Summary:".bold());
        println!("  Tables: {}", report.tables_restored);
        println!("  Rows: {}", format_number(report.rows_restored));
        println!("  Duration: {}", format_duration(report.duration_ms));
        println!();
        println!("Database restored to: {}", target_dir.display().to_string().cyan());

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
