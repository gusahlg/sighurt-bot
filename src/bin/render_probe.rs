//! Render the exact prompt the bot would send, without a model or Discord.
//!
//! Reads JSONL on stdin, one request per line:
//!   {"request": {"user": "zunabaro", "user_id": 1, "input": "what is rule 4",
//!                "context": [{"user": "walnutty2", "text": "hi", "is_self": false, "is_bot": false}],
//!                "reply_to": null},
//!    "situation": {"now": "Thursday 2026-09-18 09:00 CEST", "guild_name": "Coolness Interactive Discord",
//!                  "channel_name": "general", "is_dm": false, "member_count": 40, "online_count": 8,
//!                  "voice": []},
//!    "is_owner": false, "format": "native", "persona_file": "persona/system_prompt.txt"}
//! and writes one JSON object per line: {"messages": [...], "tools": [...]}.
//!
//! The training-data builder pipes every row through this so train == serve
//! byte for byte (system prompt, situation line, message shapes, tool specs).

use discord_bot::agent::prompt::{self, Situation, ToolFormat, DEFAULT_PERSONA};
use discord_bot::agent::tools;
use discord_bot::chat::{ChatContextMessage, ChatReplyTo, ChatRequest};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0)
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut personas: HashMap<String, String> = HashMap::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(out, "{}", json!({"error": format!("bad json: {e}")}));
                continue;
            }
        };
        let persona_file = s(&v, "persona_file");
        let persona = if persona_file.is_empty() {
            DEFAULT_PERSONA.to_string()
        } else {
            personas
                .entry(persona_file.clone())
                .or_insert_with(|| std::fs::read_to_string(&persona_file).unwrap_or_else(|_| DEFAULT_PERSONA.to_string()))
                .clone()
        };
        let format = ToolFormat::parse(&s(&v, "format")).unwrap_or(ToolFormat::Native);
        let is_owner = b(&v, "is_owner");
        let sudo = b(&v, "sudo");
        let r = &v["request"];
        let context: Vec<ChatContextMessage> = r["context"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| ChatContextMessage {
                message_id: u(c, "message_id"),
                user: s(c, "user"),
                user_id: u(c, "user_id"),
                text: s(c, "text"),
                is_bot: b(c, "is_bot"),
                is_self: b(c, "is_self"),
                reply_to_message_id: c.get("reply_to_message_id").and_then(Value::as_u64),
            })
            .collect();
        let reply_to = r.get("reply_to").filter(|x| x.is_object()).map(|x| ChatReplyTo {
            message_id: u(x, "message_id"),
            user: s(x, "user"),
            user_id: u(x, "user_id"),
            text: s(x, "text"),
            is_bot: b(x, "is_bot"),
            is_self: b(x, "is_self"),
        });
        let request = ChatRequest {
            channel_id: u(r, "channel_id"),
            user: s(r, "user"),
            user_id: u(r, "user_id"),
            user_is_bot: b(r, "user_is_bot"),
            input: s(r, "input"),
            context,
            reply_to,
            web_search: None,
            react: b(r, "react"),
        };
        let sv = &v["situation"];
        let situation = Situation {
            now: s(sv, "now"),
            guild_name: sv.get("guild_name").and_then(Value::as_str).map(str::to_string),
            channel_name: sv.get("channel_name").and_then(Value::as_str).map(str::to_string),
            is_dm: b(sv, "is_dm"),
            member_count: sv.get("member_count").and_then(Value::as_u64),
            online_count: sv.get("online_count").and_then(Value::as_u64),
            voice: sv["voice"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        };
        let system = prompt::system_prompt(&persona, &situation, &request.user, format, is_owner, sudo);
        let messages = if request.react {
            prompt::build_react_messages(&system, &request, &situation)
        } else {
            prompt::build_messages(&system, &request, format, &situation)
        };
        let tools = match format {
            ToolFormat::Native => tools::native_tools(is_owner),
            _ => Value::Null,
        };
        let _ = writeln!(out, "{}", json!({"messages": messages, "tools": tools}));
    }
}
