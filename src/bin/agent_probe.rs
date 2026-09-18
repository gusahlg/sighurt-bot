//! Offline harness for Sig's agent loop: run one message (or a batch) through
//! the persona + tools against a model server, without Discord.
//!
//!   SIG_ENDPOINT=http://127.0.0.1:8081 SIG_API_KEY=… \
//!   cargo run --bin agent_probe -- "what is 17*23"
//!
//! Env:
//!   SIG_ENDPOINT     chat-completions server (default http://127.0.0.1:8081)
//!   SIG_API_KEY      bearer/X-API-Key (default: none)
//!   SIG_TOOL_FORMAT  native | text | none (default text)
//!   SIG_OWNER=1      pretend the speaker is the owner (unlocks run_command)
//!   SIG_USER         speaker display name (default tester)
//!   SIG_DATA_ROOT    channel-log root (default data/channels)
//!   SIG_PERSONA_FILE persona text (default: built-in)
//!   SIG_BATCH=file   one message per line; prints "INPUT\t-> REPLY" per line
//!   DISCORD_TOKEN    optional; refreshes channel names for search_discord
//!   SIG_GUILD_ID     guild id for the log tools (default 1367116390728994927)

use discord_bot::agent::backend::{OpenAiBackend, Sampling};
use discord_bot::agent::prompt::{Situation, ToolFormat, DEFAULT_PERSONA};
use discord_bot::agent::tools::discord::Directory;
use discord_bot::agent::tools::memory::ReminderStore;
use discord_bot::agent::{Agent, AgentConfig};
use discord_bot::chat::ChatRequest;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("discord_bot=info".parse()?))
        .with_writer(std::io::stderr)
        .init();
    let endpoint = std::env::var("SIG_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    let api_key = std::env::var("SIG_API_KEY").unwrap_or_default();
    let format = ToolFormat::parse(&std::env::var("SIG_TOOL_FORMAT").unwrap_or_else(|_| "text".into())).unwrap_or(ToolFormat::Text);
    let owner = std::env::var("SIG_OWNER").map(|v| v == "1").unwrap_or(false);
    let user = std::env::var("SIG_USER").unwrap_or_else(|_| "tester".into());
    let guild_id: u64 = std::env::var("SIG_GUILD_ID").ok().and_then(|g| g.parse().ok()).unwrap_or(1367116390728994927);
    let persona = std::env::var("SIG_PERSONA_FILE").ok().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_else(|| DEFAULT_PERSONA.to_string());
    let cfg = AgentConfig {
        tool_format: format,
        max_tool_iters: 4,
        max_reply_tokens: 220,
        max_tool_tokens: 260,
        sampling: Sampling { temperature: 0.5, top_p: 0.92, top_k: 40, min_p: 0.05, repeat_penalty: 1.15, repeat_last_n: 256, presence_penalty: 0.0 },
        owner_user_id: 367334632515043329,
        rules_channel_id: Some(1405453745512386581),
        data_root: PathBuf::from(std::env::var("SIG_DATA_ROOT").unwrap_or_else(|_| "data/channels".into())),
        memory_dir: PathBuf::from(std::env::var("SIG_MEMORY_DIR").unwrap_or_else(|_| "/tmp/sig-probe-memory".into())),
        news_feeds: vec!["https://feeds.bbci.co.uk/news/world/rss.xml".into(), "https://hnrss.org/frontpage".into()],
        persona,
        legacy_render: false,
        extra_tools: std::env::var("SIG_EXTRA_TOOLS").map(|v| v == "1").unwrap_or(false),
    };
    let backend = OpenAiBackend::new(&endpoint, &api_key, "sig", 180, false)?;
    let directory = Arc::new(Directory::new());
    let reminders = Arc::new(ReminderStore::load(cfg.memory_dir.join("reminders.jsonl")));
    let agent = Agent::new(cfg, backend, Arc::clone(&directory), reminders, None)?;
    let token = std::env::var("DISCORD_TOKEN").unwrap_or_else(|_| "probe".into());
    let http = twilight_http::Client::new(token.clone());
    if token != "probe" {
        directory.refresh(&http).await;
        eprintln!("directory refreshed; #general = {:?}", directory.resolve_channel(guild_id, "general"));
    }
    let situation = Situation {
        now: discord_bot::agent::tools::system::now_line(),
        guild_name: Some("Coolness Interactive Discord".into()),
        channel_name: Some("general".into()),
        is_dm: false,
        member_count: Some(40),
        online_count: Some(8),
        voice: vec![],
    };
    let user_id = if owner { 367334632515043329 } else { 4242 };
    let inputs: Vec<String> = match std::env::var("SIG_BATCH") {
        Ok(path) => std::fs::read_to_string(path)?.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect(),
        Err(_) => vec![std::env::args().skip(1).collect::<Vec<_>>().join(" ")],
    };
    for input in inputs {
        let request = ChatRequest {
            channel_id: 1405453469032120340,
            user: user.clone(),
            user_id,
            user_is_bot: false,
            input: input.clone(),
            context: Vec::new(),
            reply_to: None,
            web_search: None,
            react: false,
        };
        let started = std::time::Instant::now();
        match agent.reply(&http, &request, &situation, 1440038621230010418, Some(guild_id)).await {
            Ok(reply) => println!("{input}\t-> {}\t({:.1}s)", reply.replace('\n', " / "), started.elapsed().as_secs_f32()),
            Err(e) => println!("{input}\t-> ERROR {e:#}"),
        }
    }
    Ok(())
}
