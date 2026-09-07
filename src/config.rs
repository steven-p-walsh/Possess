//! Configuration and private on-disk location policy.
//!
//! Possess uses its own home instead of the project because handoffs can contain secrets
//! and must never become accidental repository changes.

use crate::domain::Harness;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub data_dir: PathBuf,
    pub context_tokens: usize,
    pub reduced_motion: bool,
    pub ascii_only: bool,
    pub binaries: HashMap<String, PathBuf>,
    pub homes: HashMap<String, PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let data_dir = std::env::var_os("POSSESS_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share/possess"));
        Self {
            data_dir,
            context_tokens: 16_000,
            reduced_motion: false,
            ascii_only: false,
            binaries: HashMap::new(),
            homes: HashMap::new(),
        }
    }
}

impl Config {
    /// Environment overrides keep tests isolated and support harness installations whose
    /// state is intentionally outside the user's default home.
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config: Self = toml::from_str(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if let Some(value) = std::env::var_os("POSSESS_HOME") {
            config.data_dir = value.into();
        }
        Ok(config)
    }

    pub fn binary(&self, harness: Harness) -> PathBuf {
        self.binaries
            .get(harness.command())
            .cloned()
            .unwrap_or_else(|| PathBuf::from(harness.command()))
    }

    pub fn harness_home(&self, harness: Harness) -> PathBuf {
        if let Some(path) = self.homes.get(harness.command()) {
            return path.clone();
        }
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        match harness {
            Harness::Codex => std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            Harness::Claude => std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
            Harness::OpenCode => std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("opencode"),
            Harness::Grok => std::env::var_os("GROK_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".grok")),
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for path in [
            self.data_dir.as_path(),
            &self.data_dir.join("handoffs"),
            &self.data_dir.join("blobs"),
        ] {
            std::fs::create_dir_all(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
            set_private_dir(path)?;
        }
        Ok(())
    }
}

pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("possess/config.toml");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/possess/config.toml")
}

#[cfg(unix)]
pub fn set_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn set_private_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn set_private_file(_path: &Path) -> Result<()> {
    Ok(())
}
