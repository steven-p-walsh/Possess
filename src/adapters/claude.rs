//! Claude Code transcript discovery and normalization.
//!
//! Listing reads a bounded tail because transcripts can be large while recent records
//! repeat the metadata needed by the picker.

use super::*;
use crate::domain::{Attachment, PortableEvent, Role, SkippedRecord, TargetKind, ToolOperation};
use std::io::{Read, Seek, SeekFrom};

pub struct ClaudeAdapter {
    home: PathBuf,
    binary: PathBuf,
}

impl ClaudeAdapter {
    pub fn new(config: &Config) -> Self {
        Self {
            home: config.harness_home(Harness::Claude),
            binary: path_from_command(config, Harness::Claude),
        }
    }

    fn metadata(path: &Path) -> Result<SessionSummary> {
        let id = path
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("unknown")
            .to_owned();
        let mut item = SessionSummary::new(Harness::Claude, id, path.to_path_buf());
        item.updated_at = file_mtime(path);
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        // First-paint cost should depend on session count, not transcript size.
        let start = len.saturating_sub(128 * 1024);
        file.seek(SeekFrom::Start(start))?;
        let mut tail = String::new();
        file.read_to_string(&mut tail)?;
        if start > 0 {
            tail = tail
                .split_once('\n')
                .map(|(_, rest)| rest.to_owned())
                .unwrap_or_default();
        }
        for line in tail.lines().rev().take(150) {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if item.cwd.as_os_str().is_empty() {
                item.cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_default();
            }
            if item.version.is_none() {
                item.version = value
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            if item.model.is_none() {
                item.model = value
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            if item.title == "Untitled session"
                && let Some(title) = value.get("aiTitle").and_then(Value::as_str)
            {
                item.title = title.to_owned();
            }
            if item.preview.is_empty() && value.get("type").and_then(Value::as_str) == Some("user")
            {
                let text = extract_text(&value["message"]["content"]);
                if !text.is_empty() && value.get("isMeta").and_then(Value::as_bool) != Some(true) {
                    item.preview = text;
                }
            }
        }
        if item.title == "Untitled session" && !item.preview.is_empty() {
            item.title = item
                .preview
                .lines()
                .next()
                .unwrap_or("Untitled session")
                .chars()
                .take(100)
                .collect();
        }
        Ok(item)
    }
}

impl HarnessAdapter for ClaudeAdapter {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn probe(&self) -> AdapterStatus {
        let version = command_version(&self.binary);
        AdapterStatus {
            harness: Harness::Claude,
            binary: binary_exists(&self.binary).then(|| self.binary.clone()),
            version: version.clone(),
            store: self.home.join("projects"),
            store_exists: self.home.join("projects").exists(),
            supported_import: false,
            native_writer: version.as_deref().is_some_and(|v| v.starts_with("2.1.")),
            detail: "project JSONL discovery; version-gated native transcript; bootstrap fallback"
                .into(),
        }
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let root = self.home.join("projects");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .max_depth(2)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if entry.file_type().is_file()
                && path.extension().and_then(|v| v.to_str()) == Some("jsonl")
                && let Ok(session) = Self::metadata(path)
            {
                sessions.push(session);
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }

    fn load_session(&self, summary: &SessionSummary) -> Result<PortableSessionV1> {
        let mut session = PortableSessionV1::new(summary.clone());
        let mut skipped_thinking = 0;
        let (bytes, partial) = for_each_stable_jsonl(&summary.source_path, |record| {
            let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
            let timestamp = record.get("timestamp").and_then(parse_time);
            match kind {
                "user" | "assistant" => {
                    if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
                        return Ok(());
                    }
                    let role = if kind == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    };
                    let content = &record["message"]["content"];
                    let text = extract_text(content);
                    if !text.trim().is_empty() && record.get("toolUseResult").is_none() {
                        session.events.push(PortableEvent::Message {
                            id: record
                                .get("uuid")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            parent_id: record
                                .get("parentUuid")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            role,
                            text,
                            timestamp,
                        });
                    }
                    if let Some(blocks) = content.as_array() {
                        for block in blocks {
                            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                                "tool_use" => {
                                    let id = block
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown")
                                        .to_owned();
                                    let name = block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("tool")
                                        .to_owned();
                                    session.events.push(PortableEvent::ToolCall {
                                        id,
                                        operation: ToolOperation::infer(&name),
                                        name,
                                        input: block.get("input").cloned().unwrap_or(Value::Null),
                                        timestamp,
                                    });
                                }
                                "tool_result" => {
                                    let call_id = block
                                        .get("tool_use_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown")
                                        .to_owned();
                                    let output =
                                        extract_text(block.get("content").unwrap_or(&Value::Null));
                                    session.events.push(PortableEvent::ToolResult {
                                        call_id,
                                        output,
                                        is_error: block
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false),
                                        timestamp,
                                    });
                                }
                                // Signed reasoning is provider-private integrity data. Visible
                                // summaries carry useful state without pretending it can replay.
                                "thinking" => skipped_thinking += 1,
                                _ => {}
                            }
                        }
                    }
                }
                "system"
                    if record
                        .get("subtype")
                        .and_then(Value::as_str)
                        .is_some_and(|v| v.contains("compact")) =>
                {
                    let text = extract_text(record.get("content").unwrap_or(&Value::Null));
                    if !text.is_empty() {
                        session
                            .events
                            .push(PortableEvent::Summary { text, timestamp });
                    }
                }
                "attachment" => {
                    if let Some(path) = record
                        .pointer("/attachment/filePath")
                        .and_then(Value::as_str)
                    {
                        session.attachments.push(Attachment {
                            path: PathBuf::from(path),
                            media_type: record
                                .pointer("/attachment/mediaType")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            name: None,
                        });
                    }
                }
                _ => {}
            }
            Ok(())
        })?;
        session.captured_bytes = Some(bytes);
        session.partial = partial;
        if skipped_thinking > 0 {
            session.skipped.push(SkippedRecord {
                record_type: "thinking".into(),
                reason: "private signed reasoning is not portable".into(),
                count: skipped_thinking,
            });
        }
        Ok(session)
    }

    fn discover_targets(&self) -> Result<Vec<TargetOption>> {
        let mut targets = vec![
            TargetOption {
                id: "default".into(),
                label: "Claude default".into(),
                kind: TargetKind::Model,
            },
            TargetOption {
                id: "sonnet".into(),
                label: "Sonnet".into(),
                kind: TargetKind::Model,
            },
            TargetOption {
                id: "opus".into(),
                label: "Opus".into(),
                kind: TargetKind::Model,
            },
            TargetOption {
                id: "haiku".into(),
                label: "Haiku".into(),
                kind: TargetKind::Model,
            },
            TargetOption {
                id: "default".into(),
                label: "Claude default agent".into(),
                kind: TargetKind::Agent,
            },
        ];
        targets.extend(markdown_targets(&[
            self.home.join("agents"),
            PathBuf::from(".claude/agents"),
        ]));
        Ok(targets)
    }
}
