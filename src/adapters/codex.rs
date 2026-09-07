//! Codex discovery and rollout normalization.
//!
//! The state database is only an index because it is cheap and authoritative for titles.
//! The rollout remains the source for portable conversational content.

use super::*;
use crate::domain::{PortableEvent, Role, TargetKind, ToolOperation};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub struct CodexAdapter {
    home: PathBuf,
    binary: PathBuf,
}

impl CodexAdapter {
    pub fn new(config: &Config) -> Self {
        Self {
            home: config.harness_home(Harness::Codex),
            binary: path_from_command(config, Harness::Codex),
        }
    }

    fn list_from_index(&self) -> Result<Vec<SessionSummary>> {
        let db = self.home.join("state_5.sqlite");
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut statement = conn.prepare(
            "SELECT id, rollout_path, created_at_ms, updated_at_ms, cwd, title, preview, model, agent_nickname, cli_version, tokens_used, archived FROM threads WHERE preview <> '' ORDER BY recency_at_ms DESC"
        )?;
        let rows = statement.query_map([], |row| {
            let id: String = row.get(0)?;
            let path: String = row.get(1)?;
            let mut item = SessionSummary::new(Harness::Codex, id, PathBuf::from(path));
            let created: Option<i64> = row.get(2)?;
            let updated: Option<i64> = row.get(3)?;
            item.created_at = created.and_then(|v| Utc.timestamp_millis_opt(v).single());
            item.updated_at = updated.and_then(|v| Utc.timestamp_millis_opt(v).single());
            item.cwd = PathBuf::from(row.get::<_, String>(4)?);
            item.title = row.get::<_, String>(5).unwrap_or_default();
            item.preview = row.get::<_, String>(6).unwrap_or_default();
            item.model = row.get(7).ok();
            item.agent = row.get(8).ok();
            item.version = row.get(9).ok();
            item.token_count = row.get::<_, i64>(10).ok().map(|v| v.max(0) as u64);
            item.archived = row.get::<_, i64>(11).unwrap_or(0) != 0;
            if item.title.is_empty() {
                item.title = first_line(&item.preview);
            }
            Ok(item)
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    fn list_from_files(&self) -> Result<Vec<SessionSummary>> {
        let root = self.home.join("sessions");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !entry.file_type().is_file()
                || path.extension().and_then(|v| v.to_str()) != Some("jsonl")
            {
                continue;
            }
            let Some(id) = rollout_id(path) else { continue };
            let mut item = SessionSummary::new(Harness::Codex, id, path.to_path_buf());
            item.updated_at = file_mtime(path);
            if let Ok((records, _, _)) = stable_jsonl_head(path, 24) {
                for record in records {
                    if record.get("type").and_then(Value::as_str) == Some("session_meta") {
                        let payload = &record["payload"];
                        item.cwd = payload
                            .get("cwd")
                            .and_then(Value::as_str)
                            .map(PathBuf::from)
                            .unwrap_or_default();
                        item.created_at = payload.get("timestamp").and_then(parse_time);
                        item.version = payload
                            .get("cli_version")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        break;
                    }
                }
            }
            item.title = format!("Codex {}", short_id(&item.vendor_id));
            sessions.push(item);
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }
}

impl HarnessAdapter for CodexAdapter {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn probe(&self) -> AdapterStatus {
        let version = command_version(&self.binary);
        AdapterStatus {
            harness: Harness::Codex,
            binary: binary_exists(&self.binary).then(|| self.binary.clone()),
            version: version.clone(),
            store: self.home.clone(),
            store_exists: self.home.exists(),
            supported_import: false,
            native_writer: version.as_deref().is_some_and(|v| v.contains("0.153.4")),
            detail: "app-server discovery; rollout fallback; bootstrap for cross-harness imports"
                .into(),
        }
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        // Older Codex installations can have rollouts without the current state index.
        // Falling back preserves discoverability without making the normal path expensive.
        self.list_from_index().or_else(|_| self.list_from_files())
    }

    fn load_session(&self, summary: &SessionSummary) -> Result<PortableSessionV1> {
        let mut session = PortableSessionV1::new(summary.clone());
        let mut events = Vec::new();
        let mut has_response_user = false;
        let mut has_response_assistant = false;
        let (bytes, partial) = for_each_stable_jsonl(&summary.source_path, |record| {
            let top = record.get("type").and_then(Value::as_str).unwrap_or("");
            let payload = &record["payload"];
            match (
                top,
                payload.get("type").and_then(Value::as_str).unwrap_or(""),
            ) {
                ("response_item", "message") => {
                    let role = match payload.get("role").and_then(Value::as_str) {
                        Some("user") => Role::User,
                        Some("assistant") => Role::Assistant,
                        _ => return Ok(()),
                    };
                    let text = extract_text(&payload["content"]);
                    if !text.trim().is_empty() && !is_codex_injected(&text) {
                        has_response_user |= role == Role::User;
                        has_response_assistant |= role == Role::Assistant;
                        events.push((
                            true,
                            PortableEvent::Message {
                                id: payload.get("id").and_then(Value::as_str).map(str::to_owned),
                                parent_id: None,
                                role,
                                text,
                                timestamp: record.get("timestamp").and_then(parse_time),
                            },
                        ));
                    }
                }
                ("response_item", "function_call" | "custom_tool_call") => {
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let id = payload
                        .get("call_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    let input = payload.get("arguments").cloned().unwrap_or(Value::Null);
                    events.push((
                        true,
                        PortableEvent::ToolCall {
                            id,
                            operation: ToolOperation::infer(&name),
                            name,
                            input,
                            timestamp: record.get("timestamp").and_then(parse_time),
                        },
                    ));
                }
                ("response_item", "function_call_output" | "custom_tool_call_output") => {
                    let call_id = payload
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    let output = extract_text(payload.get("output").unwrap_or(&Value::Null));
                    events.push((
                        true,
                        PortableEvent::ToolResult {
                            call_id,
                            output,
                            is_error: false,
                            timestamp: record.get("timestamp").and_then(parse_time),
                        },
                    ));
                }
                ("event_msg", "user_message") => {
                    if let Some(text) = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| payload.get("text").and_then(Value::as_str))
                        .filter(|text| !is_codex_injected(text))
                    {
                        events.push((
                            false,
                            PortableEvent::Message {
                                id: None,
                                parent_id: None,
                                role: Role::User,
                                text: text.to_owned(),
                                timestamp: record.get("timestamp").and_then(parse_time),
                            },
                        ));
                    }
                }
                ("event_msg", "agent_message") => {
                    if let Some(text) = payload.get("message").and_then(Value::as_str) {
                        events.push((
                            false,
                            PortableEvent::Message {
                                id: None,
                                parent_id: None,
                                role: Role::Assistant,
                                text: text.to_owned(),
                                timestamp: record.get("timestamp").and_then(parse_time),
                            },
                        ));
                    }
                }
                ("event_msg", "context_compacted") | ("response_item", "compaction") => {
                    let text = payload
                        .get("summary")
                        .or_else(|| payload.get("message"))
                        .map(extract_text)
                        .unwrap_or_default();
                    if !text.is_empty() {
                        events.push((
                            true,
                            PortableEvent::Summary {
                                text,
                                timestamp: record.get("timestamp").and_then(parse_time),
                            },
                        ));
                    }
                }
                ("event_msg", "error") => {
                    let text = payload.get("message").map(extract_text).unwrap_or_default();
                    if !text.is_empty() {
                        events.push((
                            true,
                            PortableEvent::Error {
                                text,
                                timestamp: record.get("timestamp").and_then(parse_time),
                            },
                        ));
                    }
                }
                _ => {}
            }
            Ok(())
        })?;
        session.captured_bytes = Some(bytes);
        session.partial = partial;
        // Codex emits UI messages beside persisted model items. Filtering after the stream
        // preserves tool ordering while preventing duplicate requests from looking emphatic.
        session.events = events
            .into_iter()
            .filter(|(canonical, event)| {
                *canonical
                    || matches!(event, PortableEvent::Message { role: Role::User, .. } if !has_response_user)
                    || matches!(event, PortableEvent::Message { role: Role::Assistant, .. } if !has_response_assistant)
            })
            .map(|(_, event)| event)
            .collect();
        Ok(session)
    }

    fn discover_targets(&self) -> Result<Vec<TargetOption>> {
        let mut targets = vec![TargetOption {
            id: "default".into(),
            label: "Codex default".into(),
            kind: TargetKind::Model,
        }];
        let mut models = BTreeMap::new();
        if let Ok(value) = std::fs::read(self.home.join("models_cache.json")).and_then(|bytes| {
            serde_json::from_slice::<Value>(&bytes).map_err(std::io::Error::other)
        }) && let Some(entries) = value.get("models").and_then(Value::as_array)
        {
            for entry in entries {
                let Some(id) = entry.get("slug").and_then(Value::as_str) else {
                    continue;
                };
                let label = entry
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(id);
                models.insert(id.to_owned(), label.to_owned());
            }
        }
        targets.extend(models.into_iter().map(|(id, label)| TargetOption {
            id,
            label,
            kind: TargetKind::Model,
        }));
        targets.push(TargetOption {
            id: "default".into(),
            label: "Codex default profile".into(),
            kind: TargetKind::Agent,
        });
        let mut profiles = BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(&self.home) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name.strip_suffix(".config.toml") {
                    profiles.insert(id.to_owned());
                }
            }
        }
        if let Ok(source) = std::fs::read_to_string(self.home.join("config.toml"))
            && let Ok(config) = toml::from_str::<toml::Value>(&source)
            && let Some(legacy) = config.get("profiles").and_then(toml::Value::as_table)
        {
            profiles.extend(legacy.keys().cloned());
        }
        // Profiles are user-authored launch presets; exposing their names avoids
        // copying provider settings or credentials into Possess.
        targets.extend(profiles.into_iter().map(|id| TargetOption {
            id: id.clone(),
            label: id,
            kind: TargetKind::Agent,
        }));
        Ok(targets)
    }
}

fn rollout_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    let candidate = stem.get(stem.len().saturating_sub(36)..)?;
    uuid::Uuid::parse_str(candidate)
        .ok()
        .map(|_| candidate.to_owned())
        .or_else(|| id_from_filename(path))
}

fn first_line(value: &str) -> String {
    value
        .lines()
        .next()
        .unwrap_or("Untitled session")
        .chars()
        .take(100)
        .collect()
}
fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn is_codex_injected(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("<collaboration_mode>")
        || trimmed.starts_with("# AGENTS.md instructions")
}

fn stable_jsonl_head(path: &Path, limit: usize) -> Result<(Vec<Value>, u64, bool)> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut values = Vec::new();
    let mut line = String::new();
    let mut bytes = 0;
    while values.len() < limit {
        line.clear();
        let count = reader.read_line(&mut line)?;
        if count == 0 || !line.ends_with('\n') {
            break;
        }
        bytes += count as u64;
        if let Ok(value) = serde_json::from_str(line.trim_end()) {
            values.push(value);
        }
    }
    Ok((values, bytes, false))
}
