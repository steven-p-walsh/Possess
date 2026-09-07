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
        let cwd = std::env::current_dir()?;
        let project = crate::project::repository_root(&cwd).unwrap_or(cwd);
        let settings: Vec<Value> = [
            self.home.join("settings.json"),
            project.join(".claude/settings.json"),
            project.join(".claude/settings.local.json"),
        ]
        .into_iter()
        .filter_map(|path| serde_json::from_slice(&std::fs::read(path).ok()?).ok())
        .collect();
        // Claude has no standalone model-catalog command. Local metadata keeps this
        // picker useful offline without starting a session, running hooks, or using auth.
        let recent = self
            .list_sessions()
            .unwrap_or_default()
            .into_iter()
            .take(100)
            .filter_map(|session| session.model);
        let mut targets = model_options(&settings, recent, |key| std::env::var(key).ok());
        targets.push(TargetOption {
            id: "default".into(),
            label: "Claude default agent".into(),
            kind: TargetKind::Agent,
        });
        targets.extend(markdown_targets(&[
            self.home.join("agents"),
            project.join(".claude/agents"),
        ]));
        Ok(targets)
    }
}

fn model_options(
    settings: &[Value],
    recent: impl IntoIterator<Item = String>,
    environment: impl Fn(&str) -> Option<String>,
) -> Vec<TargetOption> {
    let mut models = BTreeMap::new();
    // Aliases remain useful on a fresh install; Claude resolves their versions and
    // checks account availability when launched. The rest of the list is discovered.
    for (id, label) in [
        ("fable", "Fable"),
        ("opus", "Opus"),
        ("sonnet", "Sonnet"),
        ("haiku", "Haiku"),
    ] {
        insert_model(&mut models, id, Some(label));
    }
    for id in recent {
        insert_model(&mut models, &id, None);
    }
    for settings in settings {
        if let Some(model) = settings.get("model").and_then(Value::as_str) {
            insert_model(&mut models, model, None);
        }
        if let Some(allowed) = settings.get("availableModels").and_then(Value::as_array) {
            for model in allowed.iter().filter_map(Value::as_str) {
                insert_model(&mut models, model, None);
            }
        }
        if let Some(overrides) = settings.get("modelOverrides").and_then(Value::as_object) {
            for (model, provider_id) in overrides {
                insert_model(&mut models, model, None);
                if let Some(id) = provider_id.as_str() {
                    insert_model(&mut models, id, None);
                }
            }
        }
        add_environment_models(&mut models, |key| {
            settings.get("env")?.get(key)?.as_str().map(str::to_owned)
        });
    }
    add_environment_models(&mut models, environment);
    let mut options = vec![TargetOption {
        id: "default".into(),
        label: "Claude default".into(),
        kind: TargetKind::Model,
    }];
    options.extend(models.into_iter().map(|(id, label)| TargetOption {
        id,
        label,
        kind: TargetKind::Model,
    }));
    options
}

fn add_environment_models(
    models: &mut BTreeMap<String, String>,
    read: impl Fn(&str) -> Option<String>,
) {
    // Only model-related variables are read. Settings may also hold credentials and
    // command helpers, neither of which belongs in discovery or should be executed.
    for key in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_MODEL",
        "ANTHROPIC_CUSTOM_MODEL_OPTION",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        if let Some(id) = read(key) {
            let label = read(&format!("{key}_NAME"));
            insert_model(models, &id, label.as_deref());
        }
    }
}

fn insert_model(models: &mut BTreeMap<String, String>, id: &str, label: Option<&str>) {
    let id = id.trim();
    if id.is_empty()
        || id == "default"
        || id == "inherit"
        || id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || id.contains(['<', '>'])
    {
        return;
    }
    let label = label.filter(|value| !value.trim().is_empty());
    if let Some(label) = label {
        models.insert(
            id.into(),
            label.chars().filter(|c| !c.is_control()).collect(),
        );
    } else {
        models.entry(id.into()).or_insert_with(|| id.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_models_follow_local_configuration_and_history_without_a_cli() {
        let settings = json!({
            "model": "claude-fable-future",
            "availableModels": ["claude-org-model", "sonnet"],
            "modelOverrides": { "claude-team-model": "provider/team-deployment" },
            "env": {
                "ANTHROPIC_DEFAULT_FABLE_MODEL": "provider/fable-deployment",
                "ANTHROPIC_CUSTOM_MODEL_OPTION": "gateway/team-model",
                "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME": "Team model",
                "ANTHROPIC_API_KEY": "secret-must-not-be-listed"
            },
            "apiKeyHelper": "must-not-be-executed"
        });
        let recent = [
            "claude-from-history".into(),
            "sonnet".into(),
            "<synthetic>".into(),
        ];
        let options = model_options(&[settings], recent, |key| {
            (key == "ANTHROPIC_MODEL").then(|| "claude-from-environment".into())
        });
        let ids: Vec<_> = options.iter().map(|option| option.id.as_str()).collect();
        for expected in [
            "default",
            "fable",
            "claude-fable-future",
            "claude-org-model",
            "claude-team-model",
            "provider/team-deployment",
            "provider/fable-deployment",
            "gateway/team-model",
            "claude-from-history",
            "claude-from-environment",
        ] {
            assert!(ids.contains(&expected), "missing {expected}");
        }
        assert_eq!(ids.iter().filter(|id| **id == "sonnet").count(), 1);
        assert!(!ids.contains(&"secret-must-not-be-listed"));
        assert!(!ids.contains(&"<synthetic>"));
        assert_eq!(
            options
                .iter()
                .find(|option| option.id == "gateway/team-model")
                .unwrap()
                .label,
            "Team model"
        );
    }
}
