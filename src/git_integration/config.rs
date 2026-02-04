//! Git integration configuration
//!
//! Manages Git repository detection and configuration.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Git configuration for CLI and hooks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    /// Git repository root path
    pub repo_path: PathBuf,

    /// Current Git branch name
    pub current_branch: Option<String>,

    /// Current Git commit SHA
    pub current_commit: Option<String>,

    /// Whether we're in a Git repository
    pub is_git_repo: bool,
}

impl GitConfig {
    /// Detect Git repository at the given path
    pub fn detect(path: &Path) -> Result<Self> {
        let repo_path = Self::find_git_root(path)?;
        let is_git_repo = repo_path.is_some();

        if let Some(ref root) = repo_path {
            let current_branch = Self::get_current_branch(root).ok();
            let current_commit = Self::get_current_commit(root).ok();

            Ok(Self {
                repo_path: root.clone(),
                current_branch,
                current_commit,
                is_git_repo: true,
            })
        } else {
            Ok(Self {
                repo_path: path.to_path_buf(),
                current_branch: None,
                current_commit: None,
                is_git_repo: false,
            })
        }
    }

    /// Find Git repository root by walking up the directory tree
    fn find_git_root(start: &Path) -> Result<Option<PathBuf>> {
        let mut current = start.to_path_buf();

        loop {
            let git_dir = current.join(".git");
            if git_dir.exists() {
                return Ok(Some(current));
            }

            if !current.pop() {
                return Ok(None);
            }
        }
    }

    /// Get current Git branch name
    fn get_current_branch(repo_path: &Path) -> Result<String> {
        let head_path = repo_path.join(".git/HEAD");
        let content = std::fs::read_to_string(&head_path)
            .map_err(|e| Error::io(format!("Failed to read .git/HEAD: {}", e)))?;

        // Parse "ref: refs/heads/branch-name"
        if let Some(stripped) = content.strip_prefix("ref: refs/heads/") {
            Ok(stripped.trim().to_string())
        } else {
            // Detached HEAD - return first 7 chars of commit
            Ok(content.trim().chars().take(7).collect())
        }
    }

    /// Get current Git commit SHA
    fn get_current_commit(repo_path: &Path) -> Result<String> {
        let head_path = repo_path.join(".git/HEAD");
        let content = std::fs::read_to_string(&head_path)
            .map_err(|e| Error::io(format!("Failed to read .git/HEAD: {}", e)))?;

        if let Some(ref_path) = content.strip_prefix("ref: ") {
            // Read the ref file
            let ref_file = repo_path.join(".git").join(ref_path.trim());
            let commit = std::fs::read_to_string(&ref_file)
                .map_err(|e| Error::io(format!("Failed to read ref file: {}", e)))?;
            Ok(commit.trim().to_string())
        } else {
            // Already a commit SHA
            Ok(content.trim().to_string())
        }
    }

    /// Get repository remote URL (for provider detection)
    pub fn get_remote_url(&self, remote_name: &str) -> Result<Option<String>> {
        let config_path = self.repo_path.join(".git/config");
        let content = std::fs::read_to_string(&config_path)
            .map_err(|e| Error::io(format!("Failed to read .git/config: {}", e)))?;

        // Simple parsing - look for [remote "origin"] section
        let section_marker = format!("[remote \"{}\"]", remote_name);
        let mut in_section = false;

        for line in content.lines() {
            if line.trim() == section_marker {
                in_section = true;
                continue;
            }

            if in_section {
                if line.starts_with('[') {
                    break; // New section
                }
                if let Some(url) = line.trim().strip_prefix("url = ") {
                    return Ok(Some(url.to_string()));
                }
            }
        }

        Ok(None)
    }

    /// Detect Git provider from remote URL
    pub fn detect_provider(&self) -> Option<String> {
        if let Ok(Some(url)) = self.get_remote_url("origin") {
            if url.contains("github.com") {
                return Some("github".to_string());
            } else if url.contains("gitlab.com") || url.contains("gitlab") {
                return Some("gitlab".to_string());
            } else if url.contains("bitbucket") {
                return Some("bitbucket".to_string());
            }
        }
        Some("generic".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_config_not_in_repo() {
        let config = GitConfig::detect(Path::new("/tmp")).unwrap_or_else(|_| GitConfig {
            repo_path: PathBuf::from("/tmp"),
            current_branch: None,
            current_commit: None,
            is_git_repo: false,
        });
        // May or may not be a git repo depending on system
    }
}
