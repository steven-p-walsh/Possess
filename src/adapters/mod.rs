//! Read adapters for vendor-owned session stores.
//!
//! Reading and writing are separated deliberately. Readers tolerate additive schema
//! changes; writers require an explicit compatibility decision.

mod claude;
mod codex;
mod grok;
mod opencode;

use crate::config::Config;
use crate::domain::{AdapterStatus, Harness, PortableSessionV1, SessionSummary, TargetOption};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use grok::GrokAdapter;
pub use opencode::OpenCodeAdapter;

pub trait HarnessAdapter: Send + Sync {
    fn harness(&self) -> Harness;
    fn probe(&self) -> AdapterStatus;
    fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    fn load_session(&self, summary: &SessionSummary) -> Result<PortableSessionV1>;
    fn discover_targets(&self) -> Result<Vec<TargetOption>>;
}

pub fn adapters(config: &Config) -> Vec<Box<dyn HarnessAdapter>> {
    vec![
        Box::new(CodexAdapter::new(config)),
        Box::new(ClaudeAdapter::new(config)),
        Box::new(OpenCodeAdapter::new(config)),
        Box::new(GrokAdapter::new(config)),
    ]
}

pub(crate) fn command_version(binary: &Path) -> Option<String> {
    let output = Command::new(binary).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn binary_exists(binary: &Path) -> bool {
    if binary.components().count() > 1 {
        return binary.is_file();
    }
    Command::new("sh")
        .args(["-c", "command -v \"$1\" >/dev/null 2>&1", "possess"])
        .arg(binary)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|v| v.with_timezone(&Utc)),
        Value::Number(n) => {
            let raw = n.as_i64()?;
            let (seconds, nanos) = if raw > 10_000_000_000 {
                (raw / 1000, ((raw % 1000) * 1_000_000) as u32)
            } else {
                (raw, 0)
            };
            Utc.timestamp_opt(seconds, nanos).single()
        }
        _ => None,
    }
}

pub(crate) fn for_each_stable_jsonl(
    path: &Path,
    mut visit: impl FnMut(Value) -> Result<()>,
) -> Result<(u64, bool)> {
    let before = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut consumed = 0u64;
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        // An active harness can leave its final JSON value half-written. A complete-line
        // boundary gives us a coherent snapshot without stopping that process.
        if !line.ends_with('\n') {
            break;
        }
        consumed += bytes as u64;
        if let Ok(value) = serde_json::from_str(line.trim_end()) {
            visit(value)?;
        }
    }
    let after = std::fs::metadata(path)?.len();
    Ok((consumed, after != before || consumed < after))
}

pub(crate) fn extract_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                if matches!(kind, "text" | "input_text" | "output_text") {
                    item.get("text").and_then(Value::as_str).map(str::to_owned)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| object.get("content").map(extract_text))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub(crate) fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

pub(crate) fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    stem.rsplit('-').next().map(str::to_owned)
}

pub(crate) fn path_from_command(config: &Config, harness: Harness) -> PathBuf {
    config.binary(harness)
}

pub(crate) fn markdown_targets(roots: &[PathBuf]) -> Vec<TargetOption> {
    let mut targets = BTreeMap::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("md") {
                continue;
            }
            let id = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if !id.is_empty() {
                // A project definition shadows a user definition in most harnesses. The
                // picker only needs one stable choice because the harness resolves scope.
                targets.insert(
                    id.clone(),
                    TargetOption {
                        id: id.clone(),
                        label: id,
                        kind: crate::domain::TargetKind::Agent,
                    },
                );
            }
        }
    }
    targets.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::for_each_stable_jsonl;
    use std::io::Write;

    #[test]
    fn active_jsonl_ends_at_the_last_complete_record() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{{\"id\":1}}\n{{\"id\":2").unwrap();

        let mut records = Vec::new();
        let (consumed, partial) = for_each_stable_jsonl(file.path(), |record| {
            records.push(record);
            Ok(())
        })
        .unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["id"], 1);
        assert_eq!(consumed, 9);
        assert!(partial);
    }
}
