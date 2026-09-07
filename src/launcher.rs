//! Destination creation and terminal process launch.
//!
//! Supported interfaces take priority. Private formats are used only for narrow, tested
//! versions because a plausible-looking corrupt session is worse than a labeled bootstrap.

use crate::config::{Config, set_private_file};
use crate::domain::{Fidelity, Harness, LaunchRequest, PortableEvent, PortableSessionV1, Role};
use crate::handoff::HandoffPackage;
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PreparedLaunch {
    pub fidelity: Fidelity,
    pub destination_session_id: Option<String>,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub detail: String,
}

pub fn preferred_fidelity(config: &Config, source: Harness, destination: Harness) -> Fidelity {
    if source == destination {
        return Fidelity::NativeResume;
    }
    match destination {
        Harness::OpenCode => Fidelity::SupportedImport,
        Harness::Codex if codex_writer_supported(config) => Fidelity::CompatibleWriter,
        Harness::Claude if claude_writer_supported(config) => Fidelity::CompatibleWriter,
        _ => Fidelity::SmartBootstrap,
    }
}

pub fn prepare(
    config: &Config,
    session: &PortableSessionV1,
    package: &HandoffPackage,
    request: &LaunchRequest,
) -> Result<PreparedLaunch> {
    let binary = config.binary(request.destination);
    let cwd = if session.source.cwd.exists() {
        session.source.cwd.clone()
    } else {
        std::env::current_dir()?
    };
    match request.destination {
        Harness::OpenCode => prepare_opencode(config, session, package, request, cwd),
        Harness::Codex if codex_writer_supported(config) => {
            prepare_codex(config, session, package, request, cwd.clone())
                .or_else(|_| Ok(bootstrap(binary, package, request, cwd)))
        }
        Harness::Claude if claude_writer_supported(config) => {
            prepare_claude(config, session, package, request, cwd.clone())
                .or_else(|_| Ok(bootstrap(binary, package, request, cwd)))
        }
        _ => Ok(bootstrap(binary, package, request, cwd)),
    }
}

pub fn launch(prepared: &PreparedLaunch) -> Result<ExitStatus> {
    Command::new(&prepared.program)
        .args(&prepared.args)
        .current_dir(&prepared.cwd)
        .status()
        .with_context(|| format!("failed to launch {}", prepared.program.display()))
}

pub fn native_resume(
    config: &Config,
    session: &crate::domain::SessionSummary,
    request: Option<&LaunchRequest>,
) -> PreparedLaunch {
    let program = config.binary(session.harness);
    let cwd = if session.cwd.exists() {
        session.cwd.clone()
    } else {
        std::env::current_dir().unwrap_or_default()
    };
    let mut args = match session.harness {
        Harness::Codex => vec![
            "resume".into(),
            session.vendor_id.clone(),
            "-C".into(),
            cwd.display().to_string(),
        ],
        Harness::Claude => vec!["--resume".into(), session.vendor_id.clone()],
        Harness::OpenCode => vec![
            cwd.display().to_string(),
            "--session".into(),
            session.vendor_id.clone(),
        ],
        Harness::Grok => vec![
            "--cwd".into(),
            cwd.display().to_string(),
            "--resume".into(),
            session.vendor_id.clone(),
        ],
    };
    if let Some(request) = request {
        add_target_args(&mut args, request);
    }
    PreparedLaunch {
        fidelity: Fidelity::NativeResume,
        destination_session_id: Some(session.vendor_id.clone()),
        program,
        args,
        cwd,
        detail: "resuming original native session".into(),
    }
}

fn prepare_opencode(
    config: &Config,
    _session: &PortableSessionV1,
    package: &HandoffPackage,
    request: &LaunchRequest,
    cwd: PathBuf,
) -> Result<PreparedLaunch> {
    let binary = config.binary(Harness::OpenCode);
    let native = package
        .native_path
        .as_ref()
        .context("OpenCode handoff export is missing")?;
    // The importer requires an existing project row. Asking OpenCode to resolve the
    // workspace first preserves its ownership of that schema and avoids direct DB writes.
    let initialized = Command::new(&binary)
        .args(["session", "list", "--format", "json"])
        .current_dir(&cwd)
        .output()?;
    if !initialized.status.success() {
        return Ok(bootstrap(binary, package, request, cwd));
    }
    let output = Command::new(&binary)
        .arg("import")
        .arg(native)
        .current_dir(&cwd)
        .output()?;
    if !output.status.success() {
        return Ok(bootstrap(binary, package, request, cwd));
    }
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let session_id = combined
        .split(|c: char| c.is_whitespace() || matches!(c, ':' | ',' | '(' | ')'))
        .find(|part| part.starts_with("ses_"))
        .map(str::to_owned);
    let mut args = vec![cwd.display().to_string()];
    if let Some(id) = &session_id {
        args.extend(["--session".into(), id.clone()]);
        args.extend(["--prompt".into(), package_prompt(package)]);
    }
    add_target_args(&mut args, request);
    Ok(PreparedLaunch {
        fidelity: Fidelity::SupportedImport,
        destination_session_id: session_id,
        program: binary,
        args,
        cwd,
        detail: "created with OpenCode's supported JSON importer".into(),
    })
}

fn prepare_claude(
    config: &Config,
    session: &PortableSessionV1,
    package: &HandoffPackage,
    request: &LaunchRequest,
    cwd: PathBuf,
) -> Result<PreparedLaunch> {
    let id = Uuid::now_v7().to_string();
    let root = config
        .harness_home(Harness::Claude)
        .join("projects")
        .join(claude_project_slug(&cwd));
    std::fs::create_dir_all(&root)?;
    let final_path = root.join(format!("{id}.jsonl"));
    let temp_path = root.join(format!(".{id}.tmp"));
    let mut file = std::fs::File::create(&temp_path)?;
    let mut parent: Option<String> = None;
    for event in &session.events {
        let PortableEvent::Message {
            role,
            text,
            timestamp,
            ..
        } = event
        else {
            continue;
        };
        let uuid = Uuid::now_v7().to_string();
        let ts = timestamp.unwrap_or_else(Utc::now).to_rfc3339();
        let record = if *role == Role::User {
            json!({ "parentUuid": parent, "isSidechain": false, "userType": "external", "cwd": cwd, "sessionId": id, "version": "2.1", "type": "user", "message": { "role": "user", "content": text }, "uuid": uuid, "timestamp": ts })
        } else {
            json!({ "parentUuid": parent, "isSidechain": false, "userType": "external", "cwd": cwd, "sessionId": id, "version": "2.1", "type": "assistant", "message": { "id": format!("msg_{}", Uuid::now_v7().simple()), "type": "message", "role": "assistant", "model": session.source.model.as_deref().unwrap_or("imported"), "content": [{ "type": "text", "text": text }], "stop_reason": "end_turn", "stop_sequence": null, "usage": { "input_tokens": 0, "output_tokens": 0 } }, "uuid": uuid, "timestamp": ts })
        };
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
        parent = Some(uuid);
    }
    file.flush()?;
    drop(file);
    set_private_file(&temp_path)?;
    std::fs::rename(temp_path, &final_path)?;
    let mut args = vec![
        "--resume".into(),
        id.clone(),
        "--name".into(),
        format!("Possessed: {}", session.source.title),
        package_prompt(package),
    ];
    add_target_args(&mut args, request);
    Ok(PreparedLaunch {
        fidelity: Fidelity::CompatibleWriter,
        destination_session_id: Some(id),
        program: config.binary(Harness::Claude),
        args,
        cwd,
        detail: "created a version-gated Claude linked transcript".into(),
    })
}

fn prepare_codex(
    config: &Config,
    session: &PortableSessionV1,
    package: &HandoffPackage,
    request: &LaunchRequest,
    cwd: PathBuf,
) -> Result<PreparedLaunch> {
    // Widening the matching version gate requires a fixture and a real resume check: the
    // Codex index has changed enough that optimistic compatibility can strand a session.
    let id = Uuid::now_v7().to_string();
    let now = Utc::now();
    let dir = config
        .harness_home(Harness::Codex)
        .join("sessions")
        .join(now.format("%Y/%m/%d").to_string());
    std::fs::create_dir_all(&dir)?;
    let filename = format!("rollout-{}-{id}.jsonl", now.format("%Y-%m-%dT%H-%M-%S"));
    let final_path = dir.join(filename);
    let temp_path = dir.join(format!(".{id}.tmp"));
    let mut file = std::fs::File::create(&temp_path)?;
    write_jsonl(
        &mut file,
        &json!({ "timestamp": now.to_rfc3339(), "type": "session_meta", "payload": { "id": id, "timestamp": now.to_rfc3339(), "cwd": cwd, "originator": "possess", "cli_version": "0.153.4", "source": "cli", "model_provider": "openai" } }),
    )?;
    for event in &session.events {
        if let PortableEvent::Message {
            role,
            text,
            timestamp,
            ..
        } = event
        {
            write_jsonl(
                &mut file,
                &json!({ "timestamp": timestamp.unwrap_or(now).to_rfc3339(), "type": "response_item", "payload": { "type": "message", "role": if *role == Role::User { "user" } else { "assistant" }, "content": [{ "type": if *role == Role::User { "input_text" } else { "output_text" }, "text": text }] } }),
            )?;
        }
    }
    file.flush()?;
    drop(file);
    set_private_file(&temp_path)?;
    std::fs::rename(&temp_path, &final_path)?;

    let db = config.harness_home(Harness::Codex).join("state_5.sqlite");
    let mut conn = Connection::open(&db)?;
    let tx = conn.transaction()?;
    let epoch = now.timestamp();
    let millis = now.timestamp_millis();
    let preview = session
        .events
        .iter()
        .find_map(|e| match e {
            PortableEvent::Message {
                role: Role::User,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    let insert = tx.execute(
        "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, tokens_used, has_user_event, archived, cli_version, first_user_message, model, created_at_ms, updated_at_ms, preview, recency_at, recency_at_ms, history_mode) VALUES (?1, ?2, ?3, ?3, 'cli', 'openai', ?4, ?5, 'workspace-write', 'on-request', 0, 1, 0, '0.153.4', ?6, ?7, ?8, ?8, ?6, ?3, ?8, 'legacy')",
        params![id, final_path.display().to_string(), epoch, cwd.display().to_string(), format!("Possessed: {}", session.source.title), preview, request.model, millis]
    );
    if let Err(error) = insert {
        drop(tx);
        let _ = std::fs::remove_file(&final_path);
        return Err(error.into());
    }
    tx.commit()?;
    let mut args = vec![
        "resume".into(),
        id.clone(),
        "-C".into(),
        cwd.display().to_string(),
        package_prompt(package),
    ];
    add_target_args(&mut args, request);
    Ok(PreparedLaunch {
        fidelity: Fidelity::CompatibleWriter,
        destination_session_id: Some(id),
        program: config.binary(Harness::Codex),
        args,
        cwd,
        detail: "created a version-gated Codex rollout and index entry".into(),
    })
}

fn bootstrap(
    binary: PathBuf,
    package: &HandoffPackage,
    request: &LaunchRequest,
    cwd: PathBuf,
) -> PreparedLaunch {
    let prompt = package_prompt(package);
    let mut args = match request.destination {
        Harness::Codex => vec!["-C".into(), cwd.display().to_string(), prompt],
        Harness::Claude => vec!["--name".into(), format!("Possessed {}", package.id), prompt],
        Harness::OpenCode => vec![cwd.display().to_string(), "--prompt".into(), prompt],
        Harness::Grok => vec!["--cwd".into(), cwd.display().to_string(), prompt],
    };
    add_target_args(&mut args, request);
    PreparedLaunch {
        fidelity: Fidelity::SmartBootstrap,
        destination_session_id: None,
        program: binary,
        args,
        cwd,
        detail: format!(
            "smart handoff; full archive at {}",
            package.manifest_path.display()
        ),
    }
}

fn package_prompt(package: &HandoffPackage) -> String {
    std::fs::read_to_string(&package.prompt_path).unwrap_or_else(|_| {
        format!(
            "Resume the Possess handoff at {}",
            package.manifest_path.display()
        )
    })
}

fn add_target_args(args: &mut Vec<String>, request: &LaunchRequest) {
    if let Some(model) = request.model.as_ref().filter(|v| v.as_str() != "default") {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(agent) = request.agent.as_ref().filter(|v| !v.is_empty()) {
        let flag = if request.destination == Harness::Codex {
            // Codex calls its persisted launch presets profiles; the portable request
            // uses one field so the UI does not need destination-specific wizard steps.
            "--profile"
        } else {
            "--agent"
        };
        args.extend([flag.into(), agent.clone()]);
    }
}

fn codex_writer_supported(config: &Config) -> bool {
    version(config, Harness::Codex).is_some_and(|v| v.contains("0.153.4"))
}
fn claude_writer_supported(config: &Config) -> bool {
    version(config, Harness::Claude).is_some_and(|v| v.starts_with("2.1."))
}

fn version(config: &Config, harness: Harness) -> Option<String> {
    let output = Command::new(config.binary(harness))
        .arg("--version")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn write_jsonl(file: &mut std::fs::File, value: &serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_launch_preserves_profile_vocabulary() {
        let request = LaunchRequest {
            destination: Harness::Codex,
            model: Some("gpt-test".into()),
            agent: Some("review".into()),
            ..LaunchRequest::default()
        };
        let mut args = Vec::new();

        add_target_args(&mut args, &request);

        assert_eq!(args, ["--model", "gpt-test", "--profile", "review"]);
    }

    #[test]
    fn other_harnesses_receive_an_agent_flag() {
        let request = LaunchRequest {
            destination: Harness::Claude,
            agent: Some("reviewer".into()),
            ..LaunchRequest::default()
        };
        let mut args = Vec::new();

        add_target_args(&mut args, &request);

        assert_eq!(args, ["--agent", "reviewer"]);
    }
}
