//! Run one of Sig's tools directly, for tests and for the training-data
//! builder (so synthetic tool traces carry REAL tool output shapes).
//!
//!   tool_probe calculator '{"expression": "17*23"}'
//!   SIG_DATA_ROOT=data/channels tool_probe lookup_rule '{"label": "4"}'
//!
//! Prints the tool text to stdout; exit code 1 when the tool reported an
//! error. Discord-backed tools that need the live API (who_is join date,
//! server_status) degrade gracefully without a token.

use discord_bot::agent::tools::discord::Directory;
use discord_bot::agent::tools::memory::ReminderStore;
use discord_bot::agent::tools::{self, ToolCall, ToolCtx};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: tool_probe <tool> [json-args]");
        std::process::exit(2);
    }
    let name = args[1].clone();
    let json_args: serde_json::Value = args
        .get(2)
        .map(|s| serde_json::from_str(s).unwrap_or_else(|e| {
            eprintln!("bad json args: {e}");
            std::process::exit(2);
        }))
        .unwrap_or_else(|| serde_json::json!({}));
    let data_root = PathBuf::from(std::env::var("SIG_DATA_ROOT").unwrap_or_else(|_| "data/channels".into()));
    let memory_dir = PathBuf::from(std::env::var("SIG_MEMORY_DIR").unwrap_or_else(|_| "/tmp/sig-tool-probe".into()));
    let guild_id: u64 = std::env::var("SIG_GUILD_ID").ok().and_then(|g| g.parse().ok()).unwrap_or(1367116390728994927);
    let rules_channel: u64 = std::env::var("SIG_RULES_CHANNEL").ok().and_then(|g| g.parse().ok()).unwrap_or(1405453745512386581);
    let token = std::env::var("DISCORD_TOKEN").unwrap_or_else(|_| "probe".into());
    let http = twilight_http::Client::new(token.clone());
    let directory = Directory::new();
    if token != "probe" {
        directory.refresh(&http).await;
    } else if let Ok(names) = std::env::var("SIG_CHANNEL_NAMES") {
        // Offline channel-name map: a JSON file {"channels": {"<id>": {"name": ..}}}.
        if let Ok(text) = std::fs::read_to_string(names) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                directory.load_offline(guild_id, &v);
            }
        }
    }
    let reminders = ReminderStore::load(memory_dir.join("reminders.jsonl"));
    let web = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client");
    let feeds = vec!["https://feeds.bbci.co.uk/news/world/rss.xml".to_string(), "https://hnrss.org/frontpage".to_string()];
    let ctx = ToolCtx {
        http: &http,
        web: &web,
        directory: &directory,
        reminders: &reminders,
        web_search: None,
        data_root: &data_root,
        memory_dir: &memory_dir,
        rules_channel_id: Some(rules_channel),
        news_feeds: &feeds,
        guild_id: Some(guild_id),
        channel_id: std::env::var("SIG_CHANNEL_ID").ok().and_then(|c| c.parse().ok()).unwrap_or(1405453469032120340),
        user_id: std::env::var("SIG_USER_ID").ok().and_then(|c| c.parse().ok()).unwrap_or(4242),
        user_name: "tester",
        bot_user_id: 1440038621230010418,
        is_owner: std::env::var("SIG_OWNER").map(|v| v == "1").unwrap_or(false),
        sudo_password: None,
    };
    let call = ToolCall { id: None, name, args: json_args };
    let out = tools::run(&call, &ctx).await;
    println!("{}", out.text);
    if out.is_error {
        std::process::exit(1);
    }
}
