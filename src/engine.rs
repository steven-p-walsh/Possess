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
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Engine {
    pub config: Config,
    adapters: Vec<Box<dyn HarnessAdapter>>,
    target_cache: Mutex<HashMap<Harness, Vec<TargetOption>>>,
}

pub struct Transfer {
    pub package: HandoffPackage,
    pub prepared: PreparedLaunch,
}

impl Engine {
    pub fn new(config: Config) -> Self {
        let adapters = adapters(&config);
        Self {
            config,
            adapters,
            target_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn scan(&self) -> Vec<SessionSummary> {
        let mut sessions = Vec::new();
        for adapter in &self.adapters {
            if let Ok(mut found) = adapter.list_sessions() {
                sessions.append(&mut found);
            }
        }
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
        let first = matches
            .next()
            .with_context(|| format!("session not found: {id}"))?;
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
        Ok(Transfer { package, prepared })
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
