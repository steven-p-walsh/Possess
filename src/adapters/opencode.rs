//! OpenCode adapter.
//!
//! OpenCode exposes supported import/export commands, while read-only SQLite access makes
//! the unified picker fast and works without a running OpenCode server.

use super::*;
use crate::domain::{PortableEvent, Role, TargetKind, TodoItem, ToolOperation};
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeSet;

pub struct OpenCodeAdapter {
    home: PathBuf,
    binary: PathBuf,
}

impl OpenCodeAdapter {
    pub fn new(config: &Config) -> Self {
        Self {
            home: config.harness_home(Harness::OpenCode),
            binary: path_from_command(config, Harness::OpenCode),
        }
    }

    fn connect(&self) -> Result<Connection> {
        let path = self.home.join("opencode.db");
        Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(Into::into)
    }
}

impl HarnessAdapter for OpenCodeAdapter {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn probe(&self) -> AdapterStatus {
        let version = command_version(&self.binary);
        AdapterStatus {
            harness: Harness::OpenCode,
            binary: binary_exists(&self.binary).then(|| self.binary.clone()),
            version,
            store: self.home.join("opencode.db"),
            store_exists: self.home.join("opencode.db").exists(),
            supported_import: binary_exists(&self.binary),
            native_writer: false,
            detail: "read-only SQLite discovery; first-party JSON export/import".into(),
        }
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let conn = match self.connect() {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        let mut statement = conn.prepare("SELECT id, directory, title, version, time_created, time_updated, time_archived FROM session ORDER BY time_updated DESC")?;
        let rows = statement.query_map([], |row| {
            let id: String = row.get(0)?;
            let mut item =
                SessionSummary::new(Harness::OpenCode, id, self.home.join("opencode.db"));
            item.cwd = PathBuf::from(row.get::<_, String>(1)?);
            item.title = row.get(2)?;
            item.version = row.get(3).ok();
            item.created_at = row
                .get::<_, i64>(4)
                .ok()
                .and_then(|v| Utc.timestamp_millis_opt(v).single());
            item.updated_at = row
                .get::<_, i64>(5)
                .ok()
                .and_then(|v| Utc.timestamp_millis_opt(v).single());
            item.archived = row.get::<_, Option<i64>>(6).unwrap_or(None).is_some();
            Ok(item)
        })?;
        let mut sessions: Vec<_> = rows.filter_map(Result::ok).collect();
        drop(statement);
        let mut preview_statement = conn.prepare(
            "SELECT p.data FROM part p JOIN message m ON m.id = p.message_id WHERE m.session_id = ? AND json_extract(p.data, '$.type') = 'text' ORDER BY p.time_created DESC, p.id DESC LIMIT 1",
        )?;
        let mut model_statement = conn.prepare(
            "SELECT data FROM message WHERE session_id = ? AND json_extract(data, '$.role') = 'assistant' ORDER BY time_created DESC, id DESC LIMIT 1",
        )?;
        for session in &mut sessions {
            if let Ok(data) =
                preview_statement.query_row([&session.vendor_id], |row| row.get::<_, String>(0))
                && let Ok(value) = serde_json::from_str::<Value>(&data)
            {
                session.preview = value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
            }
            if let Ok(data) =
                model_statement.query_row([&session.vendor_id], |row| row.get::<_, String>(0))
                && let Ok(value) = serde_json::from_str::<Value>(&data)
            {
                let provider = value.get("providerID").and_then(Value::as_str);
                let model = value.get("modelID").and_then(Value::as_str);
                session.model = match (provider, model) {
                    (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
                    (_, Some(model)) => Some(model.to_owned()),
                    _ => None,
                };
                session.agent = value
                    .get("agent")
                    .or_else(|| value.get("mode"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
        Ok(sessions)
    }

    fn load_session(&self, summary: &SessionSummary) -> Result<PortableSessionV1> {
        let conn = self.connect()?;
        // A single read transaction keeps messages and parts from different WAL moments
        // out of the same handoff when OpenCode is still active.
        let tx = conn.unchecked_transaction()?;
        let mut session = PortableSessionV1::new(summary.clone());
        let mut statement = tx.prepare("SELECT id, data, time_created FROM message WHERE session_id = ? ORDER BY time_created, id")?;
        let messages: Vec<(String, String, i64)> = statement
            .query_map([&summary.vendor_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .filter_map(Result::ok)
            .collect();
        drop(statement);
        for (message_id, data, created) in messages {
            let value: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            let role = match value.get("role").and_then(Value::as_str) {
                Some("user") => Some(Role::User),
                Some("assistant") => Some(Role::Assistant),
                _ => None,
            };
            let timestamp = Utc.timestamp_millis_opt(created).single();
            let mut part_statement =
                tx.prepare("SELECT data FROM part WHERE message_id = ? ORDER BY time_created, id")?;
            let parts: Vec<String> = part_statement
                .query_map([&message_id], |r| r.get(0))?
                .filter_map(Result::ok)
                .collect();
            for part in parts {
                let part: Value = serde_json::from_str(&part).unwrap_or(Value::Null);
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                            && let Some(role) = role
                        {
                            session.events.push(PortableEvent::Message {
                                id: Some(message_id.clone()),
                                parent_id: None,
                                role,
                                text: text.to_owned(),
                                timestamp,
                            });
                        }
                    }
                    "tool" => {
                        let id = part
                            .get("callID")
                            .or_else(|| part.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned();
                        let name = part
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_owned();
                        let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
                        session.events.push(PortableEvent::ToolCall {
                            id: id.clone(),
                            operation: ToolOperation::infer(&name),
                            name,
                            input,
                            timestamp,
                        });
                        if let Some(output) = part.pointer("/state/output") {
                            session.events.push(PortableEvent::ToolResult {
                                call_id: id,
                                output: extract_text(output),
                                is_error: part.pointer("/state/status").and_then(Value::as_str)
                                    == Some("error"),
                                timestamp,
                            });
                        }
                    }
                    "error" => {
                        let text = part.get("error").map(extract_text).unwrap_or_default();
                        session
                            .events
                            .push(PortableEvent::Error { text, timestamp });
                    }
                    _ => {}
                }
            }
        }
        if let Ok(mut todo_statement) = tx.prepare(
            "SELECT content, status, priority FROM todo WHERE session_id = ? ORDER BY position",
        ) && let Ok(rows) = todo_statement.query_map([&summary.vendor_id], |r| {
            Ok(TodoItem {
                text: r.get(0)?,
                status: r.get(1)?,
                priority: r.get(2).ok(),
            })
        }) {
            // Todo storage has moved between OpenCode releases. Conversation recovery
            // should still succeed when this optional table is absent.
            session.todos = rows.filter_map(Result::ok).collect();
        }
        Ok(session)
    }

    fn discover_targets(&self) -> Result<Vec<TargetOption>> {
        let mut targets = vec![TargetOption {
            id: "default".into(),
            label: "OpenCode default".into(),
            kind: TargetKind::Model,
        }];
        let mut models = BTreeSet::new();
        if let Ok(conn) = self.connect()
            && let Ok(mut statement) =
                conn.prepare("SELECT data FROM message ORDER BY time_created DESC LIMIT 500")
            && let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0))
        {
            // Recent local history is a useful offline model catalog. Running `models`
            // here can authenticate or contact providers just because the wizard opened.
            for data in rows.filter_map(Result::ok) {
                let Ok(value) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                let provider = value
                    .get("providerID")
                    .or_else(|| value.pointer("/model/providerID"))
                    .and_then(Value::as_str);
                let model = value
                    .get("modelID")
                    .or_else(|| value.pointer("/model/modelID"))
                    .and_then(Value::as_str);
                if let (Some(provider), Some(model)) = (provider, model) {
                    models.insert(format!("{provider}/{model}"));
                }
            }
        }
        targets.extend(models.into_iter().map(|id| TargetOption {
            label: id.clone(),
            id,
            kind: TargetKind::Model,
        }));
        targets.push(TargetOption {
            id: "default".into(),
            label: "OpenCode default agent".into(),
            kind: TargetKind::Agent,
        });
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            });
        targets.extend(markdown_targets(&[
            config_home.join("opencode/agents"),
            PathBuf::from(".opencode/agents"),
        ]));
        Ok(targets)
    }
}
