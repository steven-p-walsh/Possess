//! Grok session adapter.
//!
//! Grok documents its summary and ACP files, yet released binaries do not all ship the
//! same import command. Capability probing is safer than version guessing.

use super::*;
use crate::domain::{PortableEvent, Role, TargetKind, TodoItem, ToolOperation};

pub struct GrokAdapter {
    home: PathBuf,
    binary: PathBuf,
}

impl GrokAdapter {
    pub fn new(config: &Config) -> Self {
        Self {
            home: config.harness_home(Harness::Grok),
            binary: path_from_command(config, Harness::Grok),
        }
    }
}

impl HarnessAdapter for GrokAdapter {
    fn harness(&self) -> Harness {
        Harness::Grok
    }

    fn probe(&self) -> AdapterStatus {
        let version = command_version(&self.binary);
        let import = Command::new(&self.binary)
            .arg("--help")
            .output()
            .ok()
            .is_some_and(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|line| line.trim_start().starts_with("import "))
            });
        AdapterStatus {
            harness: Harness::Grok,
            binary: binary_exists(&self.binary).then(|| self.binary.clone()),
            version,
            store: self.home.join("sessions"),
            store_exists: self.home.join("sessions").exists(),
            supported_import: false,
            native_writer: false,
            detail: if import {
                "documented session store; bootstrap used (CLI also has a Claude-only importer)"
            } else {
                "documented session store; interactive prompt bootstrap (import unavailable)"
            }
            .into(),
        }
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let root = self.home.join("sessions");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .min_depth(2)
            .max_depth(3)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_name() != "summary.json" || !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let value: Value = serde_json::from_slice(&std::fs::read(path)?).unwrap_or(Value::Null);
            let id = value
                .pointer("/info/id")
                .or_else(|| value.get("session_id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    path.parent()
                        .and_then(Path::file_name)
                        .map(|v| v.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| "unknown".into());
            let mut item = SessionSummary::new(
                Harness::Grok,
                id,
                path.parent().unwrap_or(path).to_path_buf(),
            );
            item.cwd = value
                .pointer("/info/cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_default();
            item.title = value
                .get("generated_title")
                .and_then(Value::as_str)
                .filter(|v| !v.trim().is_empty())
                .or_else(|| {
                    value
                        .get("session_summary")
                        .and_then(Value::as_str)
                        .filter(|v| !v.trim().is_empty())
                })
                .unwrap_or("Untitled Grok session")
                .to_owned();
            item.preview = value
                .get("last_turn_summary")
                .or_else(|| value.get("last_recap"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            item.model = value
                .get("current_model_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            item.agent = value
                .get("agent_name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            item.created_at = value.get("created_at").and_then(parse_time);
            item.updated_at = value
                .get("updated_at")
                .or_else(|| value.get("last_active_at"))
                .and_then(parse_time)
                .or_else(|| file_mtime(path));
            item.turn_count = value
                .get("num_chat_messages")
                .or_else(|| value.get("num_messages"))
                .and_then(Value::as_u64);
            sessions.push(item);
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }

    fn load_session(&self, summary: &SessionSummary) -> Result<PortableSessionV1> {
        let dir = &summary.source_path;
        let updates = dir.join("updates.jsonl");
        let chat = dir.join("chat_history.jsonl");
        // Raw chat has less UI noise. ACP remains the fallback for sessions created by
        // clients that did not persist a model-facing chat history.
        let source = if chat.exists() { &chat } else { &updates };
        let mut session = PortableSessionV1::new(summary.clone());
        let (bytes, partial) = for_each_stable_jsonl(source, |record| {
            if source == &updates {
                parse_acp_update(&mut session, &record);
            } else {
                parse_chat_record(&mut session, &record);
            }
            Ok(())
        })?;
        session.captured_bytes = Some(bytes);
        session.partial = partial;
        let plan = dir.join("plan.json");
        if let Ok(value) = std::fs::read(&plan)
            .and_then(|v| serde_json::from_slice::<Value>(&v).map_err(std::io::Error::other))
        {
            let items = value
                .as_array()
                .or_else(|| value.get("items").and_then(Value::as_array));
            if let Some(items) = items {
                for item in items {
                    if let Some(text) = item
                        .get("content")
                        .or_else(|| item.get("text"))
                        .and_then(Value::as_str)
                    {
                        session.todos.push(TodoItem {
                            text: text.to_owned(),
                            status: item
                                .get("status")
                                .and_then(Value::as_str)
                                .unwrap_or("pending")
                                .to_owned(),
                            priority: item
                                .get("priority")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        });
                    }
                }
            }
        }
        Ok(session)
    }

    fn discover_targets(&self) -> Result<Vec<TargetOption>> {
        let mut targets = vec![TargetOption {
            id: "default".into(),
            label: "Grok default".into(),
            kind: TargetKind::Model,
        }];
        if let Ok(output) = Command::new(&self.binary).arg("models").output() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let id = line.trim().trim_start_matches(['*', '-']).trim();
                if id.starts_with("grok-") {
                    targets.push(TargetOption {
                        id: id.into(),
                        label: id.into(),
                        kind: TargetKind::Model,
                    });
                }
            }
        }
        targets.push(TargetOption {
            id: "default".into(),
            label: "Grok default agent".into(),
            kind: TargetKind::Agent,
        });
        targets.extend(markdown_targets(&[
            self.home.join("agents"),
            self.home.join("bundled/agents"),
        ]));
        Ok(targets)
    }
}

fn parse_chat_record(session: &mut PortableSessionV1, record: &Value) {
    let kind = record
        .get("role")
        .or_else(|| record.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let timestamp = record.get("timestamp").and_then(parse_time);
    match kind {
        "user" | "assistant" => {
            let text = extract_text(
                record
                    .get("content")
                    .or_else(|| record.get("message"))
                    .unwrap_or(&Value::Null),
            );
            if !text.is_empty() {
                session.events.push(PortableEvent::Message {
                    id: record.get("id").and_then(Value::as_str).map(str::to_owned),
                    parent_id: None,
                    role: if kind == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    text,
                    timestamp,
                });
            }
            if let Some(calls) = record.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let input = call.get("arguments").cloned().unwrap_or(Value::Null);
                    session.events.push(PortableEvent::ToolCall {
                        id,
                        operation: ToolOperation::infer(&name),
                        name,
                        input,
                        timestamp,
                    });
                }
            }
        }
        "tool" | "tool_result" => {
            let call_id = record
                .get("tool_use_id")
                .or_else(|| record.get("call_id"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            session.events.push(PortableEvent::ToolResult {
                call_id,
                output: extract_text(record.get("content").unwrap_or(&Value::Null)),
                is_error: false,
                timestamp,
            });
        }
        _ => {}
    }
}

fn parse_acp_update(session: &mut PortableSessionV1, record: &Value) {
    let params = &record["params"];
    let update = params.get("update").unwrap_or(params);
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "user_message_chunk" | "agent_message_chunk" => {
            let text = extract_text(update.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                let role = if kind.starts_with("user") {
                    Role::User
                } else {
                    Role::Assistant
                };
                if let Some(PortableEvent::Message {
                    role: previous_role,
                    text: previous_text,
                    ..
                }) = session.events.last_mut()
                    && *previous_role == role
                {
                    // ACP streams chunks rather than durable messages. Merging adjacent
                    // chunks keeps the destination from treating deltas as separate turns.
                    previous_text.push_str(&text);
                } else {
                    session.events.push(PortableEvent::Message {
                        id: None,
                        parent_id: None,
                        role,
                        text,
                        timestamp: None,
                    });
                }
            }
        }
        "tool_call" => {
            let id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let name = update
                .get("title")
                .or_else(|| update.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            session.events.push(PortableEvent::ToolCall {
                id,
                operation: ToolOperation::infer(&name),
                name,
                input: update.get("rawInput").cloned().unwrap_or(Value::Null),
                timestamp: None,
            });
        }
        "tool_call_update" => {
            if let Some(output) = update
                .get("rawOutput")
                .map(extract_text)
                .filter(|v| !v.is_empty())
            {
                session.events.push(PortableEvent::ToolResult {
                    call_id: update
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    output,
                    is_error: update.get("status").and_then(Value::as_str) == Some("failed"),
                    timestamp: None,
                });
            }
        }
        "plan" => {
            if let Some(entries) = update.get("entries").and_then(Value::as_array) {
                for item in entries {
                    if let Some(text) = item.get("content").and_then(Value::as_str) {
                        session.todos.push(TodoItem {
                            text: text.to_owned(),
                            status: item
                                .get("status")
                                .and_then(Value::as_str)
                                .unwrap_or("pending")
                                .into(),
                            priority: None,
                        });
                    }
                }
            }
        }
        _ => {}
    }
}
