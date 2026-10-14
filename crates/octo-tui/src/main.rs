//! octo-tui: thin Ratatui client. Deep polish B.
//! Wrap, scroll, mouse toggle for copy, cmd history, /help, multiline, resize safe, theme.
//! No tools here. Only talks TCP. Approval card lands in 2b.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use octo_core::{ClientMsg, Config, ServerMsg, APP_V, PROTOCOL_V};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

mod client;
use client::StreamState;

struct Ui {
    history: Vec<(String, String)>,
    stream: StreamState,
    input: String,
    status: String,
    model: String,
    session: String,
    mode: String,
    cancel_sent: bool,
    busy_noted: bool,
    last_delta: Instant,
    first_ms: u64,
    t0: Instant,
    paint_ms: u128,
    scroll: usize,
    stick_bottom: bool,
    cmd_hist: Vec<String>,
    hist_idx: Option<usize>,
    mouse_on: bool,
    est_tokens: usize,
}

impl Ui {
    fn push(&mut self, kv: (&str, String)) {
        let (role, text) = kv;
        self.history.push((role.to_string(), text));
        // Rough token est for status: chars/4 over history plus live.
        let chars: usize = self.history.iter().map(|(_, t)| t.len()).sum::<usize>() + self.stream.buf.len();
        self.est_tokens = chars / 4;
        // Keep RAM bounded: cap history at 500 turns.
        if self.history.len() > 500 {
            self.history.drain(0..50);
        }
        self.stick_bottom = true;
        self.scroll = 0;
    }
    fn set_status(&mut self, s: String) {
        self.status = s;
    }
    fn status_line(&self, ram_mb: u64) -> String {
        // ctx % assumes 200k window until server sends real limit in Phase 3.
        let pct = (self.est_tokens as f64 / 200_000.0 * 100.0).min(99.9);
        format!(
            "{} | paint {}ms | ctx ~{}tok {:.1}% | {}MB | {} | {} | {} | scroll {} {} | m mouse {}",
            self.status,
            self.paint_ms,
            self.est_tokens,
            pct,
            ram_mb,
            self.mode,
            self.model,
            self.session,
            self.scroll,
            if self.stick_bottom { "live" } else { "frozen" },
            if self.mouse_on { "on" } else { "off(copy)" },
        )
    }
}

fn role_color(role: &str) -> Color {
    match role {
        "you" => Color::Cyan,
        "assistant" | "assistant(partial)" => Color::White,
        "sys" => Color::DarkGray,
        "error" => Color::Red,
        _ => Color::White,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--once" || a == "--list") {
        return headless(&args).await;
    }
    let cfg = Config::load(&Config::default_path());
    let addr = cfg.addr();
    if TcpStream::connect(&addr).await.is_err() {
        spawn_daemon()?;
        wait_for_port(&addr).await?;
    }
    let sock = TcpStream::connect(&addr).await?;
    let (r, w) = sock.into_split();
    let writer: Arc<Mutex<OwnedWriteHalf>> = Arc::new(Mutex::new(w));
    let mut lines = BufReader::new(r).lines();

    send(
        &writer,
        &ClientMsg::Hello {
            client: "octo-tui".into(),
            client_v: APP_V.to_string(),
        },
    )
    .await?;
    let resp = lines.next_line().await?.unwrap_or_default();
    match serde_json::from_str::<ServerMsg>(&resp)? {
        ServerMsg::HelloOk { protocol_v, .. } if protocol_v != PROTOCOL_V => {
            eprintln!("version mismatch v{} vs v{}. Rebuild both.", protocol_v, PROTOCOL_V);
            std::process::exit(1);
        }
        ServerMsg::HelloOk { .. } => {}
        other => {
            eprintln!("bad hello: {:?}", other);
            std::process::exit(1);
        }
    }

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut term = ratatui::Terminal::new(backend)?;

    let sid = format!(
        "sess-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let mut ui = Ui {
        history: vec![("sys".into(), "octo deep polish: Enter sends, Ctrl+J newline, Up recalls, PgUp scrolls, m toggles mouse for copy, /help lists.".into())],
        stream: StreamState::new(&sid),
        input: String::new(),
        status: "ready".into(),
        model: cfg.openrouter.default_model.clone(),
        session: sid,
        mode: format!("{:?}", cfg.tools.mode).to_lowercase(),
        cancel_sent: false,
        busy_noted: false,
        last_delta: Instant::now(),
        first_ms: 0,
        t0: Instant::now(),
        paint_ms: 0,
        scroll: 0,
        stick_bottom: true,
        cmd_hist: vec![],
        hist_idx: None,
        mouse_on: true,
        est_tokens: 0,
    };

    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<KeyEvent>();
    let (mouse_tx, mut mouse_rx) = tokio::sync::mpsc::unbounded_channel::<MouseEventKind>();
    let (srv_tx, mut srv_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(k)) => {
                if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                    let _ = key_tx.send(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
                    break;
                }
                let _ = key_tx.send(k);
            }
            Ok(Event::Mouse(m)) => {
                let _ = mouse_tx.send(m.kind);
            }
            Ok(Event::Resize(_, _)) => {
                // Resize is handled on next draw. Send noop via mouse channel to wake select.
                let _ = mouse_tx.send(MouseEventKind::Moved);
            }
            _ => {}
        }
    });
    let srv_tx2 = srv_tx.clone();
    // Debug log of every server line. Proves what TUI actually received.
    let tui_log = Config::expand("~/.local/share/octo/logs/octo-tui.log");
    tokio::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    log_line(&tui_log, &line);
                    match serde_json::from_str::<ServerMsg>(&line) {
                        Ok(m) => {
                            if srv_tx2.send(m).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            log_line(&tui_log, &format!("PARSE_FAIL {} : {}", e, line.chars().take(200).collect::<String>()));
                        }
                    }
                }
                // EOF or read error: reader is dead, no more server messages
                // will arrive. Tell UI so it never hangs on streaming=true.
                _ => {
                    log_line(&tui_log, "READER_EOF_OR_ERR");
                    let _ = srv_tx2.send(ServerMsg::Error {
                        code: "conn_lost".into(),
                        msg: "daemon connection lost. Daemon may have restarted. Esc resets.".into(),
                    });
                    break;
                }
            }
        }
    });

    let mut sys = sysinfo::System::new();
    let mut quit = false;
    while !quit {
        let pt0 = Instant::now();
        sys.refresh_memory();
        let ram_mb = sysinfo::get_current_pid()
            .ok()
            .and_then(|p| sys.process(p).map(|pr| (pr.memory() / 1024 / 1024) as u64))
            .unwrap_or(0);
        let status = ui.status_line(ram_mb);
        term.draw(|f| {
            let area = f.area();
            // Resize safe: min heights so small windows do not panic.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(5), Constraint::Length(1)])
                .split(area);
            // Build wrapped lines for history plus live buffer.
            let mut wrapped: Vec<Line> = vec![];
            for (role, text) in ui.history.iter().rev().take(200).rev() {
                let color = role_color(role);
                // Split long text into visual chunks by width at draw time via Wrap widget,
                // but for scroll math we keep logical lines and let Paragraph wrap.
                wrapped.push(Line::from(vec![
                    Span::styled(format!("{}: ", role), Style::default().fg(color)),
                    Span::styled(text.clone(), Style::default().fg(color)),
                ]));
            }
            if ui.stream.streaming && !ui.stream.buf.is_empty() {
                wrapped.push(Line::from(vec![
                    Span::styled("assistant(live): ", Style::default().fg(Color::Green)),
                    Span::styled(ui.stream.buf.clone(), Style::default().fg(Color::Green)),
                ]));
            }
            // Scroll: 0 means bottom. Paragraph scroll counts from top, so convert.
            let total = wrapped.len() as u16;
            let visible = chunks[0].height.saturating_sub(2) as usize;
            let max_scroll = total.saturating_sub(visible as u16) as usize;
            let off = ui.scroll.min(max_scroll);
            // Paragraph scroll expects top offset. Bottom offset = max - off when stuck? Invert.
            let top = if ui.stick_bottom { max_scroll } else { max_scroll.saturating_sub(off) };
            let hist = Paragraph::new(wrapped)
                .block(Block::default().borders(Borders::ALL).title("octo"))
                .wrap(Wrap { trim: false })
                .scroll((top as u16, 0));
            f.render_widget(hist, chunks[0]);
            let inp = Paragraph::new(ui.input.as_str())
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "{} | {} | Enter send Ctrl+J newline Up recall",
                    ui.session, ui.model
                )))
                .wrap(Wrap { trim: false });
            f.render_widget(inp, chunks[1]);
            f.render_widget(Paragraph::new(status.as_str()), chunks[2]);
        })?;
        ui.paint_ms = pt0.elapsed().as_millis();

        tokio::select! {
            // Watchdog: streaming with no delta for 120s means done was lost.
            // Reset locally so UI never hangs. DB already holds the partial.
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                if ui.stream.streaming && ui.last_delta.elapsed().as_secs() > 120 {
                    if !ui.stream.buf.is_empty() {
                        let p = std::mem::take(&mut ui.stream.buf);
                        ui.push(("assistant(partial)", p));
                    }
                    ui.stream.streaming = false;
                    ui.cancel_sent = false;
                    ui.busy_noted = false;
                    ui.set_status("watchdog: no data 120s, stream reset, see /resume".into());
                    ui.push(("sys", "watchdog reset: stream stalled, state cleared.".into()));
                }
            }
            Some(kind) = mouse_rx.recv() => {
                match kind {
                    MouseEventKind::ScrollUp => {
                        ui.stick_bottom = false;
                        ui.scroll = ui.scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        if ui.scroll <= 3 { ui.stick_bottom = true; ui.scroll = 0; }
                        else { ui.scroll -= 3; }
                    }
                    _ => {}
                }
            }
            Some(k) = key_rx.recv() => {
                match (k.code, k.modifiers) {
                    (KeyCode::Esc, _) => {
                        if ui.stream.streaming {
                            if !ui.cancel_sent {
                                let _ = send(&writer, &ClientMsg::Cancel { session_id: ui.session.clone() }).await;
                                ui.cancel_sent = true;
                                ui.set_status("cancel sent, partial will save".into());
                            } else {
                                // Second Esc: done never arrived (reconnect, dropped
                                // conn). Force local reset so UI never hangs forever.
                                // Server already saved partial to DB.
                                if !ui.stream.buf.is_empty() {
                                    let p = std::mem::take(&mut ui.stream.buf);
                                    ui.push(("assistant(partial)", p));
                                }
                                ui.stream.streaming = false;
                                ui.cancel_sent = false;
                                ui.busy_noted = false;
                                ui.set_status("stream reset locally, partial kept".into());
                            }
                        } else {
                            quit = true;
                        }
                    }
                    (KeyCode::Char('m'), m) if m.is_empty() && ui.input.is_empty() => {
                        // Toggle mouse capture for native copy. Empty input only to avoid eating typing.
                        ui.mouse_on = !ui.mouse_on;
                        if ui.mouse_on {
                            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
                        } else {
                            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
                        }
                    }
                    (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => {
                        ui.input.push('\n');
                    }
                    (KeyCode::Enter, _) => {
                        let text = ui.input.trim().to_string();
                        if text.is_empty() && ui.input.contains('\n') {
                            // Allow sending multiline with empty trim but real newlines? Require non empty.
                        }
                        if text.is_empty() { continue; }
                        ui.input.clear();
                        ui.hist_idx = None;
                        ui.stick_bottom = true; ui.scroll = 0;
                        if text == "/quit" { quit = true; continue; }
                        if text == "/help" {
                            ui.push((
                                "sys",
                                "cmds: /model [id] | /resume [id] | /mode | /compact | /quit | Enter send Ctrl+J newline Up recall PgUp scroll m mouse-copy Esc cancel".into(),
                            ));
                            continue;
                        }
                        if let Some(m) = text.strip_prefix("/model ") {
                            ui.model = m.trim().to_string();
                            ui.push(("sys", format!("model -> {}", ui.model)));
                            continue;
                        }
                        if text == "/model" {
                            ui.push(("sys", format!("model = {}", ui.model)));
                            continue;
                        }
                        if text == "/resume" {
                            let _ = send(&writer, &ClientMsg::ListSessions).await;
                            ui.set_status("fetching sessions...".into());
                            continue;
                        }
                        if let Some(id) = text.strip_prefix("/resume ") {
                            ui.session = id.trim().to_string();
                            ui.stream = StreamState::new(&ui.session);
                            ui.push(("sys", format!("session -> {}", ui.session)));
                            continue;
                        }
                        if text.starts_with("/mode") || text.starts_with("/compact") {
                            ui.push(("sys", "coming in 2b/Phase3. Server already prunes and compacts.".into()));
                            continue;
                        }
                        if text.starts_with('/') {
                            ui.push(("sys", "unknown. Try /help".into()));
                            continue;
                        }
                        if ui.stream.streaming {
                            // One turn at a time in 2a. Sending a second prompt now
                            // would interleave two streams on one session and lose
                            // the first answer. Queue lands with tool loop in 2b.
                            ui.input = text;
                            ui.set_status("busy streaming, Esc cancels first".into());
                            // Note once per stream so holding Enter does not spam.
                            if !ui.busy_noted {
                                ui.busy_noted = true;
                                ui.push((
                                    "sys",
                                    "busy: wait for stream or press Esc to cancel, your text is kept in input.".into(),
                                ));
                            }
                            continue;
                        }
                        // Only real prompts go to recall history. Slash cmds do not,
                        // so Up gives you back actual prompts, not /resume noise.
                        if ui.cmd_hist.last().map(|s| s != &text).unwrap_or(true) {
                            ui.cmd_hist.push(text.clone());
                        }
                        ui.push(("you", text.clone()));
                        ui.stream = StreamState::new(&ui.session);
                        ui.stream.start();
                        ui.cancel_sent = false;
                        ui.busy_noted = false;
                        ui.last_delta = Instant::now();
                        ui.t0 = Instant::now();
                        ui.first_ms = 0;
                        ui.set_status("streaming... Esc cancels".into());
                        let _ = send(&writer, &ClientMsg::Prompt { session_id: ui.session.clone(), model: ui.model.clone(), prompt: text }).await;
                    }
                    (KeyCode::Backspace, _) => { ui.input.pop(); }
                    (KeyCode::Up, _) => {
                        // Recall history. If empty input or at bottom, walk back.
                        if !ui.cmd_hist.is_empty() {
                            let next = match ui.hist_idx {
                                None => ui.cmd_hist.len().saturating_sub(1),
                                Some(0) => 0,
                                Some(i) => i.saturating_sub(1),
                            };
                            ui.hist_idx = Some(next);
                            ui.input = ui.cmd_hist[next].clone();
                        } else {
                            ui.stick_bottom = false;
                            ui.scroll = ui.scroll.saturating_add(1);
                        }
                    }
                    (KeyCode::Down, _) => {
                        match ui.hist_idx {
                            Some(i) if i + 1 < ui.cmd_hist.len() => {
                                let next = i + 1;
                                ui.hist_idx = Some(next);
                                ui.input = ui.cmd_hist[next].clone();
                            }
                            _ => {
                                ui.hist_idx = None;
                                // If recalling, clear to fresh. Else scroll down.
                                if ui.input.is_empty() {
                                    if ui.scroll <= 1 { ui.stick_bottom = true; ui.scroll = 0; }
                                    else { ui.scroll -= 1; }
                                } else {
                                    ui.input.clear();
                                }
                            }
                        }
                    }
                    (KeyCode::PageUp, _) => { ui.stick_bottom = false; ui.scroll = ui.scroll.saturating_add(10); }
                    (KeyCode::PageDown, _) => {
                        if ui.scroll <= 10 { ui.stick_bottom = true; ui.scroll = 0; }
                        else { ui.scroll -= 10; }
                    }
                    (KeyCode::Char(c), m) if m.is_empty() || m.contains(KeyModifiers::SHIFT) => { ui.input.push(c); }
                    _ => {}
                }
            }
            Some(m) = srv_rx.recv() => {
                // Shared match logic lives in StreamState::apply. UI mirrors
                // state into history and status. Headless uses the same apply.
                match &m {
                    ServerMsg::TokenDelta { .. } => {
                        let was = ui.stream.streaming;
                        if ui.stream.apply(&m) && was {
                            if ui.first_ms == 0 { ui.first_ms = ui.t0.elapsed().as_millis() as u64; }
                            ui.last_delta = Instant::now();
                            let chars: usize = ui.history.iter().map(|(_, t)| t.len()).sum::<usize>() + ui.stream.buf.len();
                            ui.est_tokens = chars / 4;
                            ui.set_status(format!("stream {}ms first {}ms", ui.t0.elapsed().as_millis(), ui.first_ms));
                        }
                    }
                    ServerMsg::TokenDone { .. } => {
                        let was = ui.stream.streaming;
                        ui.stream.apply(&m);
                        if !ui.stream.streaming && was {
                            if let Some((ms, first_ms)) = ui.stream.done_ms {
                                let full = std::mem::take(&mut ui.stream.buf);
                                ui.push(("assistant", full));
                                ui.cancel_sent = false;
                                ui.busy_noted = false;
                                ui.set_status(format!("done {}ms first {}ms", ms, first_ms));
                            }
                        } else if let Some(n) = ui.stream.notice.take() {
                            ui.push(("sys", n));
                        }
                    }
                    ServerMsg::TokenError { .. } => {
                        let was = ui.stream.streaming;
                        ui.stream.apply(&m);
                        if !ui.stream.streaming && was {
                            if !ui.stream.buf.is_empty() {
                                let p = std::mem::take(&mut ui.stream.buf);
                                ui.push(("assistant(partial)", p));
                            }
                            ui.cancel_sent = false;
                            ui.busy_noted = false;
                            let msg = ui.stream.error.clone().unwrap_or_default();
                            ui.set_status(format!("error: {}", msg));
                            ui.push(("sys", format!("error: {}", msg)));
                        } else if let Some(n) = ui.stream.notice.take() {
                            ui.push(("sys", n));
                        }
                    }
                    ServerMsg::SessionList { .. } => {
                        ui.stream.apply(&m);
                        if let Some(n) = ui.stream.notice.take() {
                            for line in n.lines().take(10) {
                                ui.push(("sys", line.to_string()));
                            }
                            ui.set_status("use /resume <id> to switch".into());
                        }
                    }
                    ServerMsg::Error { code, .. } => {
                        ui.stream.apply(&m);
                        if code == "conn_lost" {
                            // Reader dead: no more server messages coming on this
                            // conn. Unstick streaming so UI never hangs. Server DB
                            // already holds whatever completed.
                            ui.cancel_sent = false;
                            ui.busy_noted = false;
                        }
                        if let Some(n) = ui.stream.notice.take() {
                            ui.push(("sys", n));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    let _ = crossterm::execute!(
        term.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    );
    println!("bye {} {}", ui.session, ui.model);
    Ok(())
}

async fn send(
    w: &Arc<Mutex<OwnedWriteHalf>>,
    msg: &ClientMsg,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    w.lock().await.write_all(s.as_bytes()).await?;
    Ok(())
}

fn spawn_daemon() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cur = std::env::current_exe().unwrap_or("octo-tui".into());
    let next = cur.parent().map(|p| p.join("octo-d"));
    let prog = match next {
        Some(p) if p.exists() => p.to_string_lossy().to_string(),
        _ => "octo-d".to_string(),
    };
    let log = Config::expand("~/.local/share/octo/logs/octo-d.out");
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::File::create(&log)?;
    let _ = std::process::Command::new(prog)
        .stdout(file.try_clone()?)
        .stderr(file)
        .spawn()?;
    println!("spawned octo-d");
    Ok(())
}

async fn wait_for_port(addr: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for _ in 0..50 {
        if TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err("daemon did not come up in 5s".into())
}

#[allow(dead_code)]
fn _keep(_: OwnedReadHalf) {}

/// Headless mode for scripts and for agent testing without a terminal.
/// Same TCP hello plus prompt plus StreamState match as interactive.
/// Usage: octo-tui --once "prompt" [--model ID] [--session ID] [--timeout SECS]
///        octo-tui --list
/// Exit 0 on done, 1 on error, 2 on timeout. Deltas go to stdout as they arrive.
async fn headless(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::io::Write;
    fn flag(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    }

    let cfg = Config::load(&Config::default_path());
    let addr = cfg.addr();
    if TcpStream::connect(&addr).await.is_err() {
        spawn_daemon()?;
        wait_for_port(&addr).await?;
    }
    let sock = TcpStream::connect(&addr).await?;
    let (r, w) = sock.into_split();
    let writer: Arc<Mutex<OwnedWriteHalf>> = Arc::new(Mutex::new(w));
    let mut lines = BufReader::new(r).lines();

    send(&writer, &ClientMsg::Hello {
        client: "octo-tui-headless".into(),
        client_v: APP_V.to_string(),
    })
    .await?;
    let resp = lines.next_line().await?.unwrap_or_default();
    match serde_json::from_str::<ServerMsg>(&resp)? {
        ServerMsg::HelloOk { protocol_v, .. } if protocol_v != PROTOCOL_V => {
            eprintln!("version mismatch v{} vs v{}. Rebuild both.", protocol_v, PROTOCOL_V);
            std::process::exit(1);
        }
        ServerMsg::HelloOk { .. } => {}
        other => {
            eprintln!("bad hello: {:?}", other);
            std::process::exit(1);
        }
    }

    if args.iter().any(|a| a == "--list") {
        send(&writer, &ClientMsg::ListSessions).await?;
        let line = lines.next_line().await?.unwrap_or_default();
        match serde_json::from_str::<ServerMsg>(&line)? {
            ServerMsg::SessionList { sessions } => {
                for s in &sessions {
                    println!("{} | {} | {}", s.id, s.model, s.status);
                }
                eprintln!("{} sessions", sessions.len());
            }
            other => {
                eprintln!("unexpected: {:?}", other);
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let prompt = flag(args, "--once").unwrap_or_default();
    if prompt.is_empty() {
        eprintln!("usage: octo-tui --once \"prompt\" [--model ID] [--session ID] [--timeout SECS]");
        std::process::exit(1);
    }
    let model = flag(args, "--model").unwrap_or_else(|| cfg.openrouter.default_model.clone());
    let session = flag(args, "--session").unwrap_or_else(|| {
        format!(
            "headless-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        )
    });
    let timeout: u64 = flag(args, "--timeout").and_then(|s| s.parse().ok()).unwrap_or(120);

    send(&writer, &ClientMsg::Prompt {
        session_id: session.clone(),
        model: model.clone(),
        prompt: prompt.clone(),
    })
    .await?;

    let mut st = StreamState::new(&session);
    st.start();
    let t0 = Instant::now();
    let mut first_ms: Option<u64> = None;
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                let _ = send(&writer, &ClientMsg::Cancel { session_id: session.clone() }).await;
                eprintln!("timeout {}s, cancel sent, session {}", timeout, session);
                std::process::exit(2);
            }
            res = lines.next_line() => {
                let line = match res? {
                    Some(l) => l,
                    None => {
                        eprintln!("connection closed, session {}", session);
                        std::process::exit(1);
                    }
                };
                if line.trim().is_empty() { continue; }
                let msg: ServerMsg = match serde_json::from_str(&line) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("parse fail {}: {}", e, line.chars().take(150).collect::<String>());
                        continue;
                    }
                };
                // Same shared apply as interactive. Print deltas live.
                if let ServerMsg::TokenDelta { session_id, text } = &msg {
                    if *session_id == session && st.streaming {
                        if first_ms.is_none() {
                            first_ms = Some(t0.elapsed().as_millis() as u64);
                        }
                    }
                }
                let was = st.streaming;
                st.apply(&msg);
                if let ServerMsg::TokenDelta { session_id, text } = msg {
                    if session_id == session && was {
                        print!("{}", text);
                        let _ = std::io::stdout().flush();
                    }
                } else if !st.streaming && was {
                    println!();
                    if let Some((ms, f)) = st.done_ms {
                        eprintln!("done in {}ms first {}ms session {} model {}", ms, f, session, model);
                        return Ok(());
                    }
                    if let Some(e) = st.error.clone() {
                        eprintln!("error session {}: {}", session, e);
                        std::process::exit(1);
                    }
                } else if let Some(n) = st.notice.take() {
                    eprintln!("note: {}", n);
                }
            }
        }
    }
}

/// Append one line to the TUI debug log. Best effort, never fails the UI.
fn log_line(path: &std::path::PathBuf, line: &str) {
    use std::io::Write;
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let short: String = line.chars().take(300).collect();
        let _ = writeln!(f, "{}", short);
    }
}
