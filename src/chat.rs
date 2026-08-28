//! HTTP client that calls the SuperSighurt LLM server living on the desktop.

use crate::config::ChatConfig;
use crate::reply_filter::ReplyFilter;
use crate::web_search::{WebSearchClient, WebSearchContext};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Wraps a `ChatClient` with a runtime on/off toggle.
///
/// The toggle is an `AtomicBool` so the `/ai on|off` slash command can flip it
/// without needing a lock or a config reload. Authorization for that command
/// (and `/moderation`, `/filterword`) is the Discord **Administrator**
/// permission, enforced in the command handlers — not an allowlist here.
pub struct ChatRuntime {
    client: ChatClient,
    enabled: AtomicBool,
    respond_to_bots: bool,
    max_bot_chain: u32,
    reply_queue_limit: u32,
    reply_context_max_chars: usize,
    recent_context_messages: usize,
    context_message_max_chars: usize,
    unprompted_reply_every: u32,
    react_probability: f64,
    notify_on_rejection: bool,
    notify_on_error: bool,
    notice_cooldown_secs: u64,
    web_search: Option<WebSearchClient>,
    /// Screens outgoing replies (two-step: deny-list, then local AI judge).
    filter: ReplyFilter,
}

impl ChatRuntime {
    pub fn new(
        client: ChatClient,
        cfg: &ChatConfig,
        web_search: Option<WebSearchClient>,
        filter: ReplyFilter,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            enabled: AtomicBool::new(cfg.enabled),
            respond_to_bots: cfg.respond_to_bots,
            max_bot_chain: cfg.max_bot_chain,
            reply_queue_limit: cfg.reply_queue_limit.max(1),
            reply_context_max_chars: cfg.reply_context_max_chars,
            recent_context_messages: cfg.recent_context_messages,
            context_message_max_chars: cfg.context_message_max_chars,
            unprompted_reply_every: cfg.unprompted_reply_every,
            react_probability: cfg.react_probability,
            notify_on_rejection: cfg.notify_on_rejection,
            notify_on_error: cfg.notify_on_error,
            notice_cooldown_secs: cfg.notice_cooldown_secs,
            web_search,
            filter,
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Returns the previous value.
    pub fn set_enabled(&self, value: bool) -> bool {
        self.enabled.swap(value, Ordering::Relaxed)
    }

    pub fn respond_to_bots(&self) -> bool {
        self.respond_to_bots
    }

    pub fn max_bot_chain(&self) -> u32 {
        self.max_bot_chain
    }

    /// Max chat triggers queued (waiting or running) per channel before further
    /// triggers are dropped. Clamped to at least 1 so a misconfig can't wedge
    /// the queue shut.
    pub fn reply_queue_limit(&self) -> u32 {
        self.reply_queue_limit
    }

    pub fn reply_context_max_chars(&self) -> usize {
        self.reply_context_max_chars
    }

    pub fn recent_context_messages(&self) -> usize {
        self.recent_context_messages
    }

    pub fn context_message_max_chars(&self) -> usize {
        self.context_message_max_chars
    }

    pub fn unprompted_reply_every(&self) -> u32 {
        self.unprompted_reply_every
    }

    pub fn notify_on_rejection(&self) -> bool {
        self.notify_on_rejection
    }

    pub fn notify_on_error(&self) -> bool {
        self.notify_on_error
    }

    pub fn notice_cooldown_secs(&self) -> u64 {
        self.notice_cooldown_secs
    }

    pub fn react_probability(&self) -> f64 {
        self.react_probability
    }

    pub fn client(&self) -> &ChatClient {
        &self.client
    }

    pub fn web_search(&self) -> Option<&WebSearchClient> {
        self.web_search.as_ref()
    }

    pub fn filter(&self) -> &ReplyFilter {
        &self.filter
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
    /// Recent ambient channel messages, oldest first. This is what lets the
    /// model understand a mention in the context of the conversation that
    /// preceded it instead of seeing the trigger in isolation.
    pub context: Vec<ChatContextMessage>,
    pub reply_to: Option<ChatReplyTo>,
    /// Present only for an explicit live-search request. Empty results mean a
    /// real retrieval was attempted but returned no usable evidence.
    pub web_search: Option<WebSearchContext>,
    /// Reaction mode: the server renders "React as SuperSighurt with one
    /// emoji, or say pass." instead of the normal reply instruction and caps
    /// generation short. The reply is an emoji (or "pass"), not a message.
    pub react: bool,
}

/// One ambient channel message supplied as structured context.
#[derive(Debug, Clone)]
pub struct ChatContextMessage {
    pub message_id: u64,
    pub user: String,
    pub user_id: u64,
    pub text: String,
    pub is_bot: bool,
    pub is_self: bool,
    pub reply_to_message_id: Option<u64>,
}

/// Context about the message the trigger replied to.
#[derive(Debug, Clone)]
pub struct ChatReplyTo {
    pub message_id: u64,
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
        "context": request.context.iter().map(|message| {
            let mut value = serde_json::json!({
                "message_id": message.message_id.to_string(),
                "user": message.user,
                "user_id": message.user_id.to_string(),
                "text": message.text,
                "is_bot": bool_str(message.is_bot),
                "is_self": bool_str(message.is_self),
            });
            if let Some(reply_id) = message.reply_to_message_id {
                value.as_object_mut().expect("context entry is an object").insert(
                    "reply_to_message_id".to_string(),
                    reply_id.to_string().into(),
                );
            }
            value
        }).collect::<Vec<_>>(),
    });
    if let Some(reply_to) = &request.reply_to {
        let obj = body.as_object_mut().expect("wire body is a JSON object");
        obj.insert(
            "reply_to_message_id".to_string(),
            reply_to.message_id.to_string().into(),
        );
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
    if request.react {
        let obj = body.as_object_mut().expect("wire body is a JSON object");
        obj.insert("mode".to_string(), "react".into());
    }
    if let Some(search) = &request.web_search {
        let obj = body.as_object_mut().expect("wire body is a JSON object");
        obj.insert("web_search_query".to_string(), search.query.clone().into());
        obj.insert(
            "web_results".to_string(),
            search
                .results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "title": result.title,
                        "url": result.url,
                        "snippet": result.snippet,
                    })
                })
                .collect::<Vec<_>>()
                .into(),
        );
    }
    body.to_string()
}

fn bool_str(v: bool) -> &'static str {
    if v {
        "true"
    } else {
        "false"
    }
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
            context: Vec::new(),
            reply_to: None,
            web_search: None,
            react: false,
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
        assert_eq!(v["context"], serde_json::json!([]));
        assert!(v.get("reply_to_user").is_none());
        assert!(v.get("reply_to_user_id").is_none());
        assert!(v.get("reply_to_text").is_none());
        assert!(v.get("reply_to_is_bot").is_none());
        assert!(v.get("web_search_query").is_none());
        assert!(v.get("mode").is_none());
    }

    #[test]
    fn wire_body_marks_react_mode() {
        let mut request = base_request();
        request.react = true;
        let v: serde_json::Value = serde_json::from_str(&wire_body(&request)).unwrap();
        assert_eq!(v["mode"], "react");
    }

    #[test]
    fn wire_body_includes_reply_fields_when_present() {
        let mut request = base_request();
        request.user_is_bot = true;
        request.context = vec![ChatContextMessage {
            message_id: 88,
            user: "Ada".to_string(),
            user_id: 8,
            text: "ambient context".to_string(),
            is_bot: false,
            is_self: false,
            reply_to_message_id: Some(77),
        }];
        request.reply_to = Some(ChatReplyTo {
            message_id: 90,
            user: "SuperSighurt".to_string(),
            user_id: 99,
            text: "previous message".to_string(),
            is_bot: true,
            is_self: true,
        });
        request.web_search = Some(WebSearchContext {
            query: "Rust ownership".to_string(),
            results: vec![crate::web_search::WebSearchResult {
                title: "Ownership".to_string(),
                url: "https://example.test/ownership".to_string(),
                snippet: "Each value has an owner.".to_string(),
            }],
        });
        let v: serde_json::Value = serde_json::from_str(&wire_body(&request)).unwrap();
        assert_eq!(v["user_is_bot"], "true");
        assert_eq!(v["context"][0]["message_id"], "88");
        assert_eq!(v["context"][0]["user"], "Ada");
        assert_eq!(v["context"][0]["reply_to_message_id"], "77");
        assert_eq!(v["reply_to_message_id"], "90");
        assert_eq!(v["reply_to_user"], "SuperSighurt");
        assert_eq!(v["reply_to_user_id"], "99");
        assert_eq!(v["reply_to_text"], "previous message");
        assert_eq!(v["reply_to_is_bot"], "true");
        assert_eq!(v["reply_to_is_self"], "true");
        assert_eq!(v["web_search_query"], "Rust ownership");
        assert_eq!(v["web_results"][0]["title"], "Ownership");
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
