//! tools: file plus shell core. Server enforces mode here.
//! Rules: plan blocks writes and shell, workdir lock, 30s kill, 32k cap.
//! Parallel is done by caller with tokio join. This file stays sync plus small async for shell.

use octo_core::{Config, Mode};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const OUTPUT_CAP: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub ok: bool,
    pub ms: u128,
    pub output: String,
    pub truncated: bool,
    pub output_file: Option<String>,
}

/// True if path is inside allowed dirs or current workdir.
pub fn inside_allow(path: &Path, cfg: &Config) -> bool {
    let abs = absolutize(path);
    // Always allow inside current dir where daemon started.
    if let Ok(cwd) = std::env::current_dir() {
        if abs.starts_with(cwd) {
            return true;
        }
    }
    for a in &cfg.tools.workdir_allow {
        let base = Config::expand(a);
        if abs.starts_with(base) {
            return true;
        }
    }
    false
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(p)
    } else {
        p.to_path_buf()
    }
}

fn cap_output(full: String, session_id: &str, step: &str) -> ToolResult {
    let start = Instant::now();
    let _ = start; // ms set by caller, kept here for shape.
    if full.len() <= OUTPUT_CAP {
        return ToolResult {
            ok: true,
            ms: 0,
            output: full,
            truncated: false,
            output_file: None,
        };
    }
    // Save full to .octo/outputs, return head for prompt.
    let dir = PathBuf::from(".octo/outputs");
    let _ = std::fs::create_dir_all(&dir);
    let name = format!("{}_{}.log", session_id, step);
    let path = dir.join(&name);
    let _ = std::fs::write(&path, &full);
    let head = full.chars().take(OUTPUT_CAP).collect::<String>();
    ToolResult {
        ok: true,
        ms: 0,
        output: format!(
            "{}\n\n[truncated {} chars -> {}]",
            head,
            full.len() - head.len(),
            path.display()
        ),
        truncated: true,
        output_file: Some(path.display().to_string()),
    }
}

pub fn read(path: &str, cfg: &Config, session: &str, step: &str) -> Result<ToolResult, String> {
    let p = Path::new(path);
    if !inside_allow(p, cfg) {
        return Err(format!("blocked: outside workdir allow: {}", path));
    }
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    Ok(cap_output(text, session, step))
}

pub fn write(path: &str, content: &str, mode: Mode, cfg: &Config) -> Result<String, String> {
    if mode == Mode::Plan {
        return Err("blocked: plan mode is read only".into());
    }
    let p = Path::new(path);
    if !inside_allow(p, cfg) {
        return Err(format!("blocked: outside workdir allow: {}", path));
    }
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(p, content).map_err(|e| e.to_string())?;
    Ok("ok".into())
}

pub fn edit(path: &str, old: &str, new: &str, mode: Mode, cfg: &Config) -> Result<String, String> {
    if mode == Mode::Plan {
        return Err("blocked: plan mode is read only".into());
    }
    let p = Path::new(path);
    if !inside_allow(p, cfg) {
        return Err(format!("blocked: outside workdir allow: {}", path));
    }
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    if !text.contains(old) {
        return Err("blocked: old string not found".into());
    }
    let out = text.replacen(old, new, 1);
    std::fs::write(p, out).map_err(|e| e.to_string())?;
    Ok("ok".into())
}

pub fn glob(pattern: &str, cfg: &Config) -> Result<Vec<String>, String> {
    // Keep glob inside cwd for MVP to stay safe and simple.
    let mut out = vec![];
    for entry in glob::glob(pattern).map_err(|e| e.to_string())? {
        match entry {
            Ok(p) => {
                if inside_allow(&p, cfg) {
                    out.push(p.display().to_string());
                }
                if out.len() >= 200 {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    Ok(out)
}

pub fn grep(pattern: &str, path: &str, cfg: &Config) -> Result<String, String> {
    let p = Path::new(path);
    if !inside_allow(p, cfg) {
        return Err(format!("blocked: outside workdir allow: {}", path));
    }
    let text = std::fs::read_to_string(p).map_err(|e| e.to_string())?;
    let mut hits = vec![];
    for (i, line) in text.lines().enumerate() {
        if line.contains(pattern) {
            hits.push(format!("{}:{}", i + 1, line));
            if hits.join("\n").len() > OUTPUT_CAP {
                break;
            }
        }
    }
    Ok(hits.join("\n"))
}

/// Run shell via bash -c with timeout. Kills child on timeout.
/// Note: full process group kill with nix lands in Phase 3 harden. Timeout kill here stops lone child.
pub async fn shell(
    cmd: &str,
    mode: Mode,
    _cfg: &Config,
    session: &str,
    step: &str,
    timeout_secs: u64,
) -> Result<ToolResult, String> {
    if mode == Mode::Plan {
        return Err("blocked: plan mode is read only".into());
    }
    let start = Instant::now();
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(std::env::current_dir().map_err(|e| e.to_string())?)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    // Save pid before move so timeout can kill even after wait owns child.
    let pid = child.id();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let out = tokio::time::timeout(timeout, child.wait_with_output()).await;
    let ms = start.elapsed().as_millis();
    match out {
        Ok(Ok(o)) => {
            let mut full = String::from_utf8_lossy(&o.stdout).to_string();
            let err = String::from_utf8_lossy(&o.stderr).to_string();
            if !err.is_empty() {
                full.push_str("\nstderr:\n");
                full.push_str(&err);
            }
            full.push_str(&format!("\nexit: {}", o.status.code().unwrap_or(-1)));
            let mut r = cap_output(full, session, step);
            r.ms = ms;
            // Non zero exit is still a result, not an error, so model can see it.
            Ok(r)
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => {
            // Timeout: child future dropped, kill leftover pid if still alive.
            if let Some(pid) = pid {
                let _ = std::process::Command::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .output();
            }
            Err(format!("timeout after {}s, killed: {}", timeout_secs, cmd))
        }
    }
}
