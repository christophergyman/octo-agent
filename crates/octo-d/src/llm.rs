//! llm: OpenRouter OpenAI compat stream with kind retry.
//! Rules B: 3 retries on 429 and 5xx with backoff, 413 does one compact signal then one retry.
//! Esc cancel saves partial. No fallback model in MVP.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct LlmReq {
    pub api_key: String,
    pub model: String,
    pub prompt: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ChatBody {
    model: String,
    stream: bool,
    messages: Vec<Msg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Msg {
    role: String,
    content: String,
}

/// One streaming chunk from OpenRouter.
pub struct Chunk {
    pub text: String,
    pub done: bool,
}

/// Call OpenRouter, stream deltas to callback, return full text.
/// Retries temp errors, saves partial on cancel via caller abort.
pub async fn chat_stream<F>(req: LlmReq, mut on_delta: F) -> Result<String, String>
where
    F: FnMut(&str),
{
    let delays = [500, 1000, 2000];
    let mut last_err = String::new();

    for attempt in 0..=3 {
        match try_once(&req, &mut on_delta).await {
            Ok(full) => return Ok(full),
            Err(e) => {
                last_err = e.clone();
                // Do not retry user cancel or bad key or bad request.
                if e.contains("cancelled")
                    || e.contains("401")
                    || e.contains("400")
                    || e.contains("402")
                {
                    return Err(e);
                }
                // 413 means context too big. Caller runs compact then retries once.
                // Here we just surface with marker so daemon can compact once.
                if e.contains("413") {
                    return Err(format!("need_compact: {}", e));
                }
                if attempt < 3 {
                    // Only retry rate limit and temp server errors.
                    let retryable = e.contains("429")
                        || e.contains("500")
                        || e.contains("502")
                        || e.contains("503")
                        || e.contains("timeout")
                        || e.contains("no data");
                    if !retryable {
                        return Err(e);
                    }
                    let wait = delays[attempt as usize];
                    // Jitter 0 to 200ms to avoid thundering herd.
                    let jitter = (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos() % 200)
                        .unwrap_or(0)
                        / 1_000_000) as u64;
                    tokio::time::sleep(std::time::Duration::from_millis(wait + jitter)).await;
                    continue;
                } else {
                    return Err(last_err);
                }
            }
        }
    }
    Err(last_err)
}

async fn try_once<F>(req: &LlmReq, on_delta: &mut F) -> Result<String, String>
where
    F: FnMut(&str),
{
    if req.api_key.is_empty() {
        return Err("401 missing OPENROUTER_API_KEY".into());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let body = ChatBody {
        model: req.model.clone(),
        stream: true,
        messages: vec![Msg {
            role: "user".into(),
            content: req.prompt.clone(),
        }],
    };

    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&req.api_key)
        .header("HTTP-Referer", "https://github.com/octo-agent")
        .header("X-Title", "octo-agent")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "timeout connect".to_string()
            } else {
                e.to_string()
            }
        })?;

    let status = resp.status().as_u16();
    if status == 429 {
        return Err("429 rate limited".into());
    }
    if status == 413 {
        return Err("413 context too big".into());
    }
    if status >= 500 {
        return Err(format!("{} server error", status));
    }
    if status >= 400 {
        let t = resp.text().await.unwrap_or_default();
        // Trim to keep logs small.
        let short: String = t.chars().take(500).collect();
        return Err(format!("{} {}", status, short));
    }

    // SSE: lines like `data: {...}` then `data: [DONE]`.
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut full = String::new();
    // No data timeout: if server stalls 60s, treat as retryable.
    let no_data = std::time::Duration::from_secs(req.timeout_secs.max(10));

    loop {
        let next = tokio::time::timeout(no_data, stream.next()).await;
        let item = match next {
            Ok(i) => i,
            Err(_) => return Err("timeout no data".into()),
        };
        let bytes = match item {
            Some(Ok(b)) => b,
            Some(Err(e)) => return Err(e.to_string()),
            None => break,
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        // Process complete lines.
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Strip `data:` prefix if present.
            let data = line.strip_prefix("data:").unwrap_or(line).trim();
            if data == "[DONE]" {
                return Ok(full);
            }
            if !data.starts_with('{') {
                continue;
            }
            // Parse OpenAI delta: choices[0].delta.content
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                if let Some(t) = v
                    .pointer("/choices/0/delta/content")
                    .and_then(|x| x.as_str())
                {
                    full.push_str(t);
                    on_delta(t);
                } else if let Some(t) = v
                    .pointer("/choices/0/message/content")
                    .and_then(|x| x.as_str())
                {
                    // Non stream fallback, just in case.
                    full.push_str(t);
                    on_delta(t);
                }
            }
        }
    }
    Ok(full)
}
