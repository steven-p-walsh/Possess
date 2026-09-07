# Compatibility research

This document records the assumptions behind the adapters. It distinguishes supported CLI
interfaces from local file formats because only the former carry a vendor compatibility
promise.

The initial implementation was validated on September 7, 2026 with Codex `0.153.4`,
Claude Code `2.1.263`, OpenCode `1.2.15`, and Grok `1.0.13`. Possess probes the installed
binary each time it starts; these versions are evidence for the current gates, not a claim
that later versions use the same private schema.

| Harness | Discovery and normalization | Same-harness path | Cross-harness path |
| --- | --- | --- | --- |
| Codex | read-only `state_5.sqlite`, streaming rollout JSONL fallback | `codex resume` | writer only on `0.153.4`, then bootstrap |
| Claude Code | bounded metadata tail, streaming project JSONL | `--resume` | writer on `2.1.x`, then bootstrap |
| OpenCode | read-only SQLite transaction | `--session` | official `opencode import` |
| Grok | `summary.json`, streaming chat or ACP updates | `--resume` | interactive bootstrap |

Since Possess 0.1.1, choosing the source harness resumes the original session ID.
It skips history loading, Git capture, package creation, and import. The TUI also
skips model and agent selection; explicit CLI overrides are passed to the resume
command. Private format compatibility gates apply only when changing harnesses.

## Codex

Codex documents `resume` and `fork` as stable commands. Possess uses `resume` for
same-harness work so it keeps the existing session ID. It treats the local database
and rollout layout as implementation details.

On `0.153.4`, `state_5.sqlite` provides titles, paths, workspace metadata, model, and
recency. Rollouts contain JSONL `session_meta`, `response_item`, and `event_msg` records.
Possess prefers persisted response messages over duplicate UI events and excludes injected
environment or policy messages from portable user history. The cross-harness writer is
locked to this exact version because both the rollout and index row must agree.

Sources: [Codex developer commands](https://developers.openai.com/codex/cli/reference),
[Codex CLI](https://developers.openai.com/codex/cli/features).

## Claude Code

Claude documents resuming by ID and exposes `--fork-session`, `--model`, and `--agent` in
its CLI. Local project transcripts are linked JSONL records under the Claude configuration
directory. Possess keeps visible messages, tool calls/results, compaction summaries, and
attachments. Signed thinking blocks are counted as skipped because their integrity data is
provider-private and cannot be recreated for another harness.

The compatible writer emits a minimal linked transcript only for the observed `2.1.x`
family. A failed write falls back to a bootstrap and never changes the source transcript.

Source: [Claude Code CLI reference](https://docs.anthropic.com/en/docs/claude-code/cli-usage).

### Claude model discovery

The installed CLI has no standalone `models` command. Possess builds an offline list
from user and project settings (`model`, `availableModels`, and `modelOverrides`),
model-related environment variables, and model IDs in the 100 most recent session
summaries. Fable, Opus, Sonnet, and Haiku remain as fallback aliases. The wizard also
accepts a typed model ID and refreshes discovered choices when the session list is
refreshed.

These are local suggestions, not a live account-entitlement check or a reproduction of
Claude's policy settings. Claude applies its own availability and organization rules at
launch. Possess does not query model APIs or invoke credential helpers for this list.

Source: [Claude model configuration](https://code.claude.com/docs/en/model-config).

## OpenCode

OpenCode documents JSON export/import and the `--session --fork`, `--model`, and `--agent`
flags. That makes its importer the preferred cross-harness boundary. Possess reads local
SQLite inside one read transaction so messages and parts come from the same WAL moment.
Model choices are inferred from recent local messages: opening the wizard should not
authenticate or refresh a provider catalog.

OpenCode’s importer requires the destination project to exist. Possess first asks the
OpenCode CLI to resolve the workspace, then uses the Git root commit as the export’s project
identity. If either supported command fails, the prepared launch falls back to the smart
prompt and leaves the package intact.

Source: [OpenCode CLI reference](https://opencode.ai/docs/cli/).

## Grok

Grok documents session resume/fork, model selection, agents, exports, ACP, and a Claude
import command. The installed `1.0.13` binary used during implementation did not expose
`import`, so version strings alone are insufficient. The documented importer is also
Claude-specific rather than a general portable-session API. Possess uses an interactive
initial prompt for cross-harness transfers and reports whether that native importer exists
for diagnostic purposes.

Session summaries supply inexpensive picker metadata. `chat_history.jsonl` is preferred for
model-facing messages and tool calls; `updates.jsonl` remains the ACP fallback for clients
that do not persist chat history.

Sources: [Grok CLI reference](https://docs.x.ai/build/cli/reference),
[Grok Build overview](https://docs.x.ai/build/overview).

## Changing a compatibility gate

A private-writer gate should move only when a sanitized fixture covers the new vendor
version and a real CLI can list and resume the generated session. A successful parse proves
only that the reader tolerates the new data. It does not prove that the destination accepts
records created by Possess.
