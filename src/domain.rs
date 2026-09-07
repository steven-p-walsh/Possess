//! Stable data shared by adapters, packaging, launchers, and user interfaces.
//!
//! Vendor records are normalized before transfer. Depending directly on one vendor's
//! vocabulary would make every other adapter inherit its blind spots.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Codex,
    Claude,
    OpenCode,
    Grok,
}

impl Harness {
    pub const ALL: [Self; 4] = [Self::Codex, Self::Claude, Self::OpenCode, Self::Grok];

    pub fn command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Grok => "grok",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Codex => "◉",
            Self::Claude => "✦",
            Self::OpenCode => "◇",
            Self::Grok => "✕",
        }
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::OpenCode => "OpenCode",
            Self::Grok => "Grok",
        })
    }
}

impl std::str::FromStr for Harness {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "codex" => Ok(Self::Codex),
            "claude" | "claudecode" => Ok(Self::Claude),
            "opencode" | "open" => Ok(Self::OpenCode),
            "grok" | "grokbuild" => Ok(Self::Grok),
            _ => anyhow::bail!("unknown harness: {value}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub qualified_id: String,
    pub vendor_id: String,
    pub harness: Harness,
    pub title: String,
    pub cwd: PathBuf,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub version: Option<String>,
    pub preview: String,
    pub turn_count: Option<u64>,
    pub token_count: Option<u64>,
    pub source_path: PathBuf,
    pub active: bool,
    pub archived: bool,
}

impl SessionSummary {
    pub fn new(harness: Harness, vendor_id: String, source_path: PathBuf) -> Self {
        Self {
            qualified_id: format!("{}:{vendor_id}", harness.command()),
            vendor_id,
            harness,
            title: "Untitled session".into(),
            cwd: PathBuf::new(),
            created_at: None,
            updated_at: None,
            model: None,
            agent: None,
            version: None,
            preview: String::new(),
            turn_count: None,
            token_count: None,
            source_path,
            active: false,
            archived: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortableSessionV1 {
    pub schema: String,
    pub source: SessionSummary,
    pub captured_at: DateTime<Utc>,
    pub partial: bool,
    pub captured_bytes: Option<u64>,
    pub events: Vec<PortableEvent>,
    pub todos: Vec<TodoItem>,
    pub attachments: Vec<Attachment>,
    pub git: Option<GitSnapshot>,
    pub skipped: Vec<SkippedRecord>,
}

impl PortableSessionV1 {
    /// Uses an explicit schema name so packages kept for years remain readable after the
    /// in-memory model evolves.
    pub fn new(source: SessionSummary) -> Self {
        Self {
            schema: "possess.portable-session.v1".into(),
            source,
            captured_at: Utc::now(),
            partial: false,
            captured_bytes: None,
            events: Vec::new(),
            todos: Vec::new(),
            attachments: Vec::new(),
            git: None,
            skipped: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortableEvent {
    Message {
        id: Option<String>,
        parent_id: Option<String>,
        role: Role,
        text: String,
        timestamp: Option<DateTime<Utc>>,
    },
    ToolCall {
        id: String,
        name: String,
        operation: ToolOperation,
        input: serde_json::Value,
        timestamp: Option<DateTime<Utc>>,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
        timestamp: Option<DateTime<Utc>>,
    },
    Summary {
        text: String,
        timestamp: Option<DateTime<Utc>>,
    },
    Error {
        text: String,
        timestamp: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolOperation {
    FileRead,
    FileWrite,
    FileEdit,
    FileGlob,
    ContentSearch,
    ShellExec,
    WebSearch,
    AgentSpawn,
    Other,
}

impl ToolOperation {
    /// Maps conservatively: an unknown tool stays representable instead of acquiring
    /// stronger replay semantics by mistake.
    pub fn infer(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        if name.contains("read") || name == "cat" {
            Self::FileRead
        } else if name.contains("apply_patch")
            || name.contains("applypatch")
            || name.contains("edit")
        {
            Self::FileEdit
        } else if name.contains("write") {
            Self::FileWrite
        } else if name.contains("glob") || name.contains("find_file") {
            Self::FileGlob
        } else if name.contains("grep") || name.contains("search") && !name.contains("web") {
            Self::ContentSearch
        } else if name.contains("shell") || name.contains("bash") || name.contains("exec") {
            Self::ShellExec
        } else if name.contains("web") || name.contains("fetch") {
            Self::WebSearch
        } else if name.contains("agent") || name.contains("task") {
            Self::AgentSpawn
        } else {
            Self::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ToolOperation;

    #[test]
    fn unknown_tools_do_not_gain_replay_semantics() {
        assert_eq!(ToolOperation::infer("ApplyPatch"), ToolOperation::FileEdit);
        assert_eq!(ToolOperation::infer("vendor_magic"), ToolOperation::Other);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub status: String,
    pub priority: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub path: PathBuf,
    pub media_type: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSnapshot {
    pub repo_root: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub origin: Option<String>,
    pub worktree: PathBuf,
    pub status: String,
    pub staged_diff: String,
    pub unstaged_diff: String,
    pub untracked: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedRecord {
    pub record_type: String,
    pub reason: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetOption {
    pub id: String,
    pub label: String,
    pub kind: TargetKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Model,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterStatus {
    pub harness: Harness,
    pub binary: Option<PathBuf>,
    pub version: Option<String>,
    pub store: PathBuf,
    pub store_exists: bool,
    pub supported_import: bool,
    pub native_writer: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    NativeResume,
    // Old handoff manifests still describe forks, even though new same-harness launches resume.
    NativeFork,
    SupportedImport,
    CompatibleWriter,
    SmartBootstrap,
}

impl fmt::Display for Fidelity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NativeResume => "resume original",
            Self::NativeFork => "native fork",
            Self::SupportedImport => "supported import",
            Self::CompatibleWriter => "compatible writer",
            Self::SmartBootstrap => "smart bootstrap",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub destination: Harness,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub context_tokens: usize,
    pub launch: bool,
}

impl Default for LaunchRequest {
    fn default() -> Self {
        Self {
            destination: Harness::Codex,
            model: None,
            agent: None,
            context_tokens: 16_000,
            launch: true,
        }
    }
}
