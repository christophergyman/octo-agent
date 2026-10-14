//! octo-core: shared types for daemon and clients.
//! No UI deps here. No server loop here. Only data plus codec.
//! GPUI later reuses this crate with no changes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// Bump when JSON lines change in a breaking way.
// TUI checks this on hello and fails fast if major differs.
pub const PROTOCOL_V: u32 = 3;
pub const APP_V: &str = env!("CARGO_PKG_VERSION");

/// Modes from PRD: plan blocks writes, ask prompts, auto checks allow list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Plan,
    Ask,
    Auto,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Ask
    }
}

/// Global config. File only, env wins at load time in daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub openrouter: OpenRouterConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub tui: TuiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub db_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterConfig {
    pub api_key: String,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    pub mode: Mode,
    #[serde(default)]
    pub workdir_allow: Vec<String>,
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_model() -> String {
    "deepseek/deepseek-v4.1-flash".to_string()
}
fn default_timeout() -> u64 {
    60
}
fn default_shell_timeout() -> u64 {
    30
}
fn default_theme() -> String {
    "dark".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 7531,
            db_path: "~/.local/share/octo/octo.db".to_string(),
        }
    }
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            default_model: default_model(),
            timeout_secs: default_timeout(),
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Ask,
            workdir_allow: vec!["~/code".to_string()],
            shell_timeout_secs: default_shell_timeout(),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            openrouter: OpenRouterConfig::default(),
            tools: ToolsConfig::default(),
            tui: TuiConfig::default(),
        }
    }
}

impl Config {
    /// Load from path, or default if missing. Env wins for key and port.
    pub fn load(path: &PathBuf) -> Self {
        let mut cfg = Self::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(parsed) = toml::from_str::<Config>(&text) {
                cfg = parsed;
            }
        }
        // Env wins. Simple rule, no surprise.
        if let Ok(k) = std::env::var("OPENROUTER_API_KEY") {
            if !k.is_empty() {
                cfg.openrouter.api_key = k;
            }
        }
        if let Ok(p) = std::env::var("OCTO_PORT") {
            if let Ok(n) = p.parse::<u16>() {
                cfg.server.port = n;
            }
        }
        cfg
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }

    /// Expand leading ~/ to home. Tiny helper to avoid extra crate.
    pub fn expand(p: &str) -> PathBuf {
        if let Some(rest) = p.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                return PathBuf::from(home).join(rest);
            }
        }
        PathBuf::from(p)
    }

    pub fn default_path() -> PathBuf {
        Self::expand("~/.config/octo/config.toml")
    }
}

/// Client to server messages, one JSON per line over TCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    #[serde(rename = "hello")]
    Hello { client: String, client_v: String },
    #[serde(rename = "health")]
    Health,
    #[serde(rename = "shutdown")]
    Shutdown,
    #[serde(rename = "prompt")]
    Prompt {
        session_id: String,
        model: String,
        prompt: String,
    },
    #[serde(rename = "cancel")]
    Cancel { session_id: String },
    #[serde(rename = "list_sessions")]
    ListSessions,
}

/// Small session row for TUI resume list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub model: String,
    pub status: String,
}

/// Server to client messages, one JSON per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMsg {
    #[serde(rename = "hello_ok")]
    HelloOk {
        server_v: String,
        protocol_v: u32,
    },
    #[serde(rename = "health_ok")]
    HealthOk {
        server_v: String,
        protocol_v: u32,
        up_ms: u64,
    },
    #[serde(rename = "token_delta")]
    TokenDelta { session_id: String, text: String },
    #[serde(rename = "token_done")]
    TokenDone {
        session_id: String,
        ms: u64,
        first_ms: u64,
    },
    #[serde(rename = "token_error")]
    TokenError { session_id: String, msg: String },
    #[serde(rename = "session_list")]
    SessionList { sessions: Vec<SessionInfo> },
    #[serde(rename = "bye")]
    Bye { reason: String },
    #[serde(rename = "error")]
    Error { code: String, msg: String },
}

/// Future tools implement this. Server enforces approval via mode.
/// Added now so web, MCP, codemode slot in later with no rework.
pub trait Tool {
    fn name(&self) -> &str;
    fn needs_approval(&self, mode: Mode) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_loopback() {
        let c = Config::default();
        assert_eq!(c.addr(), "127.0.0.1:7531");
    }

    #[test]
    fn protocol_version_is_1() {
        assert_eq!(PROTOCOL_V, 3);
    }

    #[test]
    fn token_done_roundtrips() {
        // Regression: serde_json here rejects u128, so done must stay u64.
        // This test fails the build if anyone widens these fields again.
        let m = ServerMsg::TokenDone {
            session_id: "s".into(),
            ms: 1946,
            first_ms: 845,
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        match back {
            ServerMsg::TokenDone { ms, first_ms, .. } => {
                assert_eq!(ms, 1946);
                assert_eq!(first_ms, 845);
            }
            _ => panic!("wrong variant"),
        }
    }
}
