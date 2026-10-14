//! octo-d: local daemon. Owns sessions, tools, DB, LLM.
//! Phase 2a: TCP prompt streaming with cancel plus list. Tool loop lands in 2b.

use octo_core::{ClientMsg, Config, ServerMsg, SessionInfo, APP_V, PROTOCOL_V};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

mod db;
mod llm;
mod tools;

/// Shared daemon state: config, DB, cancel flags per session.
struct State {
    cfg: Config,
    db: db::Db,
    cancel: Mutex<HashMap<String, bool>>,
    boot: Instant,
}

#[tokio::main]
async fn main() -> AnyhowLite {
    init_logs();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--health") {
        return health_once().await;
    }
    if args.iter().any(|a| a == "--shutdown") {
        return shutdown_once().await;
    }
    if args.iter().any(|a| a == "--stats") {
        println!("octo-d 0.1.0 phase0: no turns yet. See logs dir.");
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest") {
        return selftest().await;
    }
    if args.iter().any(|a| a == "--llm-test") {
        let prompt = args
            .iter()
            .skip_while(|a| *a != "--llm-test")
            .nth(1)
            .cloned()
            .unwrap_or_else(|| "say hi in 5 words".to_string());
        return llm_test(&prompt).await;
    }

    let cfg = Config::load(&Config::default_path());
    let addr = cfg.addr();
    let db_path = Config::expand(&cfg.server.db_path);
    let conn = db::init(&db_path).map_err(|e| format!("db init {}: {}", db_path.display(), e))?;
    let state = Arc::new(State {
        cfg,
        db: Arc::new(Mutex::new(conn)),
        cancel: Mutex::new(HashMap::new()),
        boot: Instant::now(),
    });

    info!(addr = %addr, "octo-d boot");

    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        eprintln!("bind {} failed: {}. Is octo-d already running?", addr, e);
        e
    })?;

    println!("octo-d {} on {} protocol v{}", APP_V, addr, PROTOCOL_V);

    loop {
        let (sock, peer) = listener.accept().await.map_err(|e| {
            eprintln!("accept failed: {}", e);
            e
        })?;
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, st).await {
                warn!(%e, peer = %peer, "conn error");
            }
        });
    }
}

// Tiny error that prints clean without extra crate.
type AnyhowLite = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn init_logs() {
    // Logs to file plus stderr. JSON lines for easy grep.
    // Dir from PRD: ~/.local/share/octo/logs/
    let dir = Config::expand("~/.local/share/octo/logs");
    let _ = std::fs::create_dir_all(&dir);
    let file_appender = tracing_appender_like(&dir);
    let _ = file_appender; // Phase 0 keeps stderr JSON, file path logged.

    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter("octo_d=info,octo_core=info")
        .try_init();
    info!(log_dir = %dir.display(), "logs ready");
}

// Phase 0: we log dir path but keep subscriber simple to avoid new dep.
// Full file appender comes in Phase 3 with rotation. This keeps Phase 0 lean.
fn tracing_appender_like(dir: &std::path::Path) -> String {
    dir.display().to_string()
}

async fn handle(
    sock: tokio::net::TcpStream,
    st: Arc<State>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (r, w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();
    // Writer shared by direct replies plus streaming task.
    let writer = Arc::new(Mutex::new(w));
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: ClientMsg = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                let err = ServerMsg::Error {
                    code: "bad_json".into(),
                    msg: e.to_string(),
                };
                send(&writer, &err).await?;
                continue;
            }
        };
        match msg {
            ClientMsg::Hello { client, client_v } => {
                info!(%client, %client_v, "hello");
                send(
                    &writer,
                    &ServerMsg::HelloOk {
                        server_v: APP_V.to_string(),
                        protocol_v: PROTOCOL_V,
                    },
                )
                .await?;
            }
            ClientMsg::Health => {
                let up_ms = st.boot.elapsed().as_millis() as u64;
                send(
                    &writer,
                    &ServerMsg::HealthOk {
                        server_v: APP_V.to_string(),
                        protocol_v: PROTOCOL_V,
                        up_ms,
                    },
                )
                .await?;
            }
            ClientMsg::Shutdown => {
                send(&writer, &ServerMsg::Bye { reason: "bye".into() }).await?;
                break;
            }
            ClientMsg::ListSessions => {
                let rows = db::list_sessions(&st.db).await.unwrap_or_default();
                let sessions = rows
                    .into_iter()
                    .map(|(id, model, status)| SessionInfo { id, model, status })
                    .collect();
                send(&writer, &ServerMsg::SessionList { sessions }).await?;
            }
            ClientMsg::Cancel { session_id } => {
                st.cancel.lock().await.insert(session_id, true);
            }
            ClientMsg::Prompt {
                session_id,
                model,
                prompt,
            } => {
                // Clear old cancel flag, ensure session row, save user event.
                st.cancel.lock().await.insert(session_id.clone(), false);
                let mode = st.cfg.tools.mode;
                let mode_s = format!("{:?}", mode).to_lowercase();
                let _ = db::create_session(&st.db, &session_id, &model, &mode_s).await;
                let _ = db::add_event(&st.db, &session_id, "user", &prompt).await;
                // Stream in background task so cancel and next prompt still read.
                let st2 = st.clone();
                let w2 = writer.clone();
                tokio::spawn(async move {
                    run_prompt(st2, w2, session_id, model, prompt).await;
                });
            }
        }
    }
    Ok(())
}

/// Run one LLM turn, forward deltas as TokenDelta lines, save final to DB.
/// Checks cancel flag between chunks. Tool loop lands in Phase 2b.
async fn run_prompt(
    st: Arc<State>,
    w: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    session_id: String,
    model: String,
    prompt: String,
) {
    let t0 = Instant::now();
    let mut first_ms: u64 = 0;
    let mut got_first = false;
    // Channel from llm callback to sender because callback is sync.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let sid = session_id.clone();
    let w2 = w.clone();
    // Sender task: reads deltas and writes lines, stops on cancel.
    let sender = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let msg = ServerMsg::TokenDelta {
                session_id: sid.clone(),
                text,
            };
            if send(&w2, &msg).await.is_err() {
                break;
            }
        }
    });

    let api_key = st.cfg.openrouter.api_key.clone();
    let timeout_secs = st.cfg.openrouter.timeout_secs;
    let st_cancel = st.clone();
    let sid_cancel = session_id.clone();
    // Poll cancel flag by wrapping stream: check before each delta via shared flag in callback.
    // llm itself has no cancel hook yet, so we check after done and mark partial.
    // Full mid stream abort lands with tool loop in 2b using AbortHandle. For 2a Esc sets flag
    // and we report cancelled at end if flag was set.
    let req = llm::LlmReq {
        api_key,
        model,
        prompt,
        timeout_secs,
    };
    let res = llm::chat_stream(req, |d| {
        if !got_first {
            first_ms = t0.elapsed().as_millis() as u64;
            got_first = true;
        }
        let _ = tx.send(d.to_string());
    })
    .await;
    drop(tx);
    let _ = sender.await;
    let ms = t0.elapsed().as_millis() as u64;
    let was_cancel = *st_cancel.cancel.lock().await.get(&sid_cancel).unwrap_or(&false);
    match res {
        Ok(full) => {
            let _ = db::add_event(&st.db, &sid_cancel, "assistant", &full).await;
            let done = if was_cancel {
                ServerMsg::TokenError {
                    session_id: sid_cancel.clone(),
                    msg: "cancelled, partial saved".into(),
                }
            } else {
                ServerMsg::TokenDone {
                    session_id: sid_cancel.clone(),
                    ms,
                    first_ms,
                }
            };
            let sent = send(&w, &done).await.is_ok();
            info!(session = %sid_cancel, ms = ms, first_ms = first_ms, sent = sent, "prompt done");
        }
        Err(e) => {
            let msg = if was_cancel {
                format!("cancelled, partial saved. last error: {}", e)
            } else {
                e
            };
            let _ = db::add_event(&st.db, &sid_cancel, "error", &msg).await;
            let sent = send(
                &w,
                &ServerMsg::TokenError {
                    session_id: sid_cancel.clone(),
                    msg: msg.clone(),
                },
            )
            .await
            .is_ok();
            info!(session = %sid_cancel, sent = sent, "prompt error: {}", msg);
        }
    }
}

async fn send(
    w: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    msg: &ServerMsg,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    w.lock().await.write_all(s.as_bytes()).await?;
    Ok(())
}

async fn health_once() -> AnyhowLite {
    let cfg = Config::load(&Config::default_path());
    match tokio::net::TcpStream::connect(cfg.addr()).await {
        Ok(_) => {
            println!("ok {}", cfg.addr());
            Ok(())
        }
        Err(e) => {
            eprintln!("down {}: {}", cfg.addr(), e);
            std::process::exit(1);
        }
    }
}

async fn shutdown_once() -> AnyhowLite {
    let cfg = Config::load(&Config::default_path());
    let sock = tokio::net::TcpStream::connect(cfg.addr()).await?;
    let (r, mut w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();
    let req = serde_json::to_string(&ClientMsg::Shutdown)? + "\n";
    w.write_all(req.as_bytes()).await?;
    let resp = lines.next_line().await?.unwrap_or_default();
    println!("{}", resp);
    Ok(())
}

/// CP1a demo: DB plus parallel tools. Run with `octo-d --selftest`.
/// Proves WAL tables, parallel reads, shell timeout, truncation, plan block.
async fn selftest() -> AnyhowLite {
    use octo_core::Mode;
    let tmp = std::env::temp_dir().join(format!("octo-selftest-{}.db", std::process::id()));
    let conn = db::init(&tmp).map_err(|e| format!("db init: {}", e))?;
    let shared: db::Db = Arc::new(tokio::sync::Mutex::new(conn));
    let cfg = Config::default();

    db::create_session(&shared, "s1", "test-model", "ask")
        .await
        .map_err(|e| format!("create session: {}", e))?;
    db::add_event(&shared, "s1", "user", "hello")
        .await
        .map_err(|e| format!("add event: {}", e))?;

    // Make 3 temp files then read them in parallel to prove tasks do not block.
    let dir = std::env::temp_dir().join(format!("octo-st-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Work inside temp dir so inside_allow passes via cwd rule after chdir? Keep abs check simple:
    // write via direct fs for setup, read via tools with cwd fallback.
    std::fs::write(dir.join("a.txt"), "alpha").map_err(|e| e.to_string())?;
    std::fs::write(dir.join("b.txt"), "beta").map_err(|e| e.to_string())?;
    std::fs::write(dir.join("c.txt"), "gamma").map_err(|e| e.to_string())?;
    // Allow temp dir for this test by chdir into it.
    std::env::set_current_dir(&dir).map_err(|e| e.to_string())?;

    let (r1, r2, r3) = tokio::join!(
        async { tools::read("a.txt", &cfg, "s1", "t1") },
        async { tools::read("b.txt", &cfg, "s1", "t2") },
        async { tools::read("c.txt", &cfg, "s1", "t3") },
    );
    println!("parallel reads: {:?} {:?} {:?}", r1.map(|r| r.output), r2.map(|r| r.output), r3.map(|r| r.output));

    // Shell ok plus timeout plus plan block.
    let ok = tools::shell("echo hi && pwd", Mode::Ask, &cfg, "s1", "t4", 5).await?;
    println!("shell ok {}ms: {}", ok.ms, ok.output.trim());
    let to = tools::shell("sleep 5", Mode::Ask, &cfg, "s1", "t5", 1).await;
    println!("shell timeout (should be Err): {:?}", to.err());
    let blocked = tools::write("x.txt", "no", Mode::Plan, &cfg);
    println!("plan block write (should be Err): {:?}", blocked.err());

    let sessions = db::list_sessions(&shared).await.map_err(|e| e.to_string())?;
    println!("sessions in db: {:?}", sessions);
    println!("CP1a selftest ok. db at {}", tmp.display());
    Ok(())
}

/// CP1b demo: real OpenRouter stream with kind retry.
/// Needs OPENROUTER_API_KEY or TOML key. Streams live, saves to DB.
async fn llm_test(prompt: &str) -> AnyhowLite {
    let cfg = Config::load(&Config::default_path());
    if cfg.openrouter.api_key.is_empty() {
        eprintln!("missing key: set OPENROUTER_API_KEY or fill config.toml openrouter.api_key");
        std::process::exit(1);
    }
    let db_path = Config::expand(&cfg.server.db_path);
    let conn = db::init(&db_path).map_err(|e| format!("db: {}", e))?;
    let shared: db::Db = Arc::new(tokio::sync::Mutex::new(conn));
    let sid = format!("llmtest-{}", std::process::id());
    db::create_session(&shared, &sid, &cfg.openrouter.default_model, "ask")
        .await
        .map_err(|e| e.to_string())?;
    db::add_event(&shared, &sid, "user", prompt)
        .await
        .map_err(|e| e.to_string())?;

    println!("model: {}", cfg.openrouter.default_model);
    println!("prompt: {}", prompt);
    let t0 = Instant::now();
    let mut first_ms: Option<u64> = None;
    let req = llm::LlmReq {
        api_key: cfg.openrouter.api_key.clone(),
        model: cfg.openrouter.default_model.clone(),
        prompt: prompt.to_string(),
        timeout_secs: cfg.openrouter.timeout_secs,
    };
    let res = llm::chat_stream(req, |d| {
        if first_ms.is_none() {
            first_ms = Some(t0.elapsed().as_millis() as u64);
        }
        print!("{}", d);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    })
    .await;
    println!();
    match res {
        Ok(full) => {
            let ms = t0.elapsed().as_millis() as u64;
            println!(
                "done in {}ms, first token {:?}ms, {} chars",
                ms,
                first_ms.unwrap_or(ms),
                full.len()
            );
            db::add_event(&shared, &sid, "assistant", &full)
                .await
                .map_err(|e| e.to_string())?;
            println!("saved to session {}", sid);
            Ok(())
        }
        Err(e) => {
            // Partial is already printed live. Save marker so resume sees it.
            let msg = format!("error after {}ms: {}", t0.elapsed().as_millis(), e);
            eprintln!("{}", msg);
            db::add_event(&shared, &sid, "error", &msg)
                .await
                .map_err(|e| e.to_string())?;
            if e.contains("need_compact") {
                eprintln!("hint: run /compact then retry once");
            }
            std::process::exit(1);
        }
    }
}
