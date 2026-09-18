//! HTTP backends: an OpenAI-compatible chat-completions server (llama.cpp
//! `llama-server`, and anything else speaking that wire format) and the raw
//! `/completion` endpoint for the legacy prompt render.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// Per-request sampling knobs (llama.cpp accepts them as extra fields).
#[derive(Debug, Clone)]
pub struct Sampling {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: u32,
    pub min_p: f64,
    pub repeat_penalty: f64,
    pub repeat_last_n: u32,
    pub presence_penalty: f64,
}

impl Default for Sampling {
    fn default() -> Self {
        Self { temperature: 0.7, top_p: 0.9, top_k: 40, min_p: 0.05, repeat_penalty: 1.1, repeat_last_n: 256, presence_penalty: 0.0 }
    }
}

/// One model turn.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: String,
    /// Raw OpenAI `tool_calls` array, if the server parsed any.
    pub tool_calls: Option<Value>,
    pub finish_reason: Option<String>,
}

#[derive(Clone)]
pub struct OpenAiBackend {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    pub thinking: bool,
}

impl OpenAiBackend {
    pub fn new(endpoint: &str, api_key: &str, model: &str, timeout_secs: u64, thinking: bool) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .build()
            .context("build backend client")?;
        Ok(Self { http, endpoint: endpoint.trim_end_matches('/').to_string(), api_key: api_key.to_string(), model: model.to_string(), thinking })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// `/health` (llama.cpp) or `/healthz` (our adapters); any 2xx counts.
    pub async fn healthy(&self) -> bool {
        for path in ["/health", "/healthz"] {
            if let Ok(r) = self.http.get(format!("{}{path}", self.endpoint)).timeout(Duration::from_secs(4)).send().await {
                if r.status().is_success() {
                    return true;
                }
            }
        }
        false
    }

    pub async fn chat(&self, messages: &[Value], tools: Option<&Value>, sampling: &Sampling, max_tokens: u32, stop: &[&str]) -> Result<Completion> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": sampling.temperature,
            "top_p": sampling.top_p,
            "top_k": sampling.top_k,
            "min_p": sampling.min_p,
            "repeat_penalty": sampling.repeat_penalty,
            "repeat_last_n": sampling.repeat_last_n,
            "presence_penalty": sampling.presence_penalty,
            "max_tokens": max_tokens,
            "stream": false,
            "chat_template_kwargs": {"enable_thinking": self.thinking},
        });
        if !stop.is_empty() {
            body["stop"] = json!(stop);
        }
        if let Some(t) = tools {
            if t.as_array().is_some_and(|a| !a.is_empty()) {
                body["tools"] = t.clone();
                body["tool_choice"] = json!("auto");
            }
        }
        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.endpoint))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("X-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context("send chat completion")?;
        let status = resp.status();
        let text = resp.text().await.context("read chat completion")?;
        if !status.is_success() {
            bail!("backend returned {status}: {}", text.chars().take(300).collect::<String>());
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| anyhow!("bad completion JSON ({e}): {}", text.chars().take(200).collect::<String>()))?;
        let choice = v["choices"].get(0).ok_or_else(|| anyhow!("completion had no choices"))?;
        let message = &choice["message"];
        let content = message["content"].as_str().unwrap_or("").to_string();
        let tool_calls = message.get("tool_calls").filter(|t| t.as_array().is_some_and(|a| !a.is_empty())).cloned();
        Ok(Completion { content, tool_calls, finish_reason: choice["finish_reason"].as_str().map(str::to_string) })
    }

    /// Raw `/completion` (llama.cpp) for the legacy prompt render.
    pub async fn complete(&self, prompt: &str, sampling: &Sampling, max_tokens: u32, stop: &[&str]) -> Result<String> {
        let body = json!({
            "prompt": prompt,
            "temperature": sampling.temperature,
            "top_p": sampling.top_p,
            "top_k": sampling.top_k,
            "min_p": sampling.min_p,
            "repeat_penalty": sampling.repeat_penalty,
            "repeat_last_n": sampling.repeat_last_n,
            "n_predict": max_tokens,
            "stream": false,
            "stop": stop,
            "cache_prompt": true,
        });
        let resp = self
            .http
            .post(format!("{}/completion", self.endpoint))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("X-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context("send completion")?;
        let status = resp.status();
        let text = resp.text().await.context("read completion")?;
        if !status.is_success() {
            bail!("backend returned {status}: {}", text.chars().take(300).collect::<String>());
        }
        let v: Value = serde_json::from_str(&text).context("bad completion JSON")?;
        Ok(v["content"].as_str().unwrap_or("").to_string())
    }
}
