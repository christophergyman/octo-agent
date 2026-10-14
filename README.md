# octo-agent

Lightning fast Rust agent harness. Many arms, small brain footprint.

`octo` runs coding sessions that stay fast for hours, survive crashes, and keep RAM flat. TUI now. GPUI next. Same core, same protocol.

## Why octo

* Fast feel: stream at once, paint at 60fps, cancel with Esc, tools in parallel.
* Low footprint: tiny native binaries, no JS runtime, stream straight through, prune early.
* Durable: every step in SQLite WAL. Kill -9 mid tool and `/resume` continues with no silent double apply.
* Long sessions: micro prune plus summary plus exact tail. Full log stays for audit.
* Thin clients: `octo-d` owns truth. `octo-tui` now and `octo-gpui` later just talk TCP JSON lines.

Think octopus: one head server, many arms sessions, each arm works on its own but shares the same brain protocol.

## Status

MVP in build. Mac plus Linux, local only. See `docs/PRD.md`, `docs/ADR.md`, `docs/PLAN.md`.

```
TUI (ratatui) --TCP 127.0.0.1 JSON lines--> daemon
                                                    |-- sessions (tokio tasks)
                                                    |-- tools (bash, files)
                                                    |-- OpenRouter stream
                                                    |-- SQLite WAL + checkpoints
GPUI (later) --same protocol -----------> same daemon
```

## Quickstart (MVP target)

Requirements: Rust stable, Mac ARM or Linux x86.

```bash
cargo install --path .
mkdir -p ~/.config/octo ~/.local/share/octo
cp examples/config.toml ~/.config/octo/config.toml
export OPENROUTER_API_KEY="..."
octo-tui
```

First run auto starts `octo-d`, checks version, connects. Close TUI and daemon stays alive for resume.

```bash
octo-d --health
octo-d --stats
octo-d --shutdown
```

## Commands

Minimal set on purpose.

* `/model` pick OpenRouter model per session
* `/mode` show plan, ask, auto plus session allow rules
* `/resume` list and resume past sessions
* `/compact` shrink context now, keep tail exact
* `/quit` leave, daemon keeps running

Approval while running:

* `y` allow once
* `a` allow this tool for this session
* `d` deny with reason
* `p` switch to plan mode
* `Esc` cancel stream, partial saved

## Config

Global only for MVP. Env wins over file.

Path: `~/.config/octo/config.toml`

```toml
[server]
host = "127.0.0.1"
port = 7531
db_path = "~/.local/share/octo/octo.db"

[openrouter]
api_key = ""
default_model = "anthropic/claude-sonnet-4"
timeout_secs = 60

[tools]
mode = "ask"
workdir_allow = ["~/code"]
shell_timeout_secs = 30

[tui]
theme = "dark"
```

Modes:

* `plan`: read only. Writes and mutating shell blocked at server.
* `ask`: write and shell pause for approval.
* `auto`: auto inside `workdir_allow`, else ask.

## How long sessions work

1. Every turn saved as events in SQLite.
2. Old tool outputs pruned to `[old output cleared -> file hash]` after 4 turns. Full text stays on disk under `.octo/outputs/`.
3. At 80 percent context or `/compact`, server writes a structured summary plus keeps last 8k tokens exact.
4. Safety rules re injected after each compact so plan and allow list never decay.
5. Crash marks running as interrupted. You resume manually. No surprise reruns.

## Performance rules

No hard caps as product goals. Instead:

* Stream with no big buffers.
* Truncate tool output at 32k in prompt.
* One DB writer queue, readers never block.
* Semaphore sized by CPU so a bad agent cannot fork bomb you.
* Status bar shows `paint ms | first token ms | ctx % | RAM`.
* Block merge on regression: slower paint or growing RAM over 4 hours.

## Build phases

See `docs/PLAN.md` for tasks plus review stops.

* Phase 0 scaffold and TCP hello, stop CP0
* Phase 1 durable loop plus tools plus stream, stops CP1a and CP1b
* Phase 2 minimal TUI plus approval, stop CP2
* Phase 3 compaction plus stats plus soak, stop CP3
* Phase 4 harden and GPUI ready, stop CP4 sign off

Each stop has a live demo, files changed, Rust ideas learned, stats, and resteer options.

## Post MVP

Strict MVP first. Next in line: web search, MCP, codemode, remote with auth, Windows path, `octo-gpui` crate. `Tool` trait in `octo-core` makes adds small.

## Learn Rust with it

You do not need Rust before. Build order teaches it:

* ownership via config and protocol types
* async via tokio tasks and channels
* process via tool spawn and kill group
* error via simple enums
* UI via ratatui state loop

Ask for the 10 min context pack at any checkpoint: core types, server loop file, TUI loop file, schema sketch, how to add a tool in 3 steps.

## Docs

* `docs/PRD.md` what we build and why
* `docs/ADR.md` why workspace, TCP, SQLite, compaction choices
* `docs/PLAN.md` phases plus checkpoints
* `docs/REVIEWS.md` log of each review (created at first checkpoint)
