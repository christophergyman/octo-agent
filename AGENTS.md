# AGENTS.md - octo-agent

Rust agent harness. TUI now, GPUI later. Read `docs/PRD.md` then `docs/ADR.md` then `docs/PLAN.md` before changing architecture.

## Build and test (all offline except live)

```bash
cargo build
cargo test          # must pass, includes token_done_roundtrips
./target/debug/octo-d --selftest   # DB plus parallel tools, no key needed
```

## Run it

```bash
./run.sh   # builds, ensures daemon, opens TUI. Daemon stays alive after quit.
./target/debug/octo-d --health     # daemon check
./target/debug/octo-tui --list     # sessions from SQLite, no terminal needed
```

## Headless (how agents test streaming without a terminal)

```bash
./target/debug/octo-tui --once "prompt" --session test-1 --timeout 60
./target/debug/octo-tui --once "prompt" --model deepseek/deepseek-v4.1-flash
```

* Exit 0 done with timing on stderr, 1 error, 2 timeout with cancel sent.
* Headless and interactive share `crates/octo-tui/src/client.rs` StreamState.
  Fix match logic there and both paths get it. Never fork them.
* Live `--once` calls real OpenRouter and spends cents. Prefer mock or existing
  sessions for repeated checks. One live run per change max unless debugging.

## Debug without asking the human to repro

* Daemon log: `/tmp/octo-d.log` when started via run.sh. Look for `prompt done`
  lines with session plus ms plus sent flag.
* TUI log: `~/.local/share/octo/logs/octo-tui.log` has every raw server line
  plus parse fails. First place to look when done never lands.
* DB: `~/.local/share/octo/octo.db`, table `events`. Query by session_id to
  separate server finished (row present) from client dropped it (row missing).

```bash
sqlite3 ~/.local/share/octo/octo.db \
  "SELECT type, substr(payload,1,60), status FROM events WHERE session_id='sess-...' ORDER BY seq;"
```

## Keys (strict)

* Real key lives only in `~/.config/octo/config.toml` (mode 600, outside repo).
* Never print, paste, grep, or commit a key. Scan diffs for `sk-or` before commit.
* `examples/config.toml` keeps `api_key = ""`. Env `OPENROUTER_API_KEY` wins
  over file when set, so unset it when testing file based auth.

## Protocol rules (learned from real bugs)

* Bump `PROTOCOL_V` in octo-core on any breaking JSON change. Mismatched
  builds fail fast with a clear message instead of hanging.
* No `u128` or `i128` in any protocol type. This serde_json setup rejects u128
  on deserialize, which once hung every stream silently. Timings are `u64`.
* One JSON object per line over TCP. Newlines inside strings must stay escaped
  by serde_json, never hand build JSON with raw text.
* Daemon owns truth: sessions, tools, DB, LLM. TUI never runs tools directly.

## Rust norms here

* Small explicit crates, learn friendly. Comment the why on tricky async bits.
* No new deps without asking. Prefer std plus tokio plus serde.
* Keep `cargo build` warning free for new code. Existing dead code warnings
  for not yet wired tools are known and go away in 2b.
* Mac has no `timeout` or `setsid`. Keep scripts portable: nohup, built in flags.
