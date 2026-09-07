//! Launch-directory scope shared by the picker and CLI.
//!
//! Harnesses record working directories at different depths and sometimes through
//! symlinks. Comparing their raw strings would split one project into several lists.

use crate::domain::SessionSummary;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum SessionScope {
    AllProjects,
    Project(PathBuf),
}

impl SessionScope {
    pub fn from_launch_directory(all_projects: bool) -> Result<Self> {
        if all_projects {
            return Ok(Self::AllProjects);
        }
        let cwd = std::env::current_dir().context("cannot determine the launch directory")?;
        Self::for_directory(&cwd)
    }

    fn for_directory(directory: &Path) -> Result<Self> {
        let directory = directory
            .canonicalize()
            .with_context(|| format!("cannot resolve project directory {}", directory.display()))?;
        Ok(Self::Project(
            repository_root(&directory).unwrap_or(directory),
        ))
    }

    pub fn root(&self) -> Option<&Path> {
        match self {
            Self::AllProjects => None,
            Self::Project(root) => Some(root),
        }
    }

    pub fn retain(&self, sessions: &mut Vec<SessionSummary>) {
        let Some(root) = self.root() else {
            return;
        };
        // Many sessions share a directory. Resolve it once per scan, while allowing a
        // refresh to notice repositories or worktrees created since the previous scan.
        let mut matches = HashMap::new();
        sessions.retain(|session| {
            *matches.entry(session.cwd.clone()).or_insert_with(|| {
                let Some(cwd) = normalized_directory(&session.cwd) else {
                    return false;
                };
                // Path components keep /app from matching /app-old. A nested repository
                // or worktree has its own files and must not inherit its parent's scope.
                cwd.starts_with(root)
                    && repository_root(&cwd).is_none_or(|session_root| session_root == root)
            })
        });
    }
}

fn normalized_directory(path: &Path) -> Option<PathBuf> {
    // Missing or relative vendor paths cannot be attributed to the launch project.
    if !path.is_absolute() {
        return None;
    }
    for ancestor in path.ancestors() {
        if let Ok(base) = ancestor.canonicalize() {
            // An old session may refer to a deleted subdirectory. Resolve any surviving
            // symlinked ancestors before adding the missing part back to the path.
            let resolved = base.join(path.strip_prefix(ancestor).ok()?);
            let mut normalized = PathBuf::new();
            for component in resolved.components() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        normalized.pop();
                    }
                    _ => normalized.push(component.as_os_str()),
                }
            }
            return Some(normalized);
        }
    }
    None
}

pub(crate) fn repository_root(directory: &Path) -> Option<PathBuf> {
    let existing = directory.ancestors().find(|path| path.is_dir())?;
    let output = Command::new("git")
        .arg("-C")
        .arg(existing)
        .args(["rev-parse", "--show-toplevel"])
        // Git hooks can export these for a different repository; session paths must
        // always be resolved against their own working trees.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    normalized_directory(Path::new(root.trim_end_matches(['\r', '\n'])))
}

#[cfg(test)]
mod tests {
    use super::SessionScope;
    use crate::domain::{Harness, SessionSummary};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let output = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn session(harness: Harness, id: &str, cwd: &Path) -> SessionSummary {
        let mut session = SessionSummary::new(harness, id.into(), PathBuf::new());
        session.cwd = cwd.into();
        session
    }

    #[test]
    fn launching_in_a_subdirectory_finds_the_project_across_harnesses() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("app");
        let sibling = temporary.path().join("app-old");
        init_repo(&root);
        init_repo(&sibling);
        let source = root.join("src");
        std::fs::create_dir(&source).unwrap();
        let scope = SessionScope::for_directory(&source).unwrap();
        assert_eq!(scope.root(), Some(root.canonicalize().unwrap().as_path()));

        let mut sessions: Vec<_> = Harness::ALL
            .into_iter()
            .map(|harness| session(harness, harness.command(), &source))
            .collect();
        sessions.push(session(Harness::Codex, "root", &root));
        sessions.push(session(Harness::Codex, "deleted", &root.join("old/src")));
        sessions.push(session(Harness::Codex, "other", &sibling));
        sessions.push(session(Harness::Codex, "unknown", Path::new("")));
        sessions.push(session(Harness::Codex, "relative", Path::new("src")));
        scope.retain(&mut sessions);
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.vendor_id.as_str())
                .collect::<Vec<_>>(),
            ["codex", "claude", "opencode", "grok", "root", "deleted"]
        );
    }

    #[test]
    fn non_git_scope_includes_subdirectories_but_excludes_nested_repositories() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("notes");
        let child = directory.join("drafts");
        let nested = directory.join("separate-repo");
        std::fs::create_dir_all(&child).unwrap();
        init_repo(&nested);
        let scope = SessionScope::for_directory(&directory).unwrap();
        let mut sessions = vec![
            session(Harness::Codex, "notes", &directory),
            session(Harness::Claude, "draft", &child),
            session(Harness::Grok, "nested", &nested),
        ];
        scope.retain(&mut sessions);
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.vendor_id.as_str())
                .collect::<Vec<_>>(),
            ["notes", "draft"]
        );
    }

    #[test]
    fn linked_worktrees_stay_separate_even_inside_the_parent_repository() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("app");
        init_repo(&root);
        let committed = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "-c",
                "user.name=Possess Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture",
            ])
            .output()
            .unwrap();
        assert!(committed.status.success());
        let worktree = root.join("linked");
        let added = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "--quiet",
                "--detach",
            ])
            .arg(&worktree)
            .output()
            .unwrap();
        assert!(added.status.success());
        let source = worktree.join("src");
        std::fs::create_dir(&source).unwrap();
        let sessions = vec![
            session(Harness::Codex, "main", &root),
            session(Harness::Claude, "linked", &source),
        ];
        let mut main_sessions = sessions.clone();
        SessionScope::for_directory(&root)
            .unwrap()
            .retain(&mut main_sessions);
        assert_eq!(main_sessions.len(), 1);
        assert_eq!(main_sessions[0].vendor_id, "main");
        let mut linked_sessions = sessions;
        SessionScope::for_directory(&source)
            .unwrap()
            .retain(&mut linked_sessions);
        assert_eq!(linked_sessions.len(), 1);
        assert_eq!(linked_sessions[0].vendor_id, "linked");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_project_paths_match_without_including_sibling_prefixes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("app");
        init_repo(&root);
        let alias = temporary.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut sessions = vec![
            session(Harness::Codex, "real", &root),
            session(Harness::Claude, "alias", &alias),
            session(Harness::Grok, "deleted", &alias.join("old/src")),
            session(Harness::Codex, "escape", &alias.join("../app-old")),
        ];
        SessionScope::for_directory(&alias)
            .unwrap()
            .retain(&mut sessions);
        assert_eq!(
            sessions
                .iter()
                .map(|s| s.vendor_id.as_str())
                .collect::<Vec<_>>(),
            ["real", "alias", "deleted"]
        );
    }

    #[test]
    fn all_projects_keeps_sessions_with_unknown_directories() {
        let mut sessions = vec![
            session(Harness::Codex, "unknown", Path::new("")),
            session(Harness::Grok, "relative", Path::new("src")),
        ];
        SessionScope::AllProjects.retain(&mut sessions);
        assert_eq!(sessions.len(), 2);
    }
}
