//! Read-only Git capture for handoff context.
//!
//! Diffs are archived because uncommitted work often explains the current state, while
//! file contents remain owned by the existing workspace.

use crate::domain::GitSnapshot;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn capture(cwd: &Path) -> Result<Option<GitSnapshot>> {
    if cwd.as_os_str().is_empty() || !cwd.exists() {
        return Ok(None);
    }
    let Some(root) = git(cwd, &["rev-parse", "--show-toplevel"])
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let repo_root = PathBuf::from(root.trim());
    let status = git(cwd, &["status", "--porcelain=v2", "--branch"])?;
    let branch = git(cwd, &["branch", "--show-current"])
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let head = git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let origin = git(cwd, &["remote", "get-url", "origin"])
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let staged_diff = git(cwd, &["diff", "--cached", "--no-ext-diff", "--no-color"])?;
    let unstaged_diff = git(cwd, &["diff", "--no-ext-diff", "--no-color"])?;
    let untracked = git(cwd, &["ls-files", "--others", "--exclude-standard"])?
        .lines()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .collect();
    Ok(Some(GitSnapshot {
        repo_root,
        branch,
        head,
        origin,
        worktree: cwd.to_path_buf(),
        status,
        staged_diff,
        unstaged_diff,
        untracked,
    }))
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
