# README demo

`possess-demo.gif` records the real Possess terminal UI browsing six fictional
Stargazer sessions, searching, and selecting Claude Code and Sonnet for a handoff.
It stops at confirmation; no coding agent is launched.

To regenerate on macOS or Linux:

```sh
cargo build --locked
python3 -m venv /tmp/possess-demo-venv
/tmp/possess-demo-venv/bin/pip install Pillow pyte
/tmp/possess-demo-venv/bin/python scripts/record-demo.py
```

The recorder creates a temporary Git project and synthetic session stores for
all four harnesses. Every store and executable is overridden in its private
config, and the child receives a minimal environment without API keys. It captures
only the app's pseudo-terminal output, never the desktop or personal sessions.
The temporary project and stores are removed when recording finishes.

Frames are rendered with Pillow and pyte, with captions added below the terminal.
Pass `--font /path/to/monospace.ttf` to choose a font, or `--qa-dir /tmp/possess-demo-qa`
to save PNG and text checkpoints for inspection. The recording checks the fixture
session count, project paths, and expected UI states before saving the GIF.
