# Possess

Repossess your coding session when a harness runs out of usage, gets stuck, or needs a
different model's perspective.

Possess is a local-first TUI that discovers sessions from Codex, Claude Code, OpenCode,
and Grok, then carries the useful state into another harness. The original session is
never changed.

```text
       ▄████▄
     ▄█ ◉  ◉ █▄     P O S S E S S
    █     ▄    █    repossess your context
    █  ▄████▄  █
     ▀█▄▀  ▀▄█▀
```

## What transfers

Possess keeps two representations for different jobs:

- A private, immutable package preserves the normalized conversation, tool calls and
  results, plans, attachments, source metadata, and Git state. Sanitized source records
  are content-addressed and compressed so repeat handoffs do not duplicate large files.
- A smart handoff prompt prioritizes summaries, active todos, unresolved errors, Git
  state, files touched, and the newest useful turns. This avoids spending a destination
  model's whole context window on stale tool output.

The destination is selected using the strongest available integration:

| Destination | Preferred path | Fallback |
| --- | --- | --- |
| Codex | native fork or version-gated rollout | smart bootstrap |
| Claude Code | native fork or version-gated transcript | smart bootstrap |
| OpenCode | first-party JSON import | smart bootstrap |
| Grok | native fork | initial-prompt bootstrap |

Private writers are intentionally narrow. An unknown harness version falls back instead
of creating a native-looking session that the harness cannot safely resume.

The reasoning behind those tiers and the formats observed during implementation is in
[Compatibility research](docs/compatibility.md). The package lifecycle and trust
boundaries are in [Architecture](docs/architecture.md).

## Install

Rust 1.92 or newer is recommended while the project is under active development.

```bash
cargo install --path .
possess doctor
possess
```

Possess currently targets macOS and Linux. Possess itself does not require API keys or call
model APIs; invoked harnesses continue to own their authentication and network behavior.

Tagged releases build native macOS and Linux archives through
[the release workflow](.github/workflows/release.yml).

## TUI

The main screen combines sessions from every installed harness. Select a session to see
its source, workspace, latest state, and the fidelity available for each destination.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move between sessions |
| `PageUp` / `PageDown` | Jump ten sessions |
| `g` / `G` | First / last session |
| `/` | Fuzzy-search title, project, ID, path, and harness; `Ctrl+U` clears |
| `Enter` | Choose a destination and repossess the session |
| `r` | Resume in the original harness |
| `R` | Rescan every harness store |
| `?` | Show help |
| `q`, `Ctrl+C` | Quit |

Possess suspends its screen while the destination runs in the same terminal. When that
harness exits, the unified list returns and refreshes.

## CLI

The CLI uses the same adapters and transfer engine as the TUI.

```bash
# List everything, or one harness
possess list
possess list --harness claude --json

# Inspect normalized history without changing anything
possess show claude:9c50e430-fc70-4d54-b0fa-5014b9e779d2

# Create and launch a handoff
possess handoff <qualified-id> --to codex
possess handoff <qualified-id> --to grok --model grok-4.6
possess handoff <qualified-id> --to claude --agent reviewer

# Prepare a destination and package without launching its TUI
possess handoff <qualified-id> --to opencode --no-launch --json

# Open the untouched original session
possess resume <qualified-id>

# Inspect paths, versions, and available integration tiers
possess doctor --json
possess adapters
```

Session IDs are qualified as `harness:vendor-id`. An unqualified unique ID or prefix is
accepted by commands; ambiguous prefixes are rejected.

## Data and privacy

State defaults to `~/.local/share/possess`, or `POSSESS_HOME` when set:

```text
handoffs/<uuid>/
  manifest.json       provenance, hashes, compatibility, sensitivity counts
  session.json        complete portable session
  handoff.md          context sent to a bootstrap destination
  opencode-session.json  generated only for an OpenCode destination
blobs/<prefix>/<sha256>.zst
  deduplicated sanitized source snapshots
```

Directories use owner-only mode `0700` and files use `0600` on Unix. Possess excludes
authentication stores, vendor system prompts, encrypted reasoning, permission grants,
and lock files. The sensitivity scan reports only categories and counts; it never prints
matched secret values.

Git capture includes repository identity, branch, HEAD, worktree status, textual staged
and unstaged diffs, and untracked paths. Untracked file contents are not copied. Workspace
files stay where they are and remain the destination agent's source of truth.

## Configuration

Create `~/.config/possess/config.toml` only when defaults need changing:

```toml
context_tokens = 16000
reduced_motion = false
ascii_only = false

[binaries]
codex = "/opt/homebrew/bin/codex"
claude = "/Users/me/.local/bin/claude"

[homes]
grok = "/Volumes/agent-state/grok"
```

Possess also honors `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `GROK_HOME`, `XDG_DATA_HOME`,
`XDG_CONFIG_HOME`, and `POSSESS_HOME`.

## Why the code is structured this way

Each harness adapter owns only discovery and normalization. Destination writers are kept
separate and gated because readers can safely tolerate new fields while writers cannot
safely guess new invariants. The `PortableSessionV1` boundary keeps the TUI, CLI,
packaging, and future adapters independent of vendor event names.

Large histories are streamed only after selection. Startup reads Codex/OpenCode indexes
and bounded Claude/Grok metadata, so a multi-hundred-megabyte rollout does not delay the
first screen.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

Adapter changes should include sanitized fixtures for the vendor version they claim to
support. Never widen a private-writer version gate based only on a successful parse.
See [CONTRIBUTING.md](CONTRIBUTING.md) for the project’s comment and compatibility rules.
