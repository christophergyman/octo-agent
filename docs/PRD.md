# Octo Agent - PRD (MVP)

Status: draft
Owner: cman
Date: 2026-10-14
Target: Mac + Linux, local only, Rust beginner friendly, blazing fast base for GPUI later

## 1. Problem
Current agent harnesses feel slow and heavy. Long sessions grow RAM, lose context, die on crash, and mix UI with core logic. We need a small native base that stays fast over hours and can swap TUI for GPUI without rework.

## 2. User
Primary: cman, solo dev, new to Rust, wants to learn as we build.
Use: coding tasks in a repo, long sessions, many tool calls, switch models per session.

## 3. Goals
* ReAct coding loop: prompt, stream, tool call, observe, repeat until done.
* Always fast, always low RAM. No hard numbers for MVP, but measure each change and fix regressions.
* Durable: crash safe via SQLite, manual resume works.
* Long sessions keep up via smart compaction.
* Thin clients over local server so TUI now and GPUI later share same core.
* Simple Rust: small crates, explicit async, easy to learn.

## 4. Non goals (MVP out)
* No MCP client.
* No web fetch or search.
* No subagents.
* No remote server, no auth system.
* No Windows. Mac + Linux only.
* No embeddings or vector DB.
* No GPUI yet, but design for it.
* Post MVP: web search, MCP, codemode, remote, Windows, profiles.

## 5. Functional - MVP in
* Loop: single agent ReAct, parallel tool calls up to CPU based safety valve, sequential model turns.
* Provider: OpenRouter OpenAI compat only. `/model` per session. Default from TOML, example `anthropic/claude-sonnet-4`. Key from env `OPENROUTER_API_KEY` wins, else TOML field.
* Tools: read, write/edit, glob, grep, shell. Shell via `bash -c` in project workdir. Timeout 30s kill process group. Output cap 32k chars, full saved to DB plus overflow file in `.octo/outputs/`.
* Modes: plan, ask, auto with workdir allow.
  * plan: read only. Write and mutating shell blocked at server.
  * ask: write and shell pause for approval.
  * auto: auto if path inside `workdir_allow`, else ask.
* Approval: per call `y` once, `a` allow tool for session, `d` deny with reason, `p` switch to plan. Rules saved per session in DB, shown via `/mode`.
* TUI: minimal chat only. Chat stream pane, bottom input, status bar. Commands: `/model` `/mode` `/resume` `/compact` `/quit`. `Esc` cancels current stream. Streaming deltas live. Partial text saved on cancel.
* Client server: `octo-d` daemon owns loop, tools, DB, LLM. `octo-tui` is thin. Talk over TCP `127.0.0.1` with JSON lines. Same message protocol for future GPUI and Windows. No product session cap, backpressure via semaphore sized by CPU and single DB writer queue.
* Sessions: SQLite WAL, single writer channel, busy timeout 5s. Event log with `seq, session_id, type, payload, status`. Manual resume only. On boot mark running as interrupted. `/resume` lists and resumes. No auto rerun.
* Compaction B: micro prune plus summary plus tail. Prune old tool outputs older than 4 turns over 2k chars to placeholder with file ref. Auto at 80% context est via `chars/4`, plus manual `/compact`. Summary has objective, details, completed, blockers, next, files. Keep last 8k tokens exact. Full log stays for audit. DB ready for C marathon with checkpoints and output files.
* Retry B: stream forward live, buffer to DB on end. Retry 429 and 5xx 3 times with 500ms, 1s, 2s plus jitter. Timeout 60s no data. 413 triggers one compact then one retry. Esc always wins and saves partial.
* Config: TOML global only. Path `~/.config/octo/config.toml`, perms 600. Env wins over file. Sections: server, openrouter, tools, tui. Project override out for MVP.
* Lifecycle: TUI auto starts daemon if port down, waits for health, reuses if up. Closing TUI leaves daemon running. `octo-d --shutdown` stops. Data in `~/.local/share/octo/octo.db` and logs.
* Stats A: detailed light stats. Per turn log model latency, first token ms, tool ms, DB ms, est tokens, RAM MB. TUI status bar shows paint ms, first token ms, RAM. Logs JSON lines. Rule: no paint slowdown and no RAM growth over 4 hours.

## 6. UX sketch
* Top: chat history with user, assistant stream, tool calls collapsed with status.
* Bottom: input line with mode hint, like `[ask|sonnet] >`.
* Status: `paint 8ms | first token 700ms | ctx 45% | session abc123 | mode ask`.
* Keys: Enter send, Esc cancel, `/` command, `y/a/d/p` on approval card, `q` quit.
* No tabs, no popups, no dashboard in MVP.

## 7. Performance principles
* No fixed caps as goals. Build with backpressure.
* Fast paths: no big buffers, stream straight through, truncate early, keep only active context in RAM.
* Measure: `tracing` spans, stats file, 4 hour soak check.
* Block merge on regression: slower paint, higher idle RAM, growing RAM over time.

## 8. Rollout
* Phase 0: workspace + core types + TOML + TCP hello.
* Phase 1: daemon loop + OpenRouter stream + 4 tools + SQLite log.
* Phase 2: TUI minimal + approval + resume + compact + stats.
* Phase 3: harden retry, prune, soak test, then plan GPUI crate `octo-gpui`.
* Install MVP: `cargo install --path .`. Targets: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`.

## 9. Acceptance for MVP done
* `octo-tui` opens fast, auto starts daemon, chat streams.
* `/model` switches, `/mode` toggles, `/resume` restores after kill, `/compact` shrinks context but task continues.
* Shell timeout kills, workdir block asks, plan blocks writes.
* 5 sessions in parallel stay responsive, RAM flat.
* Kill -9 daemon mid tool, restart shows interrupted, no duplicate write without log.

## 10. Open risks
* Plan mode exact read only shell list: allow `ls, git status, cat` or block all shell. Default now is allow reads, block mutating.
* Token est `chars/4` is rough. Fine for 80% trigger, revisit with tokenizer later.
* TCP localhost port conflicts. Default `127.0.0.1:7531` from TOML, version check on hello.
