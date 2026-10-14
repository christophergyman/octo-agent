# Octo Agent - Build Plan with Phases and Checkpoints

Owner: cman
Stack: Rust workspace `octo-core, octo-d, octo-tui`, TCP `127.0.0.1` JSON lines, SQLite WAL, TOML global, Ratatui minimal
How to use: build phase by phase. Do not skip checkpoint review. Each checkpoint is a sit down to get context, test live, and resteer if needed.

Review ritual for each checkpoint (15 to 30 min):
* Demo: run binary live, show logs and DB rows.
* Context: what files were added, what Rust ideas were used, where is the fast path.
* Stats: paint ms, first token ms, RAM MB, DB size.
* Decision: continue, adjust, or pause and learn deeper.
* Resteer options always include pros and cons.

## Phase 0 - Scaffold and hello
Goal: one repo builds two bins that talk once over TCP. No LLM yet.

Tasks:
* `Cargo.toml` workspace plus `crates/octo-core, octo-d, octo-tui`.
* `octo-core`: `Config` load from `~/.config/octo/config.toml`, `Protocol` hello types, `Tool` trait stub, version const.
* `octo-d`: bind TCP, handle `hello`, `health`, `shutdown`, log with `tracing`.
* `octo-tui`: connect, auto spawn daemon if port down, show `connected vX` and quit.
* Version check on hello. Mismatch shows clear error.
* Basic `tracing` JSON lines to `~/.local/share/octo/logs/`.

Rust you learn:
* workspaces, `lib.rs` vs `main.rs`, `serde` plus `toml`, `tokio::net::TcpListener`, `Command` spawn, `tracing`.

Done means:
* `cargo build` works on Mac and Linux.
* `octo-tui` auto starts `octo-d` and shows health.
* Kill daemon, restart, TUI reconnects.

Checkpoint CP0 - Scaffold review:
* Demo: start TUI with no daemon running, watch it spawn and connect.
* Context to learn: workspace layout, where GPUI crate will slot in later, where protocol lives so we never drift.
* Stats: binary size, cold start ms, idle RAM.
* Resteer options:
  * A: continue to Phase 1. Pros: momentum. Cons: none if hello is solid.
  * B: switch transport to Unix socket now. Pros: tiny speed gain on Mac/Linux. Cons: rework for Windows later, fights your TCP extensible choice.
  * C: add system service now. Pros: always on. Cons: OS specific pain, slows loop work. Suggest defer.

## Phase 1 - Durable loop plus tools plus OpenRouter stream
Goal: daemon can run a ReAct turn with real model and real tools, saved to SQLite.

Tasks:
* SQLite schema: `sessions, events, tool_runs, allow_rules, attempts`. WAL on, single writer channel, busy timeout 5s.
* Session manager: one tokio task per session, global semaphore `num_cpus * 2`, per session 4.
* Tools: `read, write, edit, glob, grep, shell` via `bash -c`, workdir lock, 30s group kill, 32k cap, overflow to `.octo/outputs/`.
* Modes enforced at server: `plan` blocks writes, `ask` asks, `auto` checks `workdir_allow`.
* OpenRouter client: `reqwest` SSE stream, forward deltas, buffer to DB on end. Timeouts 10s connect, 60s no data.
* Retry B kind rules: 3 retries on 429 and 5xx with 500ms, 1s, 2s plus jitter. 413 does one compact signal then one retry. Esc saves partial.
* Manual resume: mark running as interrupted on boot, list ready.

Rust you learn:
* `rusqlite` plus channels, `tokio::process`, signals and kill pgid, `tokio::select!` for cancel, SSE parsing, error with `thiserror` or simple enums.

Done means:
* CLI test can start session, prompt streams, shell runs and saves, kill -9 mid tool shows interrupted on restart with no silent double write.

Checkpoint CP1a - DB and tools review:
* Demo: run 3 parallel reads plus one shell, show `tool.start` then `tool.end` rows, show truncated output file ref.
* Context to learn: single writer queue, idempotency key `session+step`, why WAL matters, how kill group works.
* Resteer options:
  * A: keep 30s and 32k. Pros: simple, low RAM. Cons: long builds need background later.
  * B: raise timeout to 120s. Pros: fewer kills on npm. Cons: hung tasks hold slots longer.
  * C: add `run_in_background` now. Pros: long jobs survive. Cons: more code, not strict MVP. Suggest defer.

Checkpoint CP1b - LLM loop review:
* Demo: `/model` switch, stream with slow network, force 429 to see retry logs, Esc to cancel and keep partial.
* Context to learn: stream to TCP to DB flow, where tokens buffer, where retry helper lives.
* Resteer options:
  * A: keep B kind retry. Pros: robust but simple. Cons: small extra code vs dumb pipe.
  * B: drop to dumb pipe. Pros: less code. Cons: fragile long sessions. Suggest keep B.
  * C: add fallback model now. Pros: uptime. Cons: hides quality change, fights manual `/model`. Suggest defer.

## Phase 2 - Minimal TUI plus approval plus resume
Goal: thin Ratatui client that feels fast and safe.

Tasks:
* Layout: history list, approval card, input line, status bar with `paint ms | first token ms | ctx % | RAM | mode | session`.
* Commands: `/model, /mode, /resume, /compact, /quit`. Approval keys `y` once, `a` session allow, `d` deny, `p` to plan.
* TCP client: reconnect, version check, cancel on Esc, live paint at 60Hz while streaming.
* `/resume` list from DB, switch sessions, show paused interrupted state.
* Approval rules table shown in `/mode`.

Rust you learn:
* `ratatui` widgets and state, event loop, non blocking paint, key handling, client TCP codec shared from `octo-core`.

Done means:
* Type feels instant, tokens show at once, approval card blocks loop until answer, deny is logged and model sees reason.

Checkpoint CP2 - TUI review:
* Demo: plan mode blocks write live, ask mode shows `y/a/d/p`, auto mode runs inside workdir without prompt.
* Context to learn: thin client rule, server owns truth, TUI never runs tools directly. This is why GPUI later is easy.
* Stats: paint ms p50 and p95 while streaming, input lag.
* Resteer options:
  * A: keep 5 commands only. Pros: fast ship, low bug surface. Cons: less flashy.
  * B: add tabs or split now. Pros: nicer multi session. Cons: much TUI state, slows MVP. Suggest defer to GPUI.
  * C: change keys or theme. Cheap to do here, costly later. Good spot to tweak.

## Phase 3 - Compaction plus stats plus soak
Goal: realllly long sessions stay fast and keep context.

Tasks:
* Micro prune: outputs older than 4 turns and over 2k chars become placeholder with file hash ref. Full stays in DB.
* Auto at 80% est `chars/4`, plus manual `/compact`. Summary fields: objective, constraints, progress, decisions, blockers, next, files R/E/W/D, dead ends. Keep last 8k tokens exact on clean boundary.
* Re inject safety plus mode plus workdir after each compact so rules never decay.
* Cheap summary model on OpenRouter, tools off, max 4096 out. Save `checkpoints` row with `tail_from_seq`.
* Detailed stats A: per turn `model ms, first token ms, tool ms, db ms, est tokens, RAM MB`. `octo-d --stats` view.
* Soak: 4 hour run with 5 sessions, check RAM flat and paint stable.

Rust you learn:
* token estimation, checkpoint state machine, background summarize task, file refs, `tracing` spans.

Done means:
* 2 hour coding session compacts at least once, task continues without repeat, `/resume` after compact still works, RAM flat.

Checkpoint CP3 - Long session review:
* Demo: force compact on long log, show summary plus tail rebuild, show pruned placeholder vs DB full text.
* Context to learn: lossy vs durable split, why tail matters for exact paths, why progress files matter for C later.
* Resteer options:
  * A: keep B now, design ready for C. Pros: solid without vector DB. Cons: very long overnight may still forget.
  * B: add C now with `progress.md` plus git checkpoints. Pros: best marathon memory. Cons: more prompts and approvals, more code. Good post MVP add.
  * C: raise tail from 8k to 15k. Pros: more exact detail. Cons: less room for future work, higher cost. Easy tweak here.

## Phase 4 - Harden and GPUI ready
Goal: MVP done definition met, codebase ready for `octo-gpui` crate without rework.

Tasks:
* Port check: `octo-core` has no `ratatui` or server deps. TCP codec used by both sides. Add `PROTOCOL_V` test.
* Failure tests: port in use, stale daemon, version mismatch, DB locked, disk full on outputs, timeout kill.
* Perf pass: remove big clones, stream straight through, cap logs, measure binary size.
* Docs: update PRD acceptance ticks, ADR consequences, add `docs/PROTOCOL.md` from core types.
* Release: `cargo install --path .` tested on ARM Mac and x86 Linux. Note binary size and start time.

Rust you learn:
* feature flags, clean trait bounds, integration tests, release profile tuning.

Checkpoint CP4 - MVP sign off:
* Live acceptance run from PRD section 9: stream, model switch, mode toggle, resume after kill, compact continue, timeout kill, workdir block, 5 parallel sessions.
* Decision:
  * A: lock MVP, start GPUI crate. Pros: momentum to GUI.
  * B: add web search next. Pros: more useful agent. Cons: delays GPUI.
  * C: add MCP next. Pros: extensible. Cons: most complex, high RAM risk.
* Human context pack: 10 min walk of `core` types, server loop file, TUI loop file, DB schema diagram, where to add new tool in 3 steps.

## Post MVP backlog (do not start before CP4)
* Web search plus fetch with size cap and approval.
* MCP stdio client as new `Tool` impl.
* Codemode plus sandbox.
* Remote TCP with auth plus TLS.
* Windows TCP path and CI binaries.
* `octo-gpui` crate reusing `octo-core` protocol.
* Full profiles and project override if global only gets painful.

## Tracking
* Each checkpoint is a live demo plus decision here in chat: what was demoed, stats, continue or resteer.
