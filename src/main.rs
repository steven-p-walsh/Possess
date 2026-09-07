//! Possess keeps the user in control of cross-harness continuity. The binary owns
//! orchestration and presentation; vendor assumptions stay behind adapters so format
//! drift cannot silently spread through the application.

mod adapters;
mod cli;
mod config;
mod domain;
mod engine;
mod git_state;
mod handoff;
mod launcher;
mod project;
mod sensitivity;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands};
use config::Config;
use domain::{Harness, LaunchRequest, PortableEvent, Role};
use engine::Engine;
use project::SessionScope;

fn main() {
    if let Err(error) = run() {
        eprintln!("possess: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load()?;
    let scope = SessionScope::from_launch_directory(cli.all_projects)?;
    let engine = Engine::new(config, scope);
    match cli.command {
        None => tui::run(&engine),
        Some(Commands::List { harness, json }) => {
            let harness = harness.map(|v| v.parse::<Harness>()).transpose()?;
            let sessions: Vec<_> = engine
                .scan()
                .into_iter()
                .filter(|s| harness.is_none_or(|h| s.harness == h))
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for session in sessions {
                    println!(
                        "{:<48} {:<13} {:<24} {}",
                        session.qualified_id,
                        session.harness,
                        session
                            .cwd
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        session.title
                    );
                }
            }
            Ok(())
        }
        Some(Commands::Show { id, json }) => {
            let summary = engine.find(&id)?;
            let session = engine.load(&summary)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&session)?);
            } else {
                println!(
                    "{}\n{} • {}\n",
                    session.source.title,
                    session.source.harness,
                    session.source.cwd.display()
                );
                for event in session.events {
                    match event {
                        PortableEvent::Message { role, text, .. } => println!(
                            "{}:\n{}\n",
                            if role == Role::User {
                                "USER"
                            } else {
                                "ASSISTANT"
                            },
                            text
                        ),
                        PortableEvent::ToolCall { name, .. } => println!("[tool: {name}]"),
                        PortableEvent::ToolResult { is_error, .. } => {
                            println!("[tool result{}]", if is_error { ": error" } else { "" })
                        }
                        PortableEvent::Summary { text, .. } => println!("SUMMARY:\n{text}\n"),
                        PortableEvent::Error { text, .. } => println!("ERROR:\n{text}\n"),
                    }
                }
            }
            Ok(())
        }
        Some(Commands::Handoff {
            id,
            to,
            model,
            agent,
            context_tokens,
            no_launch,
            json,
        }) => {
            let summary = engine.find(&id)?;
            let request = LaunchRequest {
                destination: to.parse()?,
                model,
                agent,
                context_tokens,
                launch: !no_launch,
            };
            let transfer = engine.transfer(&summary, &request)?;
            if let Some(package) = &transfer.package {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "handoff_id": package.id, "package": package.dir, "fidelity": transfer.prepared.fidelity, "destination_session_id": transfer.prepared.destination_session_id, "sensitivity": package.findings })
                    );
                } else {
                    println!(
                        "Possessed {} → {} ({})",
                        summary.harness, request.destination, transfer.prepared.fidelity
                    );
                    println!("Package: {}", package.dir.display());
                    if !package.findings.is_empty() {
                        println!(
                            "Sensitivity scan: {} potential finding group(s)",
                            package.findings.len()
                        );
                    }
                    println!("{}", transfer.prepared.detail);
                }
            } else if json {
                println!(
                    "{}",
                    serde_json::json!({ "handoff_id": null, "package": null, "fidelity": transfer.prepared.fidelity, "destination_session_id": transfer.prepared.destination_session_id, "sensitivity": [] })
                );
            } else if request.launch {
                println!("Resuming {} in {}", summary.vendor_id, summary.harness);
            } else {
                println!(
                    "Ready to resume {} in {}",
                    summary.vendor_id, summary.harness
                );
            }
            if request.launch {
                let status = launcher::launch(&transfer.prepared)
                    .context("failed to start the destination")?;
                if !status.success() {
                    anyhow::bail!("destination exited with {status}");
                }
            }
            Ok(())
        }
        Some(Commands::Resume { id }) => {
            let summary = engine.find(&id)?;
            let prepared = launcher::native_resume(&engine.config, &summary, None);
            let status = launcher::launch(&prepared)?;
            if !status.success() {
                anyhow::bail!("{} exited with {status}", summary.harness);
            }
            Ok(())
        }
        Some(Commands::Doctor { json }) | Some(Commands::Adapters { json }) => {
            let statuses = engine.statuses();
            if json {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else {
                for status in statuses {
                    let state = if status.binary.is_some() && status.store_exists {
                        "ready"
                    } else if status.binary.is_some() {
                        "no sessions"
                    } else {
                        "missing"
                    };
                    println!(
                        "{} {:<12} {:<12} {}",
                        status.harness.icon(),
                        status.harness,
                        state,
                        status.version.as_deref().unwrap_or("binary not found")
                    );
                    println!("  {}\n  {}", status.store.display(), status.detail);
                }
            }
            Ok(())
        }
    }
}
