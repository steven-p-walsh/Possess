# Installation

Possess runs on macOS and Linux. Install it for the same operating-system user that runs
your coding harnesses because session discovery reads their local state directories.

## Prerequisites

- [Rust 1.92 or newer](https://www.rust-lang.org/tools/install) when building from source
- Git
- At least one supported harness: Codex, Claude Code, OpenCode, or Grok

Possess does not need model-provider API keys. Each harness keeps control of its own
installation, authentication, network access, and usage limits.

## Install from source with Cargo

Clone the repository and install the locked dependency graph:

```bash
git clone https://github.com/steven-p-walsh/Possess.git
cd Possess
cargo install --path . --locked
```

Cargo places the executable in `~/.cargo/bin`. Add that directory to `PATH` if your Rust
installation did not configure it already.

For zsh, the default shell on current macOS releases:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

For bash:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

## Install a release archive

Tagged versions publish native archives on the
[GitHub Releases page](https://github.com/steven-p-walsh/Possess/releases). Download the
archive matching your operating system and architecture, then install the executable in
a directory on `PATH`:

```bash
tar -xzf possess-<system>-<architecture>.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 possess "$HOME/.local/bin/possess"
```

If `~/.local/bin` is not on `PATH`, add it to the startup file for your shell. For zsh:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

## Verify the installation

Check the installed version and inspect the detected harnesses:

```bash
possess --version
possess doctor
possess list
```

`possess doctor` reports the executable, version, session directory, and available
transfer tier for every adapter. A harness can be absent without preventing the other
adapters from working.

If a harness uses a nonstandard executable or state directory, create
`~/.config/possess/config.toml` and specify only the overrides you need:

```toml
[binaries]
codex = "/opt/homebrew/bin/codex"
claude = "/Users/me/.local/bin/claude"

[homes]
grok = "/Volumes/agent-state/grok"
```

Start the interactive session browser with:

```bash
possess
```

## Upgrade

For a source installation, update the checkout and reinstall:

```bash
git pull --ff-only
cargo install --path . --locked --force
```

For an archive installation, download the newer release and replace the existing
`possess` executable using the same `install` command shown above.

## Uninstall

Remove a Cargo installation with:

```bash
cargo uninstall possess
```

For an archive installation, remove `~/.local/bin/possess`. Possess keeps transfer
packages under `~/.local/share/possess`; uninstalling the executable leaves those private
handoffs available unless you choose to remove them separately.
