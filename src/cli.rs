//! Scriptable surface for the operations offered by the TUI.
//!
//! JSON output lets compatibility checks run without scraping a human-oriented screen.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "possess",
    version,
    about = "Repossess coding-agent sessions across harnesses"
)]
pub struct Cli {
    /// Include sessions from every project instead of the launch directory's project.
    #[arg(long, global = true)]
    pub all_projects: bool,
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// List this project's sessions across all installed harnesses.
    List {
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one normalized session.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Switch harnesses, or resume directly when the destination is the source harness.
    Handoff {
        id: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 16_000)]
        context_tokens: usize,
        #[arg(long)]
        no_launch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Resume an existing session in its original harness.
    Resume { id: String },
    /// Check harness binaries, stores, and compatibility.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Show registered adapters and their capabilities.
    Adapters {
        #[arg(long)]
        json: bool,
    },
}
