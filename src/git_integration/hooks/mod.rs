//! Git Hooks Integration
//!
//! Provides shell script generation and installation for Git hooks that
//! automatically synchronize database branches with Git branches.
//!
//! ## Supported Hooks
//!
//! - **post-checkout**: Auto-switch DB branch when Git branch changes
//! - **pre-commit**: Validate schema before committing
//! - **post-merge**: Apply pending migrations and sync state after merge

#![allow(dead_code)]
#![allow(unused_variables)]

use crate::{Error, Result};
use std::path::PathBuf;
use std::fs;
use std::os::unix::fs::PermissionsExt;

/// Hook type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookType {
    /// Runs after git checkout
    PostCheckout,
    /// Runs before git commit
    PreCommit,
    /// Runs after git merge
    PostMerge,
}

impl HookType {
    /// Get hook filename
    pub fn filename(&self) -> &'static str {
        match self {
            HookType::PostCheckout => "post-checkout",
            HookType::PreCommit => "pre-commit",
            HookType::PostMerge => "post-merge",
        }
    }

    /// Get all hook types
    pub fn all() -> &'static [HookType] {
        &[HookType::PostCheckout, HookType::PreCommit, HookType::PostMerge]
    }
}

/// Hook status
#[derive(Debug, Clone)]
pub struct HookStatus {
    pub hook_type: HookType,
    pub installed: bool,
    pub path: PathBuf,
    pub is_helios_hook: bool,
}

/// Hook configuration
#[derive(Debug, Clone)]
pub struct HookConfig {
    /// Database path/connection
    pub database: String,
    /// Migration directory (optional)
    pub migration_dir: Option<String>,
    /// Enable verbose output
    pub verbose: bool,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            database: String::new(),
            migration_dir: None,
            verbose: false,
        }
    }
}

/// Git hooks manager
pub struct HookManager {
    /// Git repository root
    repo_path: PathBuf,
    /// Hook configuration
    config: HookConfig,
}

impl HookManager {
    /// Create a new hook manager
    pub fn new(repo_path: PathBuf, config: HookConfig) -> Self {
        Self { repo_path, config }
    }

    /// Get hooks directory path
    fn hooks_dir(&self) -> PathBuf {
        self.repo_path.join(".git").join("hooks")
    }

    /// Get path for a specific hook
    fn hook_path(&self, hook_type: HookType) -> PathBuf {
        self.hooks_dir().join(hook_type.filename())
    }

    /// Generate post-checkout hook script
    fn generate_post_checkout(&self) -> String {
        let db_arg = if self.config.database.is_empty() {
            String::new()
        } else {
            format!("--database \"{}\"", self.config.database)
        };

        format!(r#"#!/bin/sh
# HeliosDB-Nano Git Hook: post-checkout
# Auto-switch database branch when Git branch changes
#
# Arguments:
#   $1 - ref of previous HEAD
#   $2 - ref of new HEAD
#   $3 - flag: 1 = branch checkout, 0 = file checkout

PREV_HEAD="$1"
NEW_HEAD="$2"
CHECKOUT_TYPE="$3"

# Only run on branch checkouts, not file checkouts
if [ "$CHECKOUT_TYPE" != "1" ]; then
    exit 0
fi

# Get current Git branch
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)

if [ -z "$GIT_BRANCH" ]; then
    exit 0
fi

# Sync database with Git branch
if command -v helios >/dev/null 2>&1; then
    helios git sync {db_arg} 2>/dev/null || true
    {verbose}
fi
"#,
            db_arg = db_arg,
            verbose = if self.config.verbose {
                "echo \"[HeliosDB] Synced to branch: $GIT_BRANCH\""
            } else {
                ""
            }
        )
    }

    /// Generate pre-commit hook script
    fn generate_pre_commit(&self) -> String {
        let db_arg = if self.config.database.is_empty() {
            String::new()
        } else {
            format!("--database \"{}\"", self.config.database)
        };

        let migration_check = self.config.migration_dir.as_ref().map(|dir| {
            format!(r#"
# Validate migrations
if [ -d "{dir}" ]; then
    helios migration validate --dir "{dir}" {db_arg}
    if [ $? -ne 0 ]; then
        echo "[HeliosDB] Migration validation failed"
        exit 1
    fi
fi
"#,
                dir = dir,
                db_arg = db_arg
            )
        }).unwrap_or_default();

        format!(r#"#!/bin/sh
# HeliosDB-Nano Git Hook: pre-commit
# Validate schema and migrations before commit

{migration_check}

# Validate schema consistency
if command -v helios >/dev/null 2>&1; then
    helios schema validate {db_arg} 2>/dev/null
    if [ $? -ne 0 ]; then
        echo "[HeliosDB] Schema validation failed"
        exit 1
    fi
fi

exit 0
"#,
            migration_check = migration_check,
            db_arg = db_arg
        )
    }

    /// Generate post-merge hook script
    fn generate_post_merge(&self) -> String {
        let db_arg = if self.config.database.is_empty() {
            String::new()
        } else {
            format!("--database \"{}\"", self.config.database)
        };

        format!(r#"#!/bin/sh
# HeliosDB-Nano Git Hook: post-merge
# Sync database state after merge

# Get current Git branch
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)

if [ -z "$GIT_BRANCH" ]; then
    exit 0
fi

if command -v helios >/dev/null 2>&1; then
    # Apply any pending migrations
    helios migration apply {db_arg} --auto 2>/dev/null || true

    # Sync database state
    helios git sync {db_arg} 2>/dev/null || true
    {verbose}
fi

exit 0
"#,
            db_arg = db_arg,
            verbose = if self.config.verbose {
                "echo \"[HeliosDB] Synced after merge on branch: $GIT_BRANCH\""
            } else {
                ""
            }
        )
    }

    /// Generate hook script for a given type
    pub fn generate(&self, hook_type: HookType) -> String {
        match hook_type {
            HookType::PostCheckout => self.generate_post_checkout(),
            HookType::PreCommit => self.generate_pre_commit(),
            HookType::PostMerge => self.generate_post_merge(),
        }
    }

    /// Install a specific hook
    pub fn install(&self, hook_type: HookType) -> Result<()> {
        let hooks_dir = self.hooks_dir();

        // Create hooks directory if it doesn't exist
        if !hooks_dir.exists() {
            fs::create_dir_all(&hooks_dir)
                .map_err(|e| Error::io(format!("Failed to create hooks directory: {}", e)))?;
        }

        let hook_path = self.hook_path(hook_type);

        // Check if existing hook is not ours
        if hook_path.exists() {
            let content = fs::read_to_string(&hook_path)
                .map_err(|e| Error::io(format!("Failed to read existing hook: {}", e)))?;

            if !content.contains("HeliosDB-Nano Git Hook") {
                // Backup existing hook
                let backup_path = hook_path.with_extension("backup");
                fs::rename(&hook_path, &backup_path)
                    .map_err(|e| Error::io(format!("Failed to backup existing hook: {}", e)))?;
                tracing::info!("Backed up existing {} hook to {:?}", hook_type.filename(), backup_path);
            }
        }

        // Write hook script
        let script = self.generate(hook_type);
        fs::write(&hook_path, &script)
            .map_err(|e| Error::io(format!("Failed to write hook: {}", e)))?;

        // Make executable (Unix only)
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&hook_path)
                .map_err(|e| Error::io(format!("Failed to get hook permissions: {}", e)))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms)
                .map_err(|e| Error::io(format!("Failed to set hook permissions: {}", e)))?;
        }

        tracing::info!("Installed {} hook at {:?}", hook_type.filename(), hook_path);
        Ok(())
    }

    /// Install all hooks
    pub fn install_all(&self) -> Result<()> {
        for hook_type in HookType::all() {
            self.install(*hook_type)?;
        }
        Ok(())
    }

    /// Uninstall a specific hook
    pub fn uninstall(&self, hook_type: HookType) -> Result<()> {
        let hook_path = self.hook_path(hook_type);

        if hook_path.exists() {
            // Check if it's our hook
            let content = fs::read_to_string(&hook_path)
                .map_err(|e| Error::io(format!("Failed to read hook: {}", e)))?;

            if content.contains("HeliosDB-Nano Git Hook") {
                fs::remove_file(&hook_path)
                    .map_err(|e| Error::io(format!("Failed to remove hook: {}", e)))?;

                // Restore backup if exists
                let backup_path = hook_path.with_extension("backup");
                if backup_path.exists() {
                    fs::rename(&backup_path, &hook_path)
                        .map_err(|e| Error::io(format!("Failed to restore backup hook: {}", e)))?;
                    tracing::info!("Restored backup {} hook", hook_type.filename());
                }

                tracing::info!("Uninstalled {} hook", hook_type.filename());
            } else {
                tracing::warn!(
                    "{} hook exists but is not a HeliosDB hook, skipping",
                    hook_type.filename()
                );
            }
        }

        Ok(())
    }

    /// Uninstall all hooks
    pub fn uninstall_all(&self) -> Result<()> {
        for hook_type in HookType::all() {
            self.uninstall(*hook_type)?;
        }
        Ok(())
    }

    /// Get status of all hooks
    pub fn status(&self) -> Vec<HookStatus> {
        HookType::all()
            .iter()
            .map(|&hook_type| {
                let path = self.hook_path(hook_type);
                let installed = path.exists();
                let is_helios_hook = if installed {
                    fs::read_to_string(&path)
                        .map(|c| c.contains("HeliosDB-Nano Git Hook"))
                        .unwrap_or(false)
                } else {
                    false
                };

                HookStatus {
                    hook_type,
                    installed,
                    path,
                    is_helios_hook,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_hook_type_filename() {
        assert_eq!(HookType::PostCheckout.filename(), "post-checkout");
        assert_eq!(HookType::PreCommit.filename(), "pre-commit");
        assert_eq!(HookType::PostMerge.filename(), "post-merge");
    }

    #[test]
    fn test_generate_post_checkout() {
        let config = HookConfig {
            database: "/path/to/db".to_string(),
            verbose: true,
            ..Default::default()
        };

        let manager = HookManager::new(PathBuf::from("/tmp"), config);
        let script = manager.generate(HookType::PostCheckout);

        assert!(script.contains("HeliosDB-Nano Git Hook"));
        assert!(script.contains("post-checkout"));
        assert!(script.contains("helios git sync"));
    }

    #[test]
    fn test_hook_manager_creation() {
        let config = HookConfig::default();
        let manager = HookManager::new(PathBuf::from("/tmp/repo"), config);
        assert_eq!(manager.hooks_dir(), PathBuf::from("/tmp/repo/.git/hooks"));
    }
}
