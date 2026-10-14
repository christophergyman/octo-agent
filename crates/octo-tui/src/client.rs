//! client: shared server message handling for interactive and headless.
//! Both paths use StreamState::apply so a done matching fix lands in both.
//! Headless `--once` is the tight loop: same match semantics, no terminal.

use octo_core::ServerMsg;

pub struct StreamState {
    pub session: String,
    pub buf: String,
    pub streaming: bool,
    pub done_ms: Option<(u64, u64)>,
    pub error: Option<String>,
    pub notice: Option<String>,
}

impl StreamState {
    pub fn new(session: &str) -> Self {
        Self {
            session: session.to_string(),
            buf: String::new(),
            streaming: false,
            done_ms: None,
            error: None,
            notice: None,
        }
    }

    /// Mark a fresh turn starting. Clears old result, opens for deltas.
    pub fn start(&mut self) {
        self.buf.clear();
        self.streaming = true;
        self.done_ms = None;
        self.error = None;
    }

    /// Returns true if state changed.
    pub fn apply(&mut self, m: &ServerMsg) -> bool {
        match m {
            ServerMsg::TokenDelta { session_id, text } => {
                if *session_id == self.session && self.streaming {
                    self.buf.push_str(text);
                    true
                } else {
                    false
                }
            }
            ServerMsg::TokenDone {
                session_id,
                ms,
                first_ms,
            } => {
                if *session_id == self.session {
                    self.streaming = false;
                    self.done_ms = Some((*ms, *first_ms));
                    true
                } else {
                    self.notice = Some(format!("done for other session {}", session_id));
                    true
                }
            }
            ServerMsg::TokenError { session_id, msg } => {
                if *session_id == self.session {
                    self.streaming = false;
                    self.error = Some(msg.clone());
                    true
                } else {
                    self.notice = Some(format!("error for other session {}: {}", session_id, msg));
                    true
                }
            }
            ServerMsg::SessionList { sessions } => {
                if sessions.is_empty() {
                    self.notice = Some("no sessions yet".into());
                } else {
                    let list = sessions
                        .iter()
                        .take(10)
                        .map(|s| format!("{} | {} | {}", s.id, s.model, s.status))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.notice = Some(list);
                }
                true
            }
            ServerMsg::Error { code, msg } => {
                self.notice = Some(format!("{}: {}", code, msg));
                if code == "conn_lost" {
                    self.streaming = false;
                }
                true
            }
            _ => false,
        }
    }
}
