//! These fixtures exercise the real CLI against isolated stores. A recording harness
//! catches accidental forks or extra invocations without spending provider usage.

#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CLAUDE_ID: &str = "11111111-1111-4111-8111-111111111111";

struct Fixture {
    temporary: tempfile::TempDir,
    project: PathBuf,
    config_home: PathBuf,
    data_dir: PathBuf,
    transcript: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path();
        let project = base.join("project");
        let other = base.join("project-other");
        for path in [&project, &other] {
            fs::create_dir_all(path.join("src")).unwrap();
            let output = Command::new("git")
                .args(["init", "--quiet"])
                .arg(path)
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        let config_home = base.join("config");
        let config_dir = config_home.join("possess");
        fs::create_dir_all(&config_dir).unwrap();
        let data_dir = base.join("possess-data");
        let harness = base.join("recording-harness");
        fs::write(&harness, "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$POSSESS_TEST_ARGS\"\npwd > \"$POSSESS_TEST_CWD\"\n").unwrap();
        fs::set_permissions(&harness, fs::Permissions::from_mode(0o755)).unwrap();
        let homes: serde_json::Map<String, Value> = ["codex", "claude", "opencode", "grok"]
            .into_iter()
            .map(|name| (name.into(), json!(base.join(name))))
            .collect();
        let binaries: serde_json::Map<String, Value> = ["codex", "claude", "opencode", "grok"]
            .into_iter()
            .map(|name| (name.into(), json!(harness)))
            .collect();
        fs::write(
            config_dir.join("config.toml"),
            toml::to_string(&json!({"homes": homes, "binaries": binaries})).unwrap(),
        )
        .unwrap();

        let store = base.join("claude/projects/fixture");
        fs::create_dir_all(&store).unwrap();
        let transcript = store.join(format!("{CLAUDE_ID}.jsonl"));
        for (path, cwd) in [
            (&transcript, &project),
            (&store.join("other-session.jsonl"), &other),
        ] {
            let record =
                json!({"type": "user", "cwd": cwd, "message": {"content": "Fixture task"}});
            fs::write(path, format!("{record}\n")).unwrap();
        }
        let grok_store = base.join("grok/sessions/grok-session");
        fs::create_dir_all(&grok_store).unwrap();
        fs::write(
            grok_store.join("summary.json"),
            json!({
                "info": {"id": "grok-session", "cwd": project},
                "generated_title": "Grok fixture"
            })
            .to_string(),
        )
        .unwrap();
        Self {
            temporary,
            project,
            config_home,
            data_dir,
            transcript,
        }
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_possess"))
            .args(args)
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("POSSESS_HOME", &self.data_dir)
            .env("POSSESS_TEST_ARGS", self.temporary.path().join("args"))
            .env("POSSESS_TEST_CWD", self.temporary.path().join("cwd"))
            .output()
            .unwrap()
    }

    fn json(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn list_defaults_to_the_launch_project_and_all_projects_is_explicit() {
    let fixture = Fixture::new();
    let source = fixture.project.join("src");
    let scoped = fixture.json(&source, &["list", "--json"]);
    assert_eq!(scoped.as_array().unwrap().len(), 2);
    assert!(
        scoped
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["cwd"] == json!(fixture.project))
    );
    let claude = fixture.json(&source, &["list", "--harness", "claude", "--json"]);
    assert_eq!(claude.as_array().unwrap().len(), 1);
    assert_eq!(claude[0]["vendor_id"], CLAUDE_ID);
    for args in [
        ["--all-projects", "list", "--json"],
        ["list", "--all-projects", "--json"],
    ] {
        assert_eq!(fixture.json(&source, &args).as_array().unwrap().len(), 3);
    }
    let foreign = fixture.run(&source, &["show", "claude:other-session"]);
    assert!(!foreign.status.success());
    assert!(String::from_utf8_lossy(&foreign.stderr).contains("--all-projects"));
}

#[test]
fn choosing_claude_for_a_claude_session_only_invokes_native_resume() {
    let fixture = Fixture::new();
    let original = fs::read(&fixture.transcript).unwrap();
    fs::write(&fixture.data_dir, "handoff writes are blocked").unwrap();
    let id = format!("claude:{CLAUDE_ID}");
    let output = fixture.run(&fixture.project, &["handoff", &id, "--to", "claude"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.temporary.path().join("args")).unwrap(),
        format!("--resume\n{CLAUDE_ID}\n")
    );
    let launched_cwd = fs::read_to_string(fixture.temporary.path().join("cwd")).unwrap();
    assert_eq!(
        Path::new(launched_cwd.trim()).canonicalize().unwrap(),
        fixture.project.canonicalize().unwrap()
    );
    assert_eq!(fs::read(&fixture.transcript).unwrap(), original);
    assert_eq!(
        fs::read_to_string(&fixture.data_dir).unwrap(),
        "handoff writes are blocked"
    );

    let prepared = fixture.json(
        &fixture.project,
        &["handoff", &id, "--to", "claude", "--no-launch", "--json"],
    );
    assert_eq!(prepared["fidelity"], "native_resume");
    assert_eq!(prepared["destination_session_id"], CLAUDE_ID);
    assert!(prepared["package"].is_null());
    assert!(prepared["handoff_id"].is_null());
    // A second dry-run command must not invoke even the recording harness.
    assert_eq!(
        fs::read_to_string(fixture.temporary.path().join("args")).unwrap(),
        format!("--resume\n{CLAUDE_ID}\n")
    );
}

#[test]
fn changing_harnesses_still_creates_the_handoff_package() {
    let fixture = Fixture::new();
    let original = fs::read(&fixture.transcript).unwrap();
    let id = format!("claude:{CLAUDE_ID}");
    let result = fixture.json(
        &fixture.project,
        &["handoff", &id, "--to", "grok", "--no-launch", "--json"],
    );
    assert_eq!(result["fidelity"], "smart_bootstrap");
    let package = Path::new(result["package"].as_str().unwrap());
    assert!(package.join("manifest.json").is_file());
    assert!(package.join("session.json").is_file());
    assert!(package.join("handoff.md").is_file());
    assert_eq!(fs::read(&fixture.transcript).unwrap(), original);
    assert!(!fixture.temporary.path().join("args").exists());
}
