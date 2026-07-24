//! HTTP client that calls the SuperSighurt LLM server living on the desktop.

use crate::config::ChatConfig;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
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
    respond_to_bots: bool,
    max_bot_chain: u32,
    reply_context_max_chars: usize,
}

impl ChatRuntime {
    pub fn new(client: ChatClient, cfg: &ChatConfig) -> Arc<Self> {
        Arc::new(Self {
            client,
            enabled: AtomicBool::new(cfg.enabled),
            admin_user_ids: cfg.admin_user_ids.iter().copied().collect(),
            respond_to_bots: cfg.respond_to_bots,
            max_bot_chain: cfg.max_bot_chain,
            reply_context_max_chars: cfg.reply_context_max_chars,
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

    pub fn respond_to_bots(&self) -> bool {
        self.respond_to_bots
    }

    pub fn max_bot_chain(&self) -> u32 {
        self.max_bot_chain
    }

    pub fn reply_context_max_chars(&self) -> usize {
        self.reply_context_max_chars
    }

    pub fn client(&self) -> &ChatClient {
        &self.client
    }
}

/// Everything the LLM server gets told about one triggering message.
/// `reply_to` is present when the trigger was a Discord reply and carries the
/// referenced message's author/content.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub channel_id: u64,
    pub user: String,
    pub user_id: u64,
    pub user_is_bot: bool,
    pub input: String,
    pub reply_to: Option<ChatReplyTo>,
}

/// Context about the message the trigger replied to.
#[derive(Debug, Clone)]
pub struct ChatReplyTo {
    pub user: String,
    pub user_id: u64,
    pub text: String,
    pub is_bot: bool,
    /// True when the replied-to message is OUR bot's own — the server maps
    /// that turn to PERSON_0 (its own voice) instead of a bystander tag.
    pub is_self: bool,
}

/// Wire response from the `/chat` endpoint.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    reply: String,
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

    pub async fn reply(&self, request: &ChatRequest) -> Result<String> {
        let body = wire_body(request);
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
        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow!("LLM response missing/invalid reply field ({}): {}", e, text))?;
        Ok(parsed.reply)
    }
}

/// Serialize the request payload. The server's handler is stringly-typed, so
/// every value goes over the wire as a JSON string ("true"/"false" for the
/// bot flags) and the reply_to_* fields are omitted entirely when absent.
fn wire_body(request: &ChatRequest) -> String {
    let mut body = serde_json::json!({
        "channel_id": request.channel_id.to_string(),
        "user": request.user,
        "user_id": request.user_id.to_string(),
        "user_is_bot": bool_str(request.user_is_bot),
        "input": request.input,
    });
    if let Some(reply_to) = &request.reply_to {
        let obj = body.as_object_mut().expect("wire body is a JSON object");
        obj.insert("reply_to_user".to_string(), reply_to.user.clone().into());
        obj.insert(
            "reply_to_user_id".to_string(),
            reply_to.user_id.to_string().into(),
        );
        obj.insert("reply_to_text".to_string(), reply_to.text.clone().into());
        obj.insert(
            "reply_to_is_bot".to_string(),
            bool_str(reply_to.is_bot).into(),
        );
        obj.insert(
            "reply_to_is_self".to_string(),
            bool_str(reply_to.is_self).into(),
        );
    }
    body.to_string()
}

fn bool_str(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> ChatRequest {
        ChatRequest {
            channel_id: 42,
            user: "Fredrik".to_string(),
            user_id: 7,
            user_is_bot: false,
            input: "hej \"du\" \u{1f600}".to_string(),
            reply_to: None,
        }
    }

    #[test]
    fn wire_body_omits_reply_fields_when_absent() {
        let v: serde_json::Value = serde_json::from_str(&wire_body(&base_request())).unwrap();
        assert_eq!(v["channel_id"], "42");
        assert_eq!(v["user"], "Fredrik");
        assert_eq!(v["user_id"], "7");
        assert_eq!(v["user_is_bot"], "false");
        assert_eq!(v["input"], "hej \"du\" \u{1f600}");
        assert!(v.get("reply_to_user").is_none());
        assert!(v.get("reply_to_user_id").is_none());
        assert!(v.get("reply_to_text").is_none());
        assert!(v.get("reply_to_is_bot").is_none());
    }

    #[test]
    fn wire_body_includes_reply_fields_when_present() {
        let mut request = base_request();
        request.user_is_bot = true;
        request.reply_to = Some(ChatReplyTo {
            user: "SuperSighurt".to_string(),
            user_id: 99,
            text: "previous message".to_string(),
            is_bot: true,
            is_self: true,
        });
        let v: serde_json::Value = serde_json::from_str(&wire_body(&request)).unwrap();
        assert_eq!(v["user_is_bot"], "true");
        assert_eq!(v["reply_to_user"], "SuperSighurt");
        assert_eq!(v["reply_to_user_id"], "99");
        assert_eq!(v["reply_to_text"], "previous message");
        assert_eq!(v["reply_to_is_bot"], "true");
        assert_eq!(v["reply_to_is_self"], "true");
    }

    #[test]
    fn response_parse_handles_unicode_escapes() {
        // The old hand-rolled parser choked on \uXXXX escapes; serde_json
        // handles them (including surrogate pairs for emoji).
        let parsed: ChatResponse =
            serde_json::from_str("{\"reply\":\"h\\u00e4r \\ud83d\\ude00\"}").unwrap();
        assert_eq!(parsed.reply, "här \u{1f600}");
    }
}
