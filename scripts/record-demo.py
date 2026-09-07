#!/usr/bin/env python3
"""Record the real TUI with fictional stores; requires Pillow and pyte.

Run `cargo build --locked`, then `python3 scripts/record-demo.py`.
No desktop capture, real harness commands, credentials, or personal stores are used.
"""

import argparse
import codecs
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import sqlite3
import struct
import subprocess
import tempfile
import termios
import time

from PIL import Image, ImageDraw, ImageFont
import pyte


ROOT = Path(__file__).resolve().parents[1]
COLS, ROWS = 160, 36
CELL_W, CELL_H = 9, 20
PAD, TOP, BOTTOM = 22, 54, 54
BG = "#13101d"
COLORS = {
    "black": "#13101d", "red": "#ef7188", "green": "#8bd5aa",
    "brown": "#e7c588", "blue": "#85b7f4", "magenta": "#bb9aff",
    "cyan": "#80d8ed", "white": "#e8e2f3", "brightblack": "#847d95",
    "brightred": "#ff8da1", "brightgreen": "#a5ebc1",
    "brightbrown": "#ffe0a3", "brightblue": "#aacfff",
    "brightmagenta": "#d1b8ff", "brightcyan": "#a3eeff",
    "brightwhite": "#ffffff",
}


def write_json(path, value, lines=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(json.dumps(row) + "\n" for row in value)
        if lines else json.dumps(value), encoding="utf-8"
    )


def fixtures(base):
    project = base / "stargazer"
    project.mkdir()
    (project / "README.md").write_text("# Stargazer\nA fictional night-sky planner.\n")
    subprocess.run(["git", "init", "--quiet", str(project)], check=True)
    names = ("codex", "claude", "opencode", "grok")
    for name in names:
        (base / name).mkdir()
    config = base / "config/possess/config.toml"
    config.parent.mkdir(parents=True)
    # Override every store and executable explicitly; never inherit harness settings.
    config.write_text(
        "reduced_motion = true\n[homes]\n"
        + "".join(f'{name} = {json.dumps(str(base / name))}\n' for name in names)
        + "[binaries]\n"
        + "".join(f'{name} = "/usr/bin/true"\n' for name in names)
    )
    now = int(time.time())
    sessions = [
        ("codex", "star-map", "Build the star map", "gpt-5", 3,
         "The interactive sky map is working. Constellation labels now avoid overlaps. Next: add keyboard navigation and a focused-star tooltip."),
        ("claude", "night-mode", "Polish night mode", "sonnet", 18,
         "Use a dim red palette to preserve night vision. The theme toggle is done; check contrast on the observation cards next."),
        ("opencode", "weather", "Cache weather forecasts", "default", 52,
         "Forecast caching is in place with a 30-minute TTL. Next: show a friendly offline state when the weather service is unavailable."),
        ("grok", "meteor", "Plan meteor alerts", "grok-4", 130,
         "The alert scheduler handles local time zones. Next: add quiet hours and a preview of the next meteor shower notification."),
        ("claude", "observations", "Export observation notes", "sonnet", 360,
         "Markdown export keeps timestamps and telescope settings. Add a sample observation and check the download filename."),
        ("codex", "setup", "Scaffold the planner", "gpt-5", 1440,
         "The starter app has a sky map, observation log, and forecast panel. Routing and the first smoke tests are ready."),
    ]
    with sqlite3.connect(base / "codex/state_5.sqlite") as db:
        db.execute("CREATE TABLE threads (id, rollout_path, created_at_ms, updated_at_ms, cwd, title, preview, model, agent_nickname, cli_version, tokens_used, archived, recency_at_ms)")
        for harness, sid, title, model, minutes, preview in sessions:
            if harness != "codex":
                continue
            stamp = (now - minutes * 60) * 1000
            rollout = base / f"codex/sessions/{sid}.jsonl"
            write_json(rollout, [
                {"type": "session_meta", "payload": {"id": sid, "cwd": str(project)}},
                {"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": title}]}},
                {"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": preview}]}},
            ], lines=True)
            db.execute("INSERT INTO threads VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
                       (sid, str(rollout), stamp, stamp, str(project), title, preview, model, None, "demo", 1200, 0, stamp))
    for harness, sid, title, model, minutes, preview in sessions:
        stamp = now - minutes * 60
        if harness == "claude":
            transcript = base / f"claude/projects/stargazer/{sid}.jsonl"
            write_json(transcript, [
                {"type": "user", "cwd": str(project), "aiTitle": title, "message": {"content": title}},
                {"type": "assistant", "cwd": str(project), "message": {"model": model, "content": preview}},
                {"type": "user", "cwd": str(project), "message": {"content": preview}},
            ], lines=True)
            os.utime(transcript, (stamp, stamp))
        elif harness == "grok":
            write_json(base / f"grok/sessions/{sid}/summary.json", {
                "info": {"id": sid, "cwd": str(project)}, "generated_title": title,
                "last_turn_summary": preview, "current_model_id": model,
            })
            os.utime(base / f"grok/sessions/{sid}/summary.json", (stamp, stamp))
        elif harness == "opencode":
            with sqlite3.connect(base / "opencode/opencode.db") as db:
                db.executescript("CREATE TABLE session (id, directory, title, version, time_created, time_updated, time_archived); CREATE TABLE message (id, session_id, data, time_created); CREATE TABLE part (id, message_id, data, time_created);")
                db.execute("INSERT INTO session VALUES (?,?,?,?,?,?,?)", (sid, str(project), title, "demo", stamp * 1000, stamp * 1000, None))
                db.execute("INSERT INTO message VALUES (?,?,?,?)", ("demo-message", sid, json.dumps({"role": "assistant", "modelID": model}), stamp * 1000))
                db.execute("INSERT INTO part VALUES (?,?,?,?)", ("demo-part", "demo-message", json.dumps({"type": "text", "text": preview}), stamp * 1000))
    # A minimal environment also excludes API keys and personal shell configuration.
    env = {"PATH": "/usr/bin:/bin", "TERM": "xterm-256color", "LANG": "en_US.UTF-8",
           "XDG_CONFIG_HOME": str(base / "config"), "POSSESS_HOME": str(base / "data")}
    return project, env


def color(value, default):
    if value == "default":
        return default
    return COLORS.get(value, "#" + value)


def render(screen, caption, font):
    width, height = COLS * CELL_W + PAD * 2, TOP + ROWS * CELL_H + BOTTOM
    frame = Image.new("RGB", (width, height), BG)
    draw = ImageDraw.Draw(frame)
    draw.rectangle((0, 0, width, 37), fill="#211b30")
    for x, fill in [(22, "#ef7188"), (42, "#e7c588"), (62, "#8bd5aa")]:
        draw.ellipse((x, 14, x + 9, 23), fill=fill)
    draw.text((width // 2, 10), "possess  /  stargazer", font=font, fill="#c8beda", anchor="mt")
    for row in range(ROWS):
        for col in range(COLS):
            cell = screen.buffer[row][col]
            fg, bg = color(cell.fg, "#e8e2f3"), color(cell.bg, BG)
            if cell.reverse:
                fg, bg = bg, fg
            x, y = PAD + col * CELL_W, TOP + row * CELL_H
            if bg != BG:
                draw.rectangle((x, y, x + CELL_W - 1, y + CELL_H - 1), fill=bg)
            if cell.data.strip():
                # Terminal block graphics must fill the cell without font side bearings.
                if cell.data in ("█", "▄", "▀"):
                    y0 = y + CELL_H // 2 if cell.data == "▄" else y
                    y1 = y + CELL_H // 2 - 1 if cell.data == "▀" else y + CELL_H - 1
                    draw.rectangle((x, y0, x + CELL_W - 1, y1), fill=fg)
                elif cell.data in "─│┌┐└┘":
                    cx, cy = x + CELL_W // 2, y + CELL_H // 2
                    if cell.data in "─┐┘":
                        draw.line((x, cy, cx, cy), fill=fg)
                    if cell.data in "─┌└":
                        draw.line((cx, cy, x + CELL_W - 1, cy), fill=fg)
                    if cell.data in "│└┘":
                        draw.line((cx, y, cx, cy), fill=fg)
                    if cell.data in "│┌┐":
                        draw.line((cx, cy, cx, y + CELL_H - 1), fill=fg)
                else:
                    draw.text((x, y), cell.data, font=font, fill=fg,
                              stroke_width=0)
    draw.line((PAD, height - BOTTOM + 8, width - PAD, height - BOTTOM + 8), fill="#433a5f")
    draw.text((PAD, height - 30), caption, font=font, fill="#c6b1ff")
    return frame


def record(binary, project, env, font):
    listing = json.loads(subprocess.check_output([str(binary), "list", "--json"], cwd=project, env=env))
    assert len(listing) == 6 and {item["harness"] for item in listing} == {"codex", "claude", "opencode", "grok"}
    assert all(Path(item["cwd"]) == project for item in listing)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    process = subprocess.Popen([str(binary)], cwd=project, env=env, stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    frames, checkpoints = [], []

    def drain(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], max(0, deadline - time.monotonic()))[0]:
                data = os.read(master, 65536)
                if not data:
                    raise RuntimeError("Demo app exited early")
                stream.feed(decoder.decode(data))

    def stage(keys, seconds, caption, expected):
        if keys:
            os.write(master, keys)
        drain(0.25)
        visible = "\n".join(screen.display)
        assert expected in visible, f"Missing {expected!r}:\n{visible}"
        assert "/Users/" not in visible and "/home/" not in visible
        checkpoints.append((caption, visible))
        # Hold settled, actual terminal states long enough to read at README size.
        frame = render(screen, caption, font)
        frames.append((frame, round(seconds * 1000)))

    try:
        stage(b"", 2.4, "01  Browse sessions from all four coding agents", "6 sessions")
        stage(b"j", 1.8, "02  Preview where each conversation left off", "Use a dim red palette")
        stage(b"j", 1.8, "02  Preview where each conversation left off", "Forecast caching")
        stage(b"/", 0.4, "03  Press / to find the session you need", "/█")
        for query in "tooltip":
            stage(query.encode(), 0.18, "03  Press / to find the session you need", "filter:")
        stage(b"\r", 1.8, "03  Find your star-map session", "1 sessions")
        stage(b"\r", 1.3, "04  Press Enter to choose where to continue", "POSSESS SESSION")
        stage(b"j", 1.8, "04  Move the Codex conversation to Claude Code", "1 Destination")
        stage(b"\r", 0.6, "05  Choose a model", "Sonnet")
        default_row = next(i for i, line in enumerate(screen.display) if "Claude default" in line)
        sonnet_row = next(i for i, line in enumerate(screen.display) if "Sonnet" in line)
        stage(b"j" * (sonnet_row - default_row), 1.8, "05  Choose a model", "Sonnet")
        stage(b"\r", 1.5, "06  Choose an agent", "Claude default agent")
        stage(b"\r", 3.4, "07  Review the handoff before launching", "Model: Sonnet")
        stage(b"\x1b", 0.5, "Your context, ready for its next home", "LAST KNOWN STATE")
        stage(b"/\x15\r", 1.7, "Your context, ready for its next home", "6 sessions")
        os.write(master, b"q")
        process.wait(timeout=5)
        assert process.returncode == 0
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        os.close(master)
    return frames, checkpoints


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/possess")
    parser.add_argument("--output", type=Path, default=ROOT / "docs/assets/possess-demo.gif")
    parser.add_argument("--font", type=Path)
    parser.add_argument("--qa-dir", type=Path, help="Optional terminal text and PNG checkpoints")
    args = parser.parse_args()
    candidates = [args.font] if args.font else [Path("/System/Library/Fonts/Menlo.ttc"), Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")]
    font_path = next((p for p in candidates if p.is_file()), None)
    if font_path is None:
        parser.error("Pass --font with a monospace TTF or TTC font")
    font = ImageFont.truetype(str(font_path), 14)
    with tempfile.TemporaryDirectory(prefix="possess-demo-", dir="/tmp") as temporary:
        project, env = fixtures(Path(temporary))
        frames, checkpoints = record(args.binary.resolve(), project, env, font)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    images = [frame.quantize(colors=128) for frame, _ in frames]
    images[0].save(args.output, save_all=True, append_images=images[1:],
                   duration=[duration for _, duration in frames], loop=0, optimize=True, disposal=1)
    if args.qa_dir:
        args.qa_dir.mkdir(parents=True, exist_ok=True)
        for i, ((frame, _), (caption, visible)) in enumerate(zip(frames, checkpoints)):
            frame.save(args.qa_dir / f"{i:02}.png")
            (args.qa_dir / f"{i:02}.txt").write_text(caption + "\n" + visible)
    print(f"Saved {args.output} ({args.output.stat().st_size:,} bytes, {sum(d for _, d in frames) / 1000:.1f}s)")


if __name__ == "__main__":
    main()
