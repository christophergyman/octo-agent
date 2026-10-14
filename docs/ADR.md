# Octo Agent - ADR / Architecture (MVP + path to GPUI)

Status: accepted for MVP
Date: 2026-10-14
Owner: cman

## 1. Context
We need a lightning fast agent harness in Rust, TUI now and GPUI soon. Beginner Rust. Goals: low footprint, durable long sessions, local client server, strict MVP that can add web, MCP, codemode later.

## 2. Decisions summary
* Workspace with 3 crates: `octo-core`, `octo-d`, `octo-tui`. Future `octo-gpui` reuses core.
* Stack: `tokio`, `ratatui`, `rusqlite` with WAL, `reqwest` SSE, `serde` plus `toml`, `tracing`.
* Transport: TCP `127.0.0.1` with newline JSON. Same protocol for all OS and future remote. Unix socket rejected for MVP to keep Windows path open, HTTP SSE rejected to save RAM and code.
* Store: SQLite single file, single writer queue, event sourced. Ready for marathon C.
* Tools: `bash -c`, workdir lock, 30s group kill, 32k cap, CPU sized semaphore, Tool trait for future.
* Config: TOML global only.
* Compaction B, retry B, manual resume, auto start daemon, light stats.

## 3. Workspace layout
```
Cargo.toml # workspace
crates/
  octo-core/ # types, protocol, config, DB schema, Tool trait, compaction types, no IO loop
    src/lib.rs
  octo-d/ # TCP server, session manager, tool exec, openrouter client, sqlite writer
    src/main.rs
  octo-tui/ # ratatui client only
    src/main.rs
docs/PRD.md
docs/ADR.md
```

Why:
* Considered single crate with 2 bins. Rejected: mixes UI and server, painful GPUI port.
* Considered 2 repos. Rejected: version drift.
* Workspace costs one hour now, saves weeks later. Build stays fast, tiny crates.

## 4. Runtime and async
* `tokio` multi thread default. Each session is a task. Each tool is a child task under per session and global semaphore.
* Global semaphore size `num_cpus * 2`. Per session max 4. Not a product cap, a safety valve against fork bomb and OOM.
* Cancel via `tokio::select!` on TCP cancel message and abort handle for reqwest stream. Esc always wins.
* Learn path: explicit tasks, channels, no magic executors.

Alternatives: `async-std`, `smol`. Rejected: tokio has best Unix process, TCP, and tracing ecosystem for beginner examples.

## 5. Protocol (TCP JSON lines)
* Bind `127.0.0.1:PORT` from TOML, default 7531. No remote bind in MVP.
* Hello: `{v:1, client:octo-tui, client_v:0.1.0}` server checks major match, else error `version_mismatch`.
* Messages, one JSON per line, `seq` per session:
  * `c->s: session.start, session.resume, prompt, cancel, approval{y/a/d}, compact`
  * `s->c: token.delta, tool.start, tool.end, approval.request, session.paused, error.retrying, stats`
* Streaming: OpenRouter SSE deltas mapped to `token.delta` at once, no batch delay. TUI paints on each line, target 60fps diff.
* Why TCP not Unix: Unix is faster on paper but Windows path is odd. Local TCP is under 1ms, LLM is seconds, so loss is noise. Same messages can later run over Unix on Mac/Linux plus TCP on Windows with no logic change.
* Why not HTTP SSE: more headers, port auth still needed, more RAM. Custom TCP lines are smaller and simpler to trace in Rust for beginner.

Future: add TLS plus token auth when remote is enabled. Add Unix fast path on Mac/Linux reusing same codec. Add MessagePack only if JSON proves slow.

## 6. Data and durability
* SQLite WAL, `busy_timeout 5000ms`, `synchronous NORMAL`, single writer task via `mpsc` channel. Readers via pool.
* Tables:
  * `sessions(id, created_at, model, mode, status)` status `active|paused|interrupted|done`.
  * `events(seq INTEGER PRIMARY KEY, session_id, type, payload JSON, status, created_at)` types `user, assistant.delta.final, tool.start, tool.end, approval, checkpoint, compact, error`.
  * `tool_runs(id, session_id, step_id, tool, args JSON, status, exit_code, ms, output_path, output_hash)` idempotency key `session_id+step_id`.
  * `checkpoints(id, session_id, from_seq, to_seq, summary JSON, tail_from_seq, created_at)`.
  * `output_files(hash, path, size, truncated)` under `.octo/outputs/`.
  * `allow_rules(session_id, tool, pattern, created_at)`.
  * `attempts(session_id, step_id, n, last_error)` for retry.
* Write rule: insert `tool.start` before spawn, update to `tool.end` after. On boot mark `running` as `interrupted`. Manual resume replays from last checkpoint plus tail.
* Why SQLite over JSONL: transactions, atomic step commit, resume without double apply. JSONL considered, rejected for crash mid write risk. Postgres considered, rejected as heavy for laptop MVP.
* Ready for C marathon: add `progress.md` writer and git commit hook without schema change, just new event types.

## 7. Tools
* Trait in core:
```rust
pub trait Tool { fn name(&self)->&str; fn needs_approval(&self, mode:Mode)->bool; }
```
* Impl: `read, write, edit, glob, grep, shell`. Shell uses `bash -c`, `current_dir=workdir`, kill pgid on 30s via `nix`.
* Parallel: join tools in one model turn, capped by semaphores.
* Safety: server enforces mode. Plan blocks write and mutating shell. Ask prompts. Auto checks `workdir_allow` from TOML then prompts if outside. All decisions logged as `approval` events.
* Output: cap 32k chars in prompt, full to file plus hash. Large outputs referenced by path so compaction can prune to placeholder.
* Considered shell per user login shell. Rejected: non portable and hard to secure for beginner.

## 8. LLM
* OpenRouter OpenAI compat via `reqwest` streaming. Per session `model` id from `/model`. Default from TOML.
* Timeouts: connect 10s, no data 60s. Retries: 429 and 5xx 3 times backoff 500ms, 1s, 2s plus jitter. 413 does one auto compact then one retry. No retry on cancel or 4xx other than 429.
* Partial on cancel saved as `assistant.delta.final` with `cancelled:true` for resume context.
* Considered fallback model router. Rejected for MVP: fights explicit `/model` and hides quality change. Schema has `attempts` so fallback slots in later.
* Key: env wins, else TOML. Never logged. File 600.

## 9. TUI
* `ratatui` immediate diff render. Layout: history list, approval card overlay, input, status bar.
* No tabs in MVP. Multi session via `/resume` switch. GPUI later adds tabs reusing same TCP calls.
* Paint budget: only rerender on new line or tick 60Hz while streaming. Measure paint ms via tracing.
* Keys: Enter send, Esc cancel, y/a/d/p on approval, `/` commands.

## 10. Config
* TOML at `~/.config/octo/config.toml`. Example in PRD. Global only for MVP.
* Considered YAML and JSON. Rejected: YAML typing bugs and indent bugs waste beginner time, JSON has no comments. TOML matches Cargo and serde.
* Considered project override. Rejected for MVP to avoid surprise rules. Add later with narrow only rule.

## 11. Compaction
* Micro prune each turn: outputs older than 4 turns and over 2k chars become `[old output cleared -> file hash]`.
* Auto at 80% est `tokens ~ chars/4` of model limit minus output reserve, plus manual `/compact`. Summary via cheap model on OpenRouter, tools off, max 4096 out. Fields: objective, constraints, progress, decisions, blockers, next, files touched with R/E/W/D, errors dead ends.
* Tail: last 8k tokens exact, cut only on clean turn boundary with no open tool call.
* Safety re inject: after each compact, server prepends mode, workdir allow, and safety rules so governance does not decay.
* Full log never deleted. Checkpoint plus tail rebuilds prompt. This matches Claude Code micro plus auto plus OpenCode checkpoint pattern.

## 12. Lifecycle and distribution
* Dev: two terminals. MVP ship: `octo-tui` auto spawns `octo-d` if health check fails, waits, connects. Close leaves daemon. `--shutdown` stops.
* Data: `~/.local/share/octo/octo.db`, logs `logs/`, outputs `.octo/outputs/` per repo plus server copy by hash.
* Install MVP: `cargo install --path .`. Targets ARM Mac and x86 Linux. Version check on hello prevents drift.
* Considered launchd/systemd now. Rejected: OS specific pain, defer to post MVP.

## 13. Observability
* `tracing` JSON lines. Spans: `llm.stream, tool.run, db.write, tui.paint`.
* Status bar shows paint ms, first token ms, ctx percent, RAM MB.
* Soak rule: 4 hour run with 5 sessions must show flat RAM and no slower paint. Block merge on regression.

## 14. Security (local MVP)
* Bind only loopback. No remote. Workdir jail for auto mode. Plan blocks writes. Denied tools logged. Key never in logs or DB.
* Future remote needs token auth plus TLS plus per session sandbox. Not in MVP.

## 15. Consequences and next steps
* Fast path is clear: core has no UI deps, so `octo-gpui` can be added as 4th crate with same TCP client code.
* Risks: TCP port clash, rough token est, bash only. Mitigations: TOML port, version check, 80% early trigger, allow list.
* Build order: core types plus config, daemon hello plus DB, loop plus tools, TUI plus approval, compact plus retry, stats plus soak.
