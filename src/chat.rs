//! HTTP client that calls the SuperSighurt LLM server living on the desktop.

use crate::config::ChatConfig;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Wraps a `ChatClient` with a runtime on/off toggle and an admin allowlist.
///
/// The toggle is an `AtomicBool` so the `!ai on|off` command can flip it without
/// needing a lock or a config reload. Admin IDs are checked against the message
/// author for that command — if the set is empty, the command is disabled (you
/// must opt in by listing at least one user in `[chat].admin_user_ids`).
pub struct ChatRuntime {
    client: ChatClient,
    enabled: AtomicBool,
    admin_user_ids: HashSet<u64>,
}

impl ChatRuntime {
    pub fn new(client: ChatClient, initial_enabled: bool, admin_user_ids: Vec<u64>) -> Arc<Self> {
        Arc::new(Self {
            client,
            enabled: AtomicBool::new(initial_enabled),
            admin_user_ids: admin_user_ids.into_iter().collect(),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Returns the previous value.
    pub fn set_enabled(&self, value: bool) -> bool {
        self.enabled.swap(value, Ordering::Relaxed)
    }

    pub fn is_admin(&self, user_id: u64) -> bool {
        self.admin_user_ids.contains(&user_id)
    }

    pub fn has_admins(&self) -> bool {
        !self.admin_user_ids.is_empty()
    }

    pub fn client(&self) -> &ChatClient {
        &self.client
    }
}

#[derive(Clone)]
pub struct ChatClient {
    endpoint: String,
    api_key: String,
    http: reqwest::Client,
}

impl ChatClient {
    pub fn new(cfg: &ChatConfig, api_key: String) -> Result<Self> {
        if api_key.trim().is_empty() {
            bail!("LLM_API_KEY env var is empty");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.request_timeout_secs.max(1)))
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            endpoint: cfg.endpoint_url.trim_end_matches('/').to_string(),
            api_key,
            http,
        })
    }

    pub async fn reply(&self, channel_id: u64, user: &str, input: &str) -> Result<String> {
        let body = format!(
            r#"{{"channel_id":"{}","user":"{}","input":"{}"}}"#,
            channel_id,
            json_escape(user),
            json_escape(input),
        );
        let resp = self
            .http
            .post(format!("{}/chat", self.endpoint))
            .header("X-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context("send /chat request")?;
        let status = resp.status();
        let text = resp.text().await.context("read /chat response body")?;
        if !status.is_success() {
            bail!("LLM /chat returned {}: {}", status, text);
        }
        extract_reply(&text).ok_or_else(|| anyhow!("LLM response missing reply field: {}", text))
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn extract_reply(body: &str) -> Option<String> {
    let key = "\"reply\"";
    let start = body.find(key)? + key.len();
    let bytes = body.as_bytes();
    let mut i = start;
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    i += 1;
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'"' {
        return None;
    }
    i += 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    return None;
                }
                match bytes[i] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    _ => return None,
                }
                i += 1;
            }
            b'"' => return Some(out),
            _ => {
                let s = std::str::from_utf8(&bytes[i..]).ok()?;
                let ch = s.chars().next()?;
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    None
}
