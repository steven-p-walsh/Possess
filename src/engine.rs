//! Application service coordinating readers, packaging, and destination preparation.
//!
//! Keeping orchestration independent of Ratatui lets the CLI and future clients exercise
//! the same transfer path as the interactive application.

use crate::adapters::{HarnessAdapter, adapters};
use crate::config::Config;
use crate::domain::{
    AdapterStatus, Fidelity, Harness, LaunchRequest, PortableSessionV1, SessionSummary,
    TargetOption,
};
use crate::git_state;
use crate::handoff::{self, HandoffPackage};
use crate::launcher::{self, PreparedLaunch};
use crate::project::SessionScope;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Engine {
    pub config: Config,
    pub scope: SessionScope,
    adapters: Vec<Box<dyn HarnessAdapter>>,
    target_cache: Mutex<HashMap<Harness, Vec<TargetOption>>>,
}

pub struct Transfer {
    pub package: Option<HandoffPackage>,
    pub prepared: PreparedLaunch,
}

impl Engine {
    pub fn new(config: Config, scope: SessionScope) -> Self {
        let adapters = adapters(&config);
        Self {
            config,
            scope,
            adapters,
            target_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn scan(&self) -> Vec<SessionSummary> {
        // A refresh should pick up model and agent changes made in the harness we just
        // returned from. Redraws between scans still use the cached choices.
        self.target_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let mut sessions = Vec::new();
        for adapter in &self.adapters {
            if let Ok(mut found) = adapter.list_sessions() {
                sessions.append(&mut found);
            }
        }
        self.scope.retain(&mut sessions);
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at.or(s.created_at)));
        sessions
    }

    pub fn statuses(&self) -> Vec<AdapterStatus> {
        self.adapters.iter().map(|a| a.probe()).collect()
    }

    pub fn targets(&self, harness: Harness) -> Vec<TargetOption> {
        let mut cache = self
            .target_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(targets) = cache.get(&harness) {
            return targets.clone();
        }
        // Ratatui redraws continuously. Caching prevents a redraw from becoming a new
        // database query or harness process while model choices cannot change mid-wizard.
        let targets = self
            .adapter(harness)
            .and_then(|a| a.discover_targets().ok())
            .unwrap_or_default();
        cache.insert(harness, targets.clone());
        targets
    }

    pub fn find(&self, id: &str) -> Result<SessionSummary> {
        let exact = id.contains(':');
        let mut matches = self.scan().into_iter().filter(|s| {
            if exact {
                s.qualified_id == id
            } else {
                s.vendor_id == id || s.vendor_id.starts_with(id)
            }
        });
        let first = matches.next().with_context(|| match self.scope.root() {
            Some(root) => format!(
                "session not found in {}: {id}; use --all-projects to search other projects",
                root.display()
            ),
            None => format!("session not found: {id}"),
        })?;
        if matches.next().is_some() {
            anyhow::bail!("session id is ambiguous; use its harness-qualified id");
        }
        Ok(first)
    }

    pub fn load(&self, summary: &SessionSummary) -> Result<PortableSessionV1> {
        let mut session = self
            .adapter(summary.harness)
            .context("adapter unavailable")?
            .load_session(summary)?;
        session.git = git_state::capture(&summary.cwd).unwrap_or(None);
        Ok(session)
    }

    pub fn transfer(&self, summary: &SessionSummary, request: &LaunchRequest) -> Result<Transfer> {
        // The source harness already owns this session. Resuming must work even when
        // its history is unreadable to Possess or the handoff directory is unwritable.
        if summary.harness == request.destination {
            return Ok(Transfer {
                package: None,
                prepared: launcher::native_resume(&self.config, summary, Some(request)),
            });
        }
        let session = self.load(summary)?;
        let fidelity =
            launcher::preferred_fidelity(&self.config, summary.harness, request.destination);
        let package = handoff::create(
            &self.config,
            &session,
            request.destination,
            fidelity,
            request.context_tokens,
        )?;
        let prepared = launcher::prepare(&self.config, &session, &package, request)?;
        Ok(Transfer {
            package: Some(package),
            prepared,
        })
    }

    pub fn fidelity(&self, source: Harness, destination: Harness) -> Fidelity {
        launcher::preferred_fidelity(&self.config, source, destination)
    }

    fn adapter(&self, harness: Harness) -> Option<&dyn HarnessAdapter> {
        self.adapters
            .iter()
            .find(|a| a.harness() == harness)
            .map(|a| a.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_harness_resumes_without_loading_history_or_creating_a_package() {
        let temporary = tempfile::tempdir().unwrap();
        let data_dir = temporary.path().join("blocked");
        std::fs::write(&data_dir, "not a directory").unwrap();
        let config = Config {
            data_dir: data_dir.clone(),
            ..Config::default()
        };
        // No adapters means any attempt to load or normalize history fails this test.
        let engine = Engine {
            config,
            scope: SessionScope::AllProjects,
            adapters: Vec::new(),
            target_cache: Mutex::new(HashMap::new()),
        };
        for harness in Harness::ALL {
            let mut summary = SessionSummary::new(
                harness,
                "existing-session".into(),
                temporary.path().join("missing-history"),
            );
            summary.cwd = temporary.path().into();
            let request = LaunchRequest {
                destination: harness,
                launch: false,
                ..LaunchRequest::default()
            };
            let transfer = engine.transfer(&summary, &request).unwrap();
            assert!(transfer.package.is_none());
            assert_eq!(transfer.prepared.fidelity, Fidelity::NativeResume);
            assert_eq!(
                transfer.prepared.destination_session_id.as_deref(),
                Some("existing-session")
            );
            assert!(
                transfer
                    .prepared
                    .args
                    .iter()
                    .any(|arg| arg == "existing-session")
            );
            assert!(
                !transfer
                    .prepared
                    .args
                    .iter()
                    .any(|arg| arg.contains("fork"))
            );
        }
        assert_eq!(
            std::fs::read_to_string(data_dir).unwrap(),
            "not a directory"
        );
    }
}
