//! Sig's tools: specs the model sees, the call parser, and the dispatcher.
//!
//! Every tool is a plain function returning a short string (what the model
//! sees as the tool result). Tools are deliberately forgiving about argument
//! names because mid-size local models drift ("expr" vs "expression"), and
//! every tool bounds its own output so a runaway result can't blow the prompt.
//!
//! Tool-call SYNTAX comes in three flavours, all parsed here:
//!   * native   — the OpenAI-style `tool_calls` array llama.cpp returns when
//!                the request carried `tools` (Qwen's Hermes template);
//!   * hermes   — a `<tool_call>{json}</tool_call>` block left in the content;
//!   * text     — the legacy `TOOL_CALL: {"name": .., "args": {..}}` line the
//!                bespoke v6 LoRA was trained on.

pub mod calc;
pub mod dice;
pub mod discord;
pub mod memory;
pub mod rss;
pub mod system;
pub mod text;
pub mod units;
pub mod web;

use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;

/// What the model is told about one tool.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// Compact JSON example of the args, for the text protocol docs.
    pub example: &'static str,
    /// JSON schema for the native protocol.
    pub parameters: Value,
    pub owner_only: bool,
    /// Not part of the schema the current model was trained with; offered
    /// only when `chat.extra_tools = true`.
    pub extra: bool,
}

/// One parsed call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub args: Value,
}

/// Result of running one tool.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false }
    }
    fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true }
    }
}

/// Everything a tool may need about the conversation it runs in.
pub struct ToolCtx<'a> {
    pub http: &'a twilight_http::Client,
    pub web: &'a reqwest::Client,
    pub directory: &'a discord::Directory,
    pub reminders: &'a memory::ReminderStore,
    pub web_search: Option<&'a crate::web_search::WebSearchClient>,
    pub data_root: &'a Path,
    pub memory_dir: &'a Path,
    pub rules_channel_id: Option<u64>,
    pub news_feeds: &'a [String],
    pub guild_id: Option<u64>,
    pub channel_id: u64,
    pub user_id: u64,
    pub user_name: &'a str,
    pub bot_user_id: u64,
    pub is_owner: bool,
    pub sudo_password: Option<&'a str>,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

fn specs_all() -> &'static Vec<ToolSpec> {
    static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        vec![
            ToolSpec {
                name: "get_time",
                description: "Current date and time.",
                example: "{}",
                parameters: obj(json!({}), &[]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "calculator",
                description: "Exact arithmetic (12*(3+4), sqrt(2), 13 squared, 15% of 80).",
                example: r#"{"expression": "987654321*123456789"}"#,
                parameters: obj(json!({"expression": {"type": "string"}}), &["expression"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "roll",
                description: "Roll dice: 2d6, d20, 4d6+3.",
                example: r#"{"dice": "2d6"}"#,
                parameters: obj(json!({"dice": {"type": "string"}}), &["dice"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "unit_convert",
                description: "Convert units (length, mass, time, data, speed, volume, area, temperature).",
                example: r#"{"value": 100, "from": "km", "to": "mi"}"#,
                parameters: obj(json!({"value": {"type": "number"}, "from": {"type": "string"}, "to": {"type": "string"}}), &["value", "from", "to"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "text_util",
                description: "String ops: count (of=substring), word_count, char_count, reverse, upper, lower.",
                example: r#"{"operation": "count", "text": "strawberry", "of": "r"}"#,
                parameters: obj(json!({"operation": {"type": "string"}, "text": {"type": "string"}, "of": {"type": "string"}}), &["operation", "text"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "weather",
                description: "Current weather for a named place.",
                example: r#"{"location": "Stockholm"}"#,
                parameters: obj(json!({"location": {"type": "string"}}), &["location"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "web_search",
                description: "Search the web for current facts.",
                example: r#"{"query": "latest rust release"}"#,
                parameters: obj(json!({"query": {"type": "string"}}), &["query"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "wiki",
                description: "Wikipedia summary of a topic.",
                example: r#"{"topic": "Voyager 1"}"#,
                parameters: obj(json!({"topic": {"type": "string"}}), &["topic"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "news",
                description: "Latest headlines, optional topic.",
                example: r#"{"topic": "nvidia", "limit": 5}"#,
                parameters: obj(json!({"topic": {"type": "string"}}), &[]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "fetch_url",
                description: "Read the text of a linked web page.",
                example: r#"{"url": "https://example.com/post"}"#,
                parameters: obj(json!({"url": {"type": "string"}}), &["url"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "define",
                description: "Definition of a word; slang=true uses Urban Dictionary.",
                example: r#"{"word": "rizz", "slang": true}"#,
                parameters: obj(json!({"word": {"type": "string"}, "slang": {"type": "boolean"}}), &["word"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "lookup_rule",
                description: "The real posted server rule for a label (4, ∞, -1, 16.1) or 'count'.",
                example: r#"{"label": "4"}"#,
                parameters: obj(json!({"label": {"type": "string"}}), &["label"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "search_discord",
                description: "Search this server's message history; optional channel and author.",
                example: r#"{"query": "ham atoms", "channel": "general"}"#,
                parameters: obj(json!({"query": {"type": "string"}, "channel": {"type": "string"}, "author": {"type": "string"}}), &["query"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "random_message",
                description: "A random real message from this server; filters contains/channel, sort=reactions for the most reacted.",
                example: r#"{"contains": "fart", "sort": "reactions"}"#,
                parameters: obj(json!({"contains": {"type": "string"}, "channel": {"type": "string"}, "sort": {"type": "string"}}), &[]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "server_activity",
                description: "What's been happening on this server in the last N hours.",
                example: r#"{"hours": 24}"#,
                parameters: obj(json!({"hours": {"type": "integer"}}), &[]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "who_is",
                description: "Profile of a server member from the logs.",
                example: r#"{"name": "walnutty2"}"#,
                parameters: obj(json!({"name": {"type": "string"}}), &["name"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "remind",
                description: "Post a reminder later in this channel (when: 'in 10 minutes', 'at 18:30', 'tomorrow 09:00').",
                example: r#"{"when": "in 20 minutes", "text": "check the oven"}"#,
                parameters: obj(json!({"when": {"type": "string"}, "text": {"type": "string"}}), &["when", "text"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "memory",
                description: "Your notes and diary: action=note|diary saves text, action=read_notes|read_diary reads (optional query).",
                example: r#"{"action": "note", "text": "zunabaro prefers tabs"}"#,
                parameters: obj(json!({"action": {"type": "string"}, "text": {"type": "string"}, "query": {"type": "string"}}), &["action"]),
                owner_only: false,
                extra: false,
            },
            ToolSpec {
                name: "events",
                description: "Upcoming scheduled events on this server (name, when, where, interested count).",
                example: "{}",
                parameters: obj(json!({}), &[]),
                owner_only: false,
                extra: true,
            },
            ToolSpec {
                name: "time_in",
                description: "Current time in another place or time zone (sweden, colombia, california, prague, hong kong, utc...).",
                example: r#"{"place": "colombia"}"#,
                parameters: obj(json!({"place": {"type": "string"}}), &["place"]),
                owner_only: false,
                extra: true,
            },
            ToolSpec {
                name: "run_command",
                description: "OWNER ONLY: run a shell command on the bot host.",
                example: r#"{"command": "uptime", "sudo": false}"#,
                parameters: obj(json!({"command": {"type": "string"}, "sudo": {"type": "boolean"}}), &["command"]),
                owner_only: true,
                extra: false,
            },
        ]
    })
}

/// Specs visible to this caller (owner-only tools are hidden from others;
/// `extra` tools only when the deployment enables them).
pub fn specs(is_owner: bool, extras: bool) -> Vec<&'static ToolSpec> {
    specs_all().iter().filter(|s| (is_owner || !s.owner_only) && (extras || !s.extra)).collect()
}

/// OpenAI `tools` payload for the native protocol.
pub fn native_tools(is_owner: bool, extras: bool) -> Value {
    Value::Array(
        specs(is_owner, extras)
            .into_iter()
            .map(|s| {
                json!({"type": "function", "function": {"name": s.name, "description": s.description, "parameters": s.parameters}})
            })
            .collect(),
    )
}

/// One line per tool for the text protocol docs.
pub fn text_docs(is_owner: bool, extras: bool) -> String {
    specs(is_owner, extras)
        .into_iter()
        .map(|s| format!("- {} {} — {}", s.name, s.example, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_known_tool(name: &str) -> bool {
    specs_all().iter().any(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// call parsing
// ---------------------------------------------------------------------------

/// Byte span of a balanced `{...}` JSON object starting at `start` (which must
/// point at `{`). Respects string escapes. None if unbalanced.
fn json_object_span(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn call_from_value(v: Value, id: Option<String>) -> Option<ToolCall> {
    let obj = v.as_object()?;
    let name = obj
        .get("name")
        .or_else(|| obj.get("tool"))
        .or_else(|| obj.get("function"))
        .and_then(|n| match n {
            Value::String(s) => Some(s.clone()),
            Value::Object(f) => f.get("name").and_then(Value::as_str).map(str::to_string),
            _ => None,
        })?;
    let mut args = obj
        .get("args")
        .or_else(|| obj.get("arguments"))
        .or_else(|| obj.get("parameters"))
        .or_else(|| obj.get("input"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Value::String(s) = &args {
        args = serde_json::from_str(s).unwrap_or_else(|_| json!({"raw": s}));
    }
    if !args.is_object() {
        args = json!({"raw": args});
    }
    Some(ToolCall { id, name: name.trim().to_string(), args })
}

/// Parse `TOOL_CALL: {...}` lines and `<tool_call>{...}</tool_call>` blocks
/// out of model text. Returns the calls plus whether any tool syntax was seen
/// (even if it failed to parse), so the caller can re-prompt on a leak.
pub fn parse_text_calls(text: &str) -> (Vec<ToolCall>, bool) {
    let mut calls = Vec::new();
    let mut saw_syntax = false;
    for marker in ["TOOL_CALL:", "tool_call:", "<tool_call>"] {
        let mut from = 0;
        while let Some(pos) = text[from..].find(marker) {
            saw_syntax = true;
            let after = from + pos + marker.len();
            let Some(brace) = text[after..].find('{').map(|b| after + b) else { break };
            // Only accept a brace that is near the marker (whitespace/newline only).
            if !text[after..brace].trim().is_empty() {
                from = after;
                continue;
            }
            match json_object_span(text, brace) {
                Some(end) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&text[brace..end]) {
                        if let Some(c) = call_from_value(v, None) {
                            calls.push(c);
                        }
                    }
                    from = end;
                }
                None => break,
            }
        }
    }
    (calls, saw_syntax)
}

/// Parse the OpenAI-style `tool_calls` array.
pub fn parse_native_calls(tool_calls: &Value) -> Vec<ToolCall> {
    let mut out = Vec::new();
    if let Some(items) = tool_calls.as_array() {
        for item in items {
            let id = item.get("id").and_then(Value::as_str).map(str::to_string);
            let func = item.get("function").cloned().unwrap_or_else(|| item.clone());
            if let Some(c) = call_from_value(func, id) {
                out.push(c);
            }
        }
    }
    out
}

/// True when the text looks like it contains raw tool syntax (leak guard).
pub fn looks_like_tool_syntax(text: &str) -> bool {
    let t = text.trim();
    t.contains("TOOL_CALL") || t.contains("<tool_call>") || t.contains("</tool_call>")
        || (t.starts_with('{') && t.contains("\"name\"") && (t.contains("\"args\"") || t.contains("\"arguments\"")))
}

/// Remove tool syntax spans from a user-facing reply.
pub fn strip_tool_syntax(text: &str) -> String {
    let mut out = text.to_string();
    for (open, close) in [("<tool_call>", "</tool_call>"), ("<tool_response>", "</tool_response>")] {
        while let Some(s) = out.find(open) {
            match out[s..].find(close) {
                Some(e) => out.replace_range(s..s + e + close.len(), ""),
                None => {
                    out.truncate(s);
                    break;
                }
            }
        }
    }
    let mut kept = Vec::new();
    for line in out.lines() {
        let l = line.trim_start();
        if l.starts_with("TOOL_CALL") || l.starts_with("tool_call:") || l.starts_with("TOOL RESULTS") {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n").trim().to_string()
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

fn s_arg<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str)).map(str::trim).filter(|s| !s.is_empty())
}

fn num_arg(args: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        args.get(*k).and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().replace(',', "").parse().ok(),
            _ => None,
        })
    })
}

fn usize_arg(args: &Value, keys: &[&str], default: usize, max: usize) -> usize {
    num_arg(args, keys).map(|n| n.max(1.0) as usize).unwrap_or(default).min(max)
}

fn bool_arg(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim().to_lowercase().as_str(), "true" | "yes" | "1"),
        _ => false,
    }
}

/// Cap a tool result so one call can't flood the prompt.
fn bounded(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}…(truncated)")
}

/// Run one tool. Never panics; unknown tools and bad args come back as errors
/// the model can read.
/// Map the names a model invents ("reverse", "search", "dice") onto real
/// tools. Returns the canonical name plus an implied argument to inject.
pub fn canonical_name(name: &str) -> (String, Option<(&'static str, &'static str)>) {
    let n = name.trim().trim_start_matches("functions.").to_lowercase().replace([' ', '-'], "_");
    let mapped: (&str, Option<(&str, &str)>) = match n.as_str() {
        "reverse" | "reverse_text" | "reverse_string" => ("text_util", Some(("operation", "reverse"))),
        "count" | "count_letters" | "count_occurrences" | "letter_count" => ("text_util", Some(("operation", "count"))),
        "word_count" | "count_words" => ("text_util", Some(("operation", "word_count"))),
        "upper" | "uppercase" => ("text_util", Some(("operation", "upper"))),
        "lower" | "lowercase" => ("text_util", Some(("operation", "lower"))),
        "text" | "string" | "string_util" => ("text_util", None),
        "time" | "clock" | "date" | "datetime" | "current_time" | "get_date" | "now" => ("get_time", None),
        "time_zone" | "timezone" | "world_time" | "time_at" | "local_time" => ("time_in", None),
        "event" | "scheduled_events" | "upcoming_events" | "calendar" | "schedule" => ("events", None),
        "calc" | "math" | "calculate" | "compute" | "eval" | "arithmetic" => ("calculator", None),
        "dice" | "roll_dice" | "dice_roll" | "rolldice" => ("roll", None),
        "convert" | "unit" | "units" | "convert_units" | "unit_conversion" => ("unit_convert", None),
        "search" | "google" | "internet" | "browse" | "search_web" | "websearch" | "duckduckgo" => ("web_search", None),
        "wikipedia" | "wiki_summary" | "encyclopedia" => ("wiki", None),
        "headlines" | "get_news" | "news_search" => ("news", None),
        "fetch" | "read_url" | "read_page" | "open_url" | "get_url" | "browse_url" => ("fetch_url", None),
        "dictionary" | "definition" | "define_word" | "lookup_word" => ("define", None),
        "rule" | "rules" | "get_rule" | "server_rule" | "server_rules" | "rule_lookup" => ("lookup_rule", None),
        "discord_search" | "search_messages" | "search_server" | "search_history" | "search_chat" | "find_message" | "search_channel" => ("search_discord", None),
        "random" | "quote" | "random_quote" | "funny_message" | "find_funny_message" => ("random_message", None),
        "activity" | "whats_happening" | "recent_activity" | "channel_activity" | "server_summary" => ("server_activity", None),
        "whois" | "who" | "user_info" | "profile" | "member_info" => ("who_is", None),
        "reminder" | "remind_me" | "set_reminder" | "timer" | "alarm" => ("remind", None),
        "note" | "write_note" | "save_note" | "add_note" | "remember" | "memo" => ("memory", Some(("action", "note"))),
        "notes" | "read_notes" | "get_notes" | "list_notes" | "recall" => ("memory", Some(("action", "read_notes"))),
        "diary" | "write_diary" | "journal" | "diary_write" | "write_journal" => ("memory", Some(("action", "diary"))),
        "read_diary" | "diary_read" | "read_journal" | "get_diary" => ("memory", Some(("action", "read_diary"))),
        "urban" | "urban_dictionary" | "urbandictionary" | "slang" => ("define", Some(("slang", "true"))),
        "server_status" | "status" | "voice" | "voice_status" | "members" | "online" => ("server_activity", None),
        "shell" | "bash" | "exec" | "command" | "execute" | "run" | "terminal" => ("run_command", None),
        "weather_lookup" | "get_weather" | "forecast" => ("weather", None),
        _ => (name, None),
    };
    (mapped.0.to_string(), mapped.1)
}

pub async fn run(call: &ToolCall, ctx: &ToolCtx<'_>) -> ToolOutcome {
    let (canonical, implied) = canonical_name(&call.name);
    let mut call = call.clone();
    call.name = canonical;
    if let Some((key, value)) = implied {
        if call.args.get(key).is_none() {
            if let Some(obj) = call.args.as_object_mut() {
                obj.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
    }
    let call = &call;
    let Some(spec) = specs_all().iter().find(|s| s.name == call.name) else {
        return ToolOutcome::err(format!(
            "error: no such tool '{}' (available: {})",
            call.name,
            specs(ctx.is_owner, true).iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
        ));
    };
    if spec.owner_only && !ctx.is_owner {
        return ToolOutcome::err(format!("error: {} is owner-only and {} is not the owner", spec.name, ctx.user_name));
    }
    let args = &call.args;
    let result: Result<String, String> = match spec.name {
        "get_time" => Ok(system::get_time()),
        "time_in" => system::time_in(s_arg(args, &["place", "location", "zone", "tz", "city", "country", "raw"]).unwrap_or("")),
        "events" => discord::events(ctx).await,
        "calculator" => {
            let expr = s_arg(args, &["expression", "expr", "input", "query", "equation", "math", "problem", "raw"]).unwrap_or("");
            calc::calculate(expr).map_err(|e| format!("calculator error: {e}"))
        }
        "roll" => {
            let spec = s_arg(args, &["dice", "notation", "expr", "spec", "raw"]).unwrap_or("1d20");
            dice::roll(spec).map_err(|e| format!("roll error: {e}"))
        }
        "unit_convert" => {
            let value = num_arg(args, &["value", "amount", "n"]);
            let from = s_arg(args, &["from", "src", "from_unit"]);
            let to = s_arg(args, &["to", "dst", "to_unit"]);
            match (value, from, to) {
                (Some(v), Some(f), Some(t)) => units::convert(v, f, t).map_err(|e| format!("unit_convert error: {e}")),
                _ => Err(r#"unit_convert error: need {"value": N, "from": "km", "to": "mi"}"#.to_string()),
            }
        }
        "text_util" => {
            let text = s_arg(args, &["text", "input", "string"]).unwrap_or("");
            let of = s_arg(args, &["of", "needle", "substring", "char", "letter"]);
            let op = s_arg(args, &["operation", "op", "mode"]).unwrap_or(if of.is_some() { "count" } else { "" });
            text::text_util(op, text, of).map_err(|e| format!("text_util error: {e}"))
        }
        "weather" => web::weather(ctx.web, s_arg(args, &["location", "place", "city", "q", "raw"]).unwrap_or("")).await,
        "web_search" => {
            let q = s_arg(args, &["query", "q", "search", "raw"]).unwrap_or("");
            web::web_search(ctx.web, ctx.web_search, q, usize_arg(args, &["max_results", "limit"], 4, 6)).await
        }
        "wiki" => web::wiki(ctx.web, s_arg(args, &["topic", "title", "query", "q", "raw"]).unwrap_or("")).await,
        "news" => {
            web::news(ctx.web, ctx.news_feeds, s_arg(args, &["topic", "query", "q"]), usize_arg(args, &["limit", "n"], 6, 10)).await
        }
        "fetch_url" => web::fetch_url(ctx.web, s_arg(args, &["url", "link", "raw"]).unwrap_or("")).await,
        "define" => {
            let word = s_arg(args, &["word", "term", "query", "raw"]).unwrap_or("");
            if bool_arg(args, "slang") || bool_arg(args, "urban") {
                web::urban(ctx.web, word).await
            } else {
                web::define(ctx.web, word).await
            }
        }
        "lookup_rule" => discord::lookup_rule(ctx, s_arg(args, &["label", "rule", "number", "n", "raw"]).unwrap_or("")),
        "search_discord" => discord::search(
            ctx,
            s_arg(args, &["query", "q", "text", "keywords", "keyword", "search", "term", "terms", "phrase", "message", "raw"]).unwrap_or(""),
            s_arg(args, &["channel"]),
            s_arg(args, &["author", "user", "from"]),
            num_arg(args, &["n", "number"]).map(|n| n as usize),
            usize_arg(args, &["limit"], 6, 10),
        ),
        "random_message" => discord::random_message(
            ctx,
            s_arg(args, &["channel"]),
            s_arg(args, &["author", "user", "from"]),
            s_arg(args, &["contains", "query", "q", "keyword", "keywords", "word", "text", "term", "about"]),
            s_arg(args, &["sort"]).unwrap_or("random"),
            usize_arg(args, &["min_length", "min_len"], 12, 400),
        ),
        "server_activity" => discord::server_activity(ctx, usize_arg(args, &["hours"], 24, 24 * 14), s_arg(args, &["channel"])).await,
        "who_is" => discord::who_is(ctx, s_arg(args, &["name", "user", "who", "raw"]).unwrap_or("")).await,
        "remind" => memory::remind(ctx, s_arg(args, &["when", "time", "in", "at"]).unwrap_or(""), s_arg(args, &["text", "message", "what"]).unwrap_or("")),
        "memory" => {
            let action = s_arg(args, &["action", "op", "mode"]).unwrap_or("note").to_lowercase();
            let text = s_arg(args, &["text", "note", "entry", "content", "raw"]).unwrap_or("");
            let query = s_arg(args, &["query", "q"]);
            let limit = usize_arg(args, &["limit"], 8, 40);
            match action.as_str() {
                "note" | "write_note" | "save" | "remember" => memory::write_note(ctx.memory_dir, text),
                "diary" | "write_diary" | "journal" => memory::write_diary(ctx.memory_dir, text),
                "read_notes" | "notes" | "read" | "recall" => memory::read_notes(ctx.memory_dir, query, limit),
                "read_diary" | "diary_read" => memory::read_diary(ctx.memory_dir, query, limit.min(25)),
                other => Err(format!("memory error: unknown action '{other}' (note, diary, read_notes, read_diary)")),
            }
        }
        "run_command" => {
            let command = s_arg(args, &["command", "cmd", "raw"]).unwrap_or("");
            system::run_command(command, bool_arg(args, "sudo"), ctx.sudo_password).await
        }
        _ => Err(format!("error: tool '{}' is registered but not wired", spec.name)),
    };
    match result {
        Ok(text) => ToolOutcome::ok(bounded(text, 3000)),
        Err(text) => {
            tracing::info!("agent: tool {} failed with args {}: {}", spec.name, call.args, text.chars().take(120).collect::<String>());
            ToolOutcome::err(bounded(text, 600))
        }
    }
}

/// Strip "search for", "roll", "what is" style lead-ins from the user's text
/// so it can stand in for an argument the model forgot.
fn strip_lead_in(input: &str) -> String {
    let mut t = input.trim().trim_end_matches(['?', '.', '!']).to_string();
    let lower = t.to_lowercase();
    for lead in [
        "can you search for ", "can you search ", "please search for ", "search the server for ", "search discord for ",
        "search for ", "search ", "look up ", "lookup ", "find me ", "find a ", "find ", "what is the weather in ",
        "what's the weather in ", "weather in ", "weather ", "roll a ", "roll ", "what is a ", "what is an ", "what is ",
        "what's ", "whats ", "who is ", "who's ", "define ", "tell me about ", "calculate ", "compute ",
    ] {
        if lower.starts_with(lead) {
            t = t[lead.len()..].trim().to_string();
            break;
        }
    }
    t
}

/// Fill in the primary argument of a call the model left empty using the
/// user's own words (models drop the query surprisingly often).
pub fn coerce_args(call: &mut ToolCall, user_input: &str) {
    let (canonical, _) = canonical_name(&call.name);
    let primary: Option<&str> = match canonical.as_str() {
        "search_discord" | "web_search" => Some("query"),
        "random_message" => None,
        "wiki" => Some("topic"),
        "weather" => Some("location"),
        "define" => Some("word"),
        "urban" => Some("term"),
        "calculator" => Some("expression"),
        "roll" => Some("dice"),
        "who_is" => Some("name"),
        "fetch_url" => Some("url"),
        _ => None,
    };
    let Some(key) = primary else { return };
    let present = s_arg(&call.args, &[key]).is_some()
        || match canonical.as_str() {
            "search_discord" | "web_search" => s_arg(&call.args, &["q", "text", "keywords", "keyword", "search", "term", "terms", "phrase", "raw"]).is_some(),
            "calculator" => s_arg(&call.args, &["expr", "input", "query", "equation", "math", "raw"]).is_some(),
            "roll" => s_arg(&call.args, &["notation", "expr", "spec", "raw"]).is_some(),
            "weather" => s_arg(&call.args, &["place", "city", "q", "raw"]).is_some(),
            _ => s_arg(&call.args, &["raw", "q", "query"]).is_some(),
        };
    if present {
        return;
    }
    let mut derived = match canonical.as_str() {
        "roll" => dice::find_spec(user_input).unwrap_or_else(|| "1d20".to_string()),
        "fetch_url" => user_input.split_whitespace().find(|w| w.starts_with("http")).unwrap_or("").trim_matches(['<', '>']).to_string(),
        _ => strip_lead_in(user_input),
    };
    // "ham atoms in general" -> query "ham atoms", channel "general".
    if canonical == "search_discord" && call.args.get("channel").is_none() {
        if let Some((q, chan)) = derived.rsplit_once(" in ") {
            let chan = chan.trim().trim_start_matches('#');
            if !q.trim().is_empty() && !chan.is_empty() && !chan.contains(' ') {
                if let Some(obj) = call.args.as_object_mut() {
                    obj.insert("channel".to_string(), Value::String(chan.to_string()));
                }
                derived = q.trim().to_string();
            }
        }
    }
    if derived.is_empty() {
        return;
    }
    if let Some(obj) = call.args.as_object_mut() {
        obj.insert(key.to_string(), Value::String(derived));
    }
}

/// Stable signature for de-looping identical calls within one reply.
pub fn signature(call: &ToolCall) -> String {
    format!("{}|{}", call.name, call.args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_protocol() {
        let (calls, seen) = parse_text_calls("TOOL_CALL: {\"name\": \"calculator\", \"args\": {\"expression\": \"1+1\"}}");
        assert!(seen);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "calculator");
        assert_eq!(calls[0].args["expression"], "1+1");
    }

    #[test]
    fn parses_hermes_blocks_and_arguments_key() {
        let (calls, _) = parse_text_calls("sure\n<tool_call>\n{\"name\": \"roll\", \"arguments\": {\"dice\": \"2d6\"}}\n</tool_call>");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "roll");
        assert_eq!(calls[0].args["dice"], "2d6");
    }

    #[test]
    fn parses_multiple_and_nested_braces() {
        let text = "TOOL_CALL: {\"name\": \"a\", \"args\": {\"x\": {\"y\": \"}\"}}}\nTOOL_CALL: {\"name\": \"b\", \"args\": {}}";
        let (calls, _) = parse_text_calls(text);
        assert_eq!(calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn parses_native_with_string_arguments() {
        let v = json!([{"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"location\": \"Oslo\"}"}}]);
        let calls = parse_native_calls(&v);
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(calls[0].args["location"], "Oslo");
    }

    #[test]
    fn leak_detection_and_strip() {
        assert!(looks_like_tool_syntax("TOOL_CALL: {\"name\": \"x\"}"));
        assert!(!looks_like_tool_syntax("just chatting"));
        assert_eq!(strip_tool_syntax("hi\nTOOL_CALL: {\"name\": \"x\"}\nthere"), "hi\nthere");
        assert_eq!(strip_tool_syntax("a <tool_call>{}</tool_call> b"), "a  b");
    }

    #[test]
    fn coercion_fills_missing_query() {
        let mut c = ToolCall { id: None, name: "search_discord".into(), args: json!({"channel": "general"}) };
        coerce_args(&mut c, "search for ham atoms");
        assert_eq!(c.args["query"], "ham atoms");
        let mut c = ToolCall { id: None, name: "search_discord".into(), args: json!({}) };
        coerce_args(&mut c, "search for ham atoms in general");
        assert_eq!(c.args["query"], "ham atoms");
        assert_eq!(c.args["channel"], "general");
        let mut c = ToolCall { id: None, name: "roll".into(), args: json!({}) };
        coerce_args(&mut c, "roll 2d6 pls");
        assert_eq!(c.args["dice"], "2d6");
        let mut c = ToolCall { id: None, name: "calculator".into(), args: json!({"expression": "1+1"}) };
        coerce_args(&mut c, "what is 2+2");
        assert_eq!(c.args["expression"], "1+1");
        assert_eq!(strip_lead_in("what is the weather in Oslo?"), "Oslo");
    }

    #[test]
    fn specs_hide_owner_tools() {
        assert!(specs(false, false).iter().all(|s| !s.owner_only && !s.extra));
        assert!(specs(true, false).iter().any(|s| s.name == "run_command"));
        assert!(specs(false, true).iter().any(|s| s.name == "events"));
        let docs = text_docs(false, false);
        assert!(docs.contains("calculator"));
        assert!(!docs.contains("run_command"));
        assert!(!docs.contains("time_in"));
        assert_eq!(native_tools(true, false).as_array().unwrap().len(), 19);
    }
}
