//! db: SQLite durable log. One file, WAL, single writer via Mutex.
//! Schema ready for marathon C: events plus checkpoints plus output files.
//! Keep it simple for beginner: blocking rusqlite behind tokio Mutex.

use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub type Db = Arc<Mutex<Connection>>;

/// Open or create DB, turn on WAL, make tables, mark crashed runs as interrupted.
pub fn init(path: &PathBuf) -> Result<Connection, rusqlite::Error> {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA busy_timeout=5000;
        PRAGMA synchronous=NORMAL;
        CREATE TABLE IF NOT EXISTS sessions(
          id TEXT PRIMARY KEY,
          created_at TEXT DEFAULT (datetime('now')),
          model TEXT DEFAULT '',
          mode TEXT DEFAULT 'ask',
          status TEXT DEFAULT 'active'
        );
        CREATE TABLE IF NOT EXISTS events(
          seq INTEGER PRIMARY KEY AUTOINCREMENT,
          session_id TEXT NOT NULL,
          type TEXT NOT NULL,
          payload TEXT NOT NULL,
          status TEXT DEFAULT 'done',
          created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, seq);
        CREATE TABLE IF NOT EXISTS tool_runs(
          id TEXT PRIMARY KEY,
          session_id TEXT NOT NULL,
          step_id TEXT NOT NULL,
          tool TEXT NOT NULL,
          args TEXT NOT NULL,
          status TEXT DEFAULT 'running',
          exit_code INTEGER,
          ms INTEGER DEFAULT 0,
          output_path TEXT,
          output_hash TEXT,
          created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS allow_rules(
          session_id TEXT NOT NULL,
          tool TEXT NOT NULL,
          pattern TEXT NOT NULL,
          created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS checkpoints(
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          session_id TEXT NOT NULL,
          from_seq INTEGER NOT NULL,
          to_seq INTEGER NOT NULL,
          summary TEXT NOT NULL,
          tail_from_seq INTEGER NOT NULL,
          created_at TEXT DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS output_files(
          hash TEXT PRIMARY KEY,
          path TEXT NOT NULL,
          size INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS attempts(
          session_id TEXT NOT NULL,
          step_id TEXT NOT NULL,
          n INTEGER NOT NULL,
          last_error TEXT DEFAULT ''
        );
        ",
    )?;
    // Manual resume rule: anything left running is now interrupted.
    conn.execute(
        "UPDATE tool_runs SET status='interrupted' WHERE status='running'",
        [],
    )?;
    conn.execute(
        "UPDATE events SET status='interrupted' WHERE status='running'",
        [],
    )?;
    Ok(conn)
}

pub async fn create_session(db: &Db, id: &str, model: &str, mode: &str) -> Result<(), String> {
    let db = db.clone();
    let id = id.to_string();
    let model = model.to_string();
    let mode = mode.to_string();
    tokio::task::spawn_blocking(move || {
        let c = db.blocking_lock();
        c.execute(
            "INSERT OR IGNORE INTO sessions(id, model, mode, status) VALUES(?,?,?, 'active')",
            params![id, model, mode],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn add_event(
    db: &Db,
    session_id: &str,
    typ: &str,
    payload: &str,
) -> Result<i64, String> {
    let db = db.clone();
    let (s, t, p) = (session_id.to_string(), typ.to_string(), payload.to_string());
    tokio::task::spawn_blocking(move || {
        let c = db.blocking_lock();
        c.execute(
            "INSERT INTO events(session_id, type, payload) VALUES(?,?,?)",
            params![s, t, p],
        )
        .map_err(|e| e.to_string())?;
        Ok(c.last_insert_rowid())
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn list_sessions(db: &Db) -> Result<Vec<(String, String, String)>, String> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        let c = db.blocking_lock();
        let mut stmt = c
            .prepare("SELECT id, model, status FROM sessions ORDER BY created_at DESC LIMIT 20")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?;
        let mut out = vec![];
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}
