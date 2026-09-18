//! Sig's agent loop: persona prompt + tools + an OpenAI-compatible model.
//!
//! This replaces the Python `serve_agent.py` adapter. The bot talks straight
//! to `llama-server` (or any chat-completions endpoint), decides when the
//! model asked for a tool, runs it here (where Discord state already lives),
//! feeds the result back, and post-processes the final reply.

pub mod backend;
pub mod postprocess;
pub mod prompt;
pub mod tools;

use crate::chat::ChatRequest;
use crate::web_search::WebSearchClient;
use anyhow::{Context, Result};
use backend::{OpenAiBackend, Sampling};
use prompt::{Situation, ToolFormat};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tools::{ToolCall, ToolCtx};
use twilight_http::Client;

/// Static configuration of one agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub tool_format: ToolFormat,
    pub max_tool_iters: usize,
    pub max_reply_tokens: u32,
    pub max_tool_tokens: u32,
    pub sampling: Sampling,
    pub owner_user_id: u64,
    pub rules_channel_id: Option<u64>,
    pub data_root: PathBuf,
    pub memory_dir: PathBuf,
    pub news_feeds: Vec<String>,
    pub persona: String,
    /// Raw legacy render (`/completion`) instead of chat messages.
    pub legacy_render: bool,
}

/// Everything the agent needs across requests.
pub struct Agent {
    pub cfg: AgentConfig,
    backend: OpenAiBackend,
    web: reqwest::Client,
    pub directory: Arc<tools::discord::Directory>,
    pub reminders: Arc<tools::memory::ReminderStore>,
    web_search: Option<WebSearchClient>,
}

/// Token budget for short pokes ("lol", "yo", an emoji).
const SHORT_REPLY_TOKENS: u32 = 60;

/// "k", "lol", "yo sig", ":3", a lone emoji: too little to reason about.
pub fn is_short_reaction(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let alnum = t.chars().filter(|c| c.is_alphanumeric()).count();
    let words = t.split_whitespace().count();
    (t.chars().count() <= 6 && words <= 2) || alnum <= 3
}

/// Hard stop sequences: chat-template turn markers and the most common
/// invented-speaker prefixes.
const STOP: &[&str] = &["<|im_end|>", "<|im_start|>", "\nsig:", "\nSig:", "\ntester:", "\nuser:", "\nUser:", "TOOL RESULTS", "\n\n\n"];

impl Agent {
    pub fn new(
        cfg: AgentConfig,
        backend: OpenAiBackend,
        directory: Arc<tools::discord::Directory>,
        reminders: Arc<tools::memory::ReminderStore>,
        web_search: Option<WebSearchClient>,
    ) -> Result<Self> {
        let web = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build tool http client")?;
        Ok(Self { cfg, backend, web, directory, reminders, web_search })
    }

    pub fn backend(&self) -> &OpenAiBackend {
        &self.backend
    }

    /// Pull a `sudo:<password>` token out of the owner's message so it never
    /// reaches the model. Non-owners get the token stripped too.
    fn extract_sudo(input: &str, is_owner: bool) -> (String, Option<String>) {
        let mut cleaned = Vec::new();
        let mut password = None;
        for tok in input.split_whitespace() {
            if let Some(p) = tok.strip_prefix("sudo:") {
                if is_owner && !p.is_empty() {
                    password = Some(p.to_string());
                }
                continue;
            }
            cleaned.push(tok);
        }
        (cleaned.join(" "), password)
    }

    fn known_names(request: &ChatRequest) -> Vec<String> {
        let mut names: Vec<String> = request.context.iter().map(|c| c.user.clone()).collect();
        names.push(request.user.clone());
        if let Some(r) = &request.reply_to {
            names.push(r.user.clone());
        }
        names.sort();
        names.dedup();
        names
    }

    /// Produce Sig's reply (or an emoji in react mode).
    pub async fn reply(&self, http: &Client, request: &ChatRequest, situation: &Situation, bot_user_id: u64, guild_id: Option<u64>) -> Result<String> {
        let is_owner = request.user_id == self.cfg.owner_user_id;
        let (input, sudo_password) = Self::extract_sudo(&request.input, is_owner);
        let mut request = request.clone();
        request.input = input;
        let names = Self::known_names(&request);

        if self.cfg.legacy_render {
            let prompt = prompt::render_legacy_prompt(&self.cfg.persona, &request);
            let max = if request.react { 12 } else { self.cfg.max_reply_tokens };
            let raw = self.backend.complete(&prompt, &self.cfg.sampling, max, &["</s>", "<|user|>", "<|system|>", "<|assistant|>"]).await?;
            let cleaned = postprocess::clean_legacy_reply(&raw, &names);
            return Ok(postprocess::finalize(&cleaned, &names).unwrap_or_else(|| postprocess::FALLBACK_REPLY.to_string()));
        }

        // Short/low-information pokes ("lol", "k", a lone emoji) never enter the
        // tool loop and get a tight budget: they are the spiral trigger for a
        // mid-size model and never need a tool.
        let short_poke = is_short_reaction(&request.input);
        let format = if request.react || short_poke { ToolFormat::None } else { self.cfg.tool_format };
        // A poke never gets tool docs, but if the model still emits a tool call
        // (the v6 LoRA does), honour it rather than posting the glitch line.
        let tools_allowed = self.cfg.tool_format != ToolFormat::None && !request.react;
        let system = prompt::system_prompt(&self.cfg.persona, situation, &request.user, format, is_owner, sudo_password.is_some());
        if request.react {
            let messages = prompt::build_react_messages(&system, &request, situation);
            let c = self.backend.chat(&messages, None, &self.cfg.sampling, 12, STOP).await?;
            return Ok(postprocess::strip_thinking(&c.content).trim().to_string());
        }

        let mut messages = prompt::build_messages(&system, &request, format, situation);
        let native_tools = match format {
            ToolFormat::Native => Some(tools::native_tools(is_owner)),
            _ => None,
        };
        let ctx = ToolCtx {
            http,
            web: &self.web,
            directory: &self.directory,
            reminders: &self.reminders,
            web_search: self.web_search.as_ref(),
            data_root: &self.cfg.data_root,
            memory_dir: &self.cfg.memory_dir,
            rules_channel_id: self.cfg.rules_channel_id,
            news_feeds: &self.cfg.news_feeds,
            guild_id,
            channel_id: request.channel_id,
            user_id: request.user_id,
            user_name: &request.user,
            bot_user_id,
            is_owner,
            sudo_password: sudo_password.as_deref(),
        };

        let mut seen: HashSet<String> = HashSet::new();
        let mut errored: HashSet<String> = HashSet::new();
        let mut leak_retries = 0;
        let mut tool_log: Vec<Value> = Vec::new();
        let iters = if tools_allowed { self.cfg.max_tool_iters.max(1) } else { 1 };
        for _ in 0..iters {
            let max = if short_poke {
                SHORT_REPLY_TOKENS
            } else if format == ToolFormat::None {
                self.cfg.max_reply_tokens
            } else {
                self.cfg.max_tool_tokens
            };
            let completion = self.backend.chat(&messages, native_tools.as_ref(), &self.cfg.sampling, max, STOP).await?;
            let content = postprocess::strip_thinking(&completion.content);
            let mut calls: Vec<ToolCall> = completion.tool_calls.as_ref().map(tools::parse_native_calls).unwrap_or_default();
            let mut saw_syntax = false;
            if calls.is_empty() && tools_allowed {
                let (parsed, seen_syntax) = tools::parse_text_calls(&content);
                calls = parsed;
                saw_syntax = seen_syntax;
            }
            if calls.is_empty() {
                if saw_syntax && leak_retries < 1 {
                    // Raw/unparseable tool syntax: ask once for a proper call or plain words.
                    leak_retries += 1;
                    messages.push(json!({"role": "assistant", "content": content}));
                    messages.push(json!({"role": "user", "content": "That wasn't a valid tool call. Either call a tool properly or just answer in plain words with no tool syntax."}));
                    continue;
                }
                let reply = match postprocess::finalize(&content, &names) {
                    Some(r) => r,
                    None => {
                        tracing::info!("agent: reply fell back to the glitch line; raw={:?}", content.chars().take(300).collect::<String>());
                        postprocess::FALLBACK_REPLY.to_string()
                    }
                };
                self.io_log(&request, &messages, &content, &reply, &tool_log);
                return Ok(reply);
            }

            // Record the assistant turn in the shape the protocol expects.
            let mut calls: Vec<ToolCall> = calls.into_iter().take(4).collect();
            for c in calls.iter_mut() {
                tools::coerce_args(c, &request.input);
            }
            if format == ToolFormat::Native {
                let tc: Vec<Value> = calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        json!({"id": c.id.clone().unwrap_or_else(|| format!("call_{i}")), "type": "function", "function": {"name": c.name, "arguments": c.args.to_string()}})
                    })
                    .collect();
                messages.push(json!({"role": "assistant", "content": content, "tool_calls": tc}));
            } else {
                messages.push(json!({"role": "assistant", "content": content}));
            }

            let mut results: Vec<(ToolCall, tools::ToolOutcome)> = Vec::new();
            let mut had_error = false;
            let mut looped = false;
            for call in calls {
                let sig = tools::signature(&call);
                if seen.contains(&sig) || errored.contains(&call.name) {
                    looped = true;
                    tracing::info!("agent: skipping repeat/errored tool call {}", call.name);
                    results.push((call, tools::ToolOutcome { text: "(already ran this; use the earlier result)".into(), is_error: true }));
                    continue;
                }
                seen.insert(sig);
                let outcome = tools::run(&call, &ctx).await;
                tracing::info!("agent: tool {} owner={} error={} -> {}", call.name, is_owner, outcome.is_error, outcome.text.chars().take(80).collect::<String>().replace('\n', " "));
                if outcome.is_error {
                    had_error = true;
                    errored.insert(call.name.clone());
                }
                tool_log.push(json!({"name": call.name, "args": call.args, "error": outcome.is_error, "result": outcome.text.chars().take(400).collect::<String>()}));
                results.push((call, outcome));
            }

            let note = if had_error {
                "A tool errored above. Do NOT call it again and do NOT repeat the raw error text; tell them briefly in your own voice that it didn't work, then answer what you still can."
            } else if looped {
                "You already had those results. Do NOT call the same tool again; reply as Sig with what you have."
            } else {
                "Now reply as Sig using the results (or call another tool if you genuinely need more)."
            };
            if format == ToolFormat::Native {
                for (i, (call, outcome)) in results.iter().enumerate() {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call.id.clone().unwrap_or_else(|| format!("call_{i}")),
                        "name": call.name,
                        "content": outcome.text,
                    }));
                }
                // A clean native turn needs no coaching: the tool responses
                // are the whole story (and that is how the training rows look).
                if had_error || looped {
                    messages.push(json!({"role": "user", "content": format!("[{note}]")}));
                }
            } else {
                let block = results.iter().map(|(c, o)| format!("[{}] {}", c.name, o.text)).collect::<Vec<_>>().join("\n\n");
                messages.push(json!({"role": "user", "content": format!("TOOL RESULTS:\n{block}\n\n{note}")}));
            }
        }

        // Out of iterations: force a plain answer.
        messages.push(json!({"role": "user", "content": "Wrap up NOW with a normal short Sig reply. No tools, no tool syntax, just talk."}));
        let completion = self.backend.chat(&messages, None, &self.cfg.sampling, self.cfg.max_reply_tokens, STOP).await?;
        let content = postprocess::strip_thinking(&completion.content);
        let reply = postprocess::finalize(&content, &names).unwrap_or_else(|| postprocess::FALLBACK_REPLY.to_string());
        self.io_log(&request, &messages, &content, &reply, &tool_log);
        Ok(reply)
    }

    /// Exact-I/O record (what the model saw, what it said, which tools ran)
    /// appended to `<memory_dir>/io-log.jsonl`. Best-effort, size-capped; this
    /// is the raw material for the next training round and for debugging.
    fn io_log(&self, request: &ChatRequest, messages: &[Value], raw: &str, reply: &str, tools: &[Value]) {
        let path = self.cfg.memory_dir.join("io-log.jsonl");
        let record = json!({
            "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "user": request.user,
            "user_id": request.user_id.to_string(),
            "channel_id": request.channel_id.to_string(),
            "input": request.input,
            "context_n": request.context.len(),
            "messages": messages,
            "tools": tools,
            "raw": raw,
            "reply": reply,
        });
        let _ = std::fs::create_dir_all(&self.cfg.memory_dir);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{record}");
            // Cap at ~40 MB: rotate by moving to .1 (one generation kept).
            if f.metadata().map(|m| m.len() > 40_000_000).unwrap_or(false) {
                let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_reactions_detected() {
        assert!(is_short_reaction("lol"));
        assert!(is_short_reaction(":3"));
        assert!(is_short_reaction("🐢🐢🐢"));
        assert!(is_short_reaction("yo sig"));
        assert!(is_short_reaction("k"));
        assert!(!is_short_reaction("what is rule 4?"));
        assert!(!is_short_reaction("roll 2d6"));
        assert!(!is_short_reaction("987654321 * 123456789?"));
    }

    #[test]
    fn sudo_token_is_extracted_only_for_owner() {
        assert_eq!(Agent::extract_sudo("restart it sudo:hunter2 please", true), ("restart it please".to_string(), Some("hunter2".to_string())));
        assert_eq!(Agent::extract_sudo("restart it sudo:hunter2 please", false), ("restart it please".to_string(), None));
        assert_eq!(Agent::extract_sudo("no token here", true), ("no token here".to_string(), None));
    }
}
