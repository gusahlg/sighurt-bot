mod ai;
mod automod;
mod channel_state;
mod commands;
mod database;
mod events;
mod voice;

// Shared with the probe/scraper binaries through the library crate; the
// `use` bindings keep `crate::chat::…` paths valid in the binary's modules.
use discord_bot::{agent, chat, config, reply_filter, web_search};

use anyhow::Result;
use futures_util::StreamExt;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use twilight_gateway::{
    stream::{self, ShardEventStream},
    CloseFrame, Config as GatewayConfig, Intents, Shard,
};
use twilight_http::Client;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::id::Id;

use crate::agent::backend::{OpenAiBackend, Sampling};
use crate::agent::prompt::{ToolFormat, DEFAULT_PERSONA};
use crate::agent::tools::discord::Directory;
use crate::agent::tools::memory::ReminderStore;
use crate::agent::{Agent, AgentConfig};
use crate::ai::{AiConfig, AiProcessor, AiProviderConfig};
use crate::automod::AutoMod;
use crate::channel_state::ChannelState;
use crate::chat::{Backend, ChatClient, ChatRuntime};
use crate::config::{ChatConfig, Config};
use crate::voice::VoiceBridge;
use crate::web_search::WebSearchClient;
use discord_bot::scrape;
use std::path::PathBuf;

/// Pick the brain from `chat.backend`: the classic tensor-ash `/chat` client,
/// or the in-process agent over an OpenAI-compatible / raw completion server.
fn build_backend(cfg: &ChatConfig, api_key: &str, search: Option<WebSearchClient>) -> Result<Backend> {
    match cfg.backend.trim() {
        "sighurt" => Ok(Backend::Sighurt(ChatClient::new(cfg, api_key.to_string())?)),
        kind @ ("openai" | "completion") => {
            let persona = match std::fs::read_to_string(&cfg.persona_file) {
                Ok(text) if !text.trim().is_empty() => {
                    tracing::info!("Persona loaded from {}", cfg.persona_file);
                    text
                }
                _ => {
                    tracing::info!("Persona file {} missing; using the built-in persona", cfg.persona_file);
                    DEFAULT_PERSONA.to_string()
                }
            };
            let tool_format = ToolFormat::parse(&cfg.tool_format).unwrap_or(ToolFormat::Native);
            let agent_cfg = AgentConfig {
                tool_format: if kind == "completion" { ToolFormat::None } else { tool_format },
                max_tool_iters: cfg.max_tool_iters,
                max_reply_tokens: cfg.max_reply_tokens,
                max_tool_tokens: cfg.max_tool_tokens,
                sampling: Sampling {
                    temperature: cfg.temperature,
                    top_p: cfg.top_p,
                    top_k: cfg.top_k,
                    min_p: cfg.min_p,
                    repeat_penalty: cfg.repeat_penalty,
                    repeat_last_n: cfg.repeat_last_n,
                    presence_penalty: 0.0,
                },
                owner_user_id: cfg.owner_user_id,
                rules_channel_id: cfg.rules_channel_id,
                data_root: PathBuf::from("data/channels"),
                memory_dir: PathBuf::from(&cfg.memory_dir),
                news_feeds: cfg.news_feeds.clone(),
                persona,
                legacy_render: kind == "completion",
                extra_tools: cfg.extra_tools,
            };
            let backend = OpenAiBackend::new(&cfg.endpoint_url, api_key, &cfg.model, cfg.request_timeout_secs, cfg.thinking)?;
            let reminders = Arc::new(ReminderStore::load(PathBuf::from(&cfg.memory_dir).join("reminders.jsonl")));
            let agent = Agent::new(agent_cfg, backend, Arc::new(Directory::new()), reminders, search)?;
            Ok(Backend::Agent(Arc::new(agent)))
        }
        other => anyhow::bail!("unknown chat.backend {other:?}"),
    }
}

/// How long the merged shard stream may go silent before we assume the
/// gateway is wedged. A healthy connection carries at least a heartbeat ACK
/// every ~41s, so minutes of total silence can only mean a dead shard.
const GATEWAY_STALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Cap on closing a shard during shutdown — the close frame goes over the
/// same (possibly dead) socket that made us shut down in the first place.
const SHARD_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file
    dotenvy::dotenv().ok();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("discord_bot=info".parse()?)
                .add_directive("twilight=info".parse()?),
        )
        .init();

    // Load configuration. A MISSING config.toml is fine (defaults). A config
    // that is PRESENT but fails to parse is FATAL: silently booting defaults
    // there disables chat and empties the admin list with no signal, leaving
    // production quietly broken. Log the parse error and exit non-zero.
    let config = match Config::load("config.toml") {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("Failed to load config.toml: {:#}", e);
            return Err(e);
        }
    };
    if let Err(e) = config.validate() {
        tracing::error!("Configuration validation failed: {}", e);
        return Err(e);
    }
    tracing::info!("Configuration loaded and validated");

    // Initialize AI processor (if enabled)
    let ai_config = AiConfig {
        enabled: config.ai.enabled,
        max_concurrent: config.ai.max_concurrent,
        queue_capacity: config.ai.queue_capacity,
        timeout_secs: config.ai.timeout_secs,
        provider: config
            .ai
            .model_path
            .as_ref()
            .map(|p| AiProviderConfig::Local {
                model_path: p.clone(),
            })
            .unwrap_or(AiProviderConfig::Mock),
    };
    let ai_processor = Arc::new(AiProcessor::new(ai_config));
    if config.ai.enabled {
        tracing::info!("AI mode enabled");
    }

    // Build the chat runtime if LLM_API_KEY is present. `chat.enabled` controls
    // the *initial* state of the runtime toggle, not whether the runtime exists,
    // so admins listed in `chat.admin_user_ids` can flip it via `!ai on`.
    let chat_runtime: Option<Arc<ChatRuntime>> = match env::var("LLM_API_KEY") {
        Ok(key) => {
            let search = match WebSearchClient::new(&config.chat, env::var("BRAVE_SEARCH_API_KEY").ok()) {
                Ok(search) => search,
                Err(e) => {
                    tracing::error!("Failed to build web-search client: {}; live search disabled", e);
                    None
                }
            };
            // The outgoing-reply word filter belongs to the chat runtime: a
            // filter build error (bad judge config) disables chat entirely
            // rather than silently running unfiltered.
            let built = reply_filter::ReplyFilter::from_config(&config.filter).and_then(|filter| {
                let backend = build_backend(&config.chat, &key, search.clone())?;
                Ok((backend, filter))
            });
            match built {
                Ok((backend, filter)) => {
                    tracing::info!("{}", filter.boot_summary());
                    let runtime = ChatRuntime::new(backend, &config.chat, search, filter);
                    tracing::info!(
                        "Chat runtime ready (initial = {}); brain = {}; web search = {}",
                        if config.chat.enabled { "ON" } else { "OFF" },
                        runtime.backend_label(),
                        if config.chat.web_search_enabled { "ON" } else { "OFF" },
                    );
                    Some(runtime)
                }
                Err(e) => {
                    tracing::error!("Failed to build chat backend: {:#}; chat disabled", e);
                    None
                }
            }
        }
        Err(_) => {
            tracing::info!("LLM_API_KEY unset; chat runtime disabled");
            None
        }
    };

    // Get Discord token
    let token = env::var("DISCORD_TOKEN").map_err(|_| {
        anyhow::anyhow!("DISCORD_TOKEN environment variable not set. See .env.example for setup.")
    })?;
    if token.is_empty() {
        return Err(anyhow::anyhow!("DISCORD_TOKEN must not be empty"));
    }

    let application_id: u64 = env::var("DISCORD_APPLICATION_ID")
        .map_err(|_| {
            anyhow::anyhow!(
                "DISCORD_APPLICATION_ID environment variable not set. See .env.example for setup."
            )
        })?
        .parse()
        .map_err(|_| anyhow::anyhow!("DISCORD_APPLICATION_ID must be a valid u64"))?;
    if application_id == 0 {
        return Err(anyhow::anyhow!("DISCORD_APPLICATION_ID must be non-zero"));
    }

    // Initialize database
    let database_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:data/bot.db".to_string());
    let pool = database::init_database(&database_url).await?;
    tracing::info!("Database initialized");

    // Create HTTP client. The default AllowedMentions is EMPTY (pings nobody)
    // so every send site — say, automod, voice — is ping-safe unless it
    // explicitly overrides per-request, which only the chat reply path does.
    let http = Arc::new(
        Client::builder()
            .token(token.clone())
            .default_allowed_mentions(AllowedMentions::default())
            .build(),
    );

    // Shared per-channel state: bot-chain loop guard + recent-authors map.
    let channel_state = Arc::new(ChannelState::new());

    // Minimum spacing between chat replies per channel (flood/DoS guard on the
    // chat trigger; DMs aren't covered by automod). Threaded into the message
    // handler so it applies to every trigger.
    let chat_min_reply_gap = Duration::from_secs(config.chat.min_seconds_between_replies);

    // Fetch bot user id so we can detect @-mentions in messages.
    let bot_user_id = http
        .current_user()
        .await
        .map_err(|e| anyhow::anyhow!("failed to fetch current user: {e}"))?
        .model()
        .await
        .map_err(|e| anyhow::anyhow!("failed to decode current user: {e}"))?
        .id;
    tracing::info!("Bot user id = {}", bot_user_id);

    // Register slash commands
    commands::register_commands(&http, Id::new(application_id)).await?;

    // Agent housekeeping: keep the guild directory (channel names, member
    // counts) fresh and fire due reminders. Both are cheap periodic tasks.
    if let Some(agent) = chat_runtime.as_ref().and_then(|c| c.agent()).cloned() {
        let dir_http = Arc::clone(&http);
        let dir_agent = Arc::clone(&agent);
        tokio::spawn(async move {
            loop {
                dir_agent.directory.refresh(&dir_http).await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        let rem_http = Arc::clone(&http);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                let now = chrono::Utc::now().timestamp();
                for r in agent.reminders.take_due(now) {
                    let allowed = AllowedMentions {
                        parse: Vec::new(),
                        replied_user: false,
                        roles: Vec::new(),
                        users: vec![Id::new(r.user_id)],
                    };
                    let text = format!("⏰ <@{}> reminder: {}", r.user_id, r.text);
                    match rem_http.create_message(Id::new(r.channel_id)).allowed_mentions(Some(&allowed)).content(&text) {
                        Ok(builder) => {
                            if let Err(e) = builder.await {
                                tracing::warn!("reminder #{} failed to post: {e}", r.id);
                            }
                        }
                        Err(e) => tracing::warn!("reminder #{} invalid content: {e}", r.id),
                    }
                }
            }
        });
    }

    // Create automod
    let automod = Arc::new(AutoMod::new(pool.clone(), Arc::clone(&http)));

    // Configure gateway intents. GUILD_VOICE_STATES is required for voice
    // mode — Songbird needs voice state + voice server updates to join voice
    // channels, and we look up the invoker's current voice channel via that
    // intent's events.
    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MESSAGES
        | Intents::MESSAGE_CONTENT
        | Intents::DIRECT_MESSAGES
        | Intents::GUILD_VOICE_STATES
        // Reactions feed the training corpus and teach Sig when to react.
        | Intents::GUILD_MESSAGE_REACTIONS
        | Intents::DIRECT_MESSAGE_REACTIONS;

    // Create gateway config
    let gateway_config = GatewayConfig::new(token.clone(), intents);

    // Create shards
    let mut shards: Vec<Shard> =
        stream::create_recommended(&http, gateway_config, |_, builder| builder.build())
            .await?
            .collect();

    let shard_count = shards.len();
    tracing::info!("Created {} shard(s)", shard_count);

    // Initialise voice mode if ELEVENLABS_AGENT_ID is set. Without it the bot
    // runs in text-only mode — the `!voice` command path simply won't fire.
    let voice_bridge: Option<Arc<VoiceBridge>> = match voice::VoiceConfig::from_env() {
        Some(cfg) => match voice::init(bot_user_id, &shards, cfg) {
            Ok(bridge) => {
                tracing::info!("Voice mode enabled (ElevenLabs ConvAI bridge ready)");
                Some(bridge)
            }
            Err(e) => {
                tracing::error!("Voice init failed: {}; running text-only", e);
                None
            }
        },
        None => {
            tracing::info!("ELEVENLABS_AGENT_ID unset; voice mode disabled");
            None
        }
    };

    // Set up graceful shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn signal handler for SIGTERM and SIGINT
    tokio::spawn(async move {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Failed to register SIGTERM handler");
        let sigint = tokio::signal::ctrl_c();

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, initiating graceful shutdown...");
            }
            _ = sigint => {
                tracing::info!("Received SIGINT, initiating graceful shutdown...");
            }
        }

        let _ = shutdown_tx.send(true);
    });

    // Spawn cleanup task with shutdown awareness
    let automod_cleanup = Arc::clone(&automod);
    let mut cleanup_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                    automod_cleanup.spam.cleanup();
                    automod_cleanup.raid.cleanup();
                    tracing::debug!("Cleanup task completed");
                }
                _ = cleanup_shutdown_rx.changed() => {
                    tracing::info!("Cleanup task shutting down");
                    break;
                }
            }
        }
    });

    // Periodic history catch-up: ~30s after boot run a full scrape (backfill
    // where needed + forward pass over every guild, channel and thread) so
    // any gap from time spent offline heals, then repeat every
    // `scrape.interval_hours`. Duplicate-safety against live gateway logging
    // is handled inside channel_log: both paths funnel through the
    // cursor-guarded append under one process-wide mutex.
    if config.scrape.enabled {
        let scrape_http = Arc::clone(&http);
        let interval = tokio::time::Duration::from_secs(
            config.scrape.interval_hours.saturating_mul(3600),
        );
        let mut scrape_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut delay = tokio::time::Duration::from_secs(30);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {
                        tracing::info!("History catch-up scrape starting");
                        // scrape_all logs its own per-run summary (channels
                        // scanned, new messages, skipped channels).
                        if let Err(e) = scrape::scrape_all(Arc::clone(&scrape_http)).await {
                            tracing::warn!("History catch-up scrape failed: {:#}", e);
                        }
                        delay = interval;
                    }
                    _ = scrape_shutdown_rx.changed() => {
                        tracing::info!("Catch-up scrape task shutting down");
                        break;
                    }
                }
            }
        });
        tracing::info!(
            "Catch-up scraper enabled (first run in ~30s, then every {}h)",
            config.scrape.interval_hours
        );
    } else {
        tracing::info!("Catch-up scraper disabled (scrape.enabled = false)");
    }

    // Create shard event stream
    let mut stream = ShardEventStream::new(shards.iter_mut());

    // Tell systemd we're up (no-op outside systemd), and keep its watchdog
    // fed from a dedicated task. If the whole runtime ever deadlocks, the
    // pings stop and systemd kills + restarts us.
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
    let mut watchdog_usec = 0;
    if sd_notify::watchdog_enabled(false, &mut watchdog_usec) {
        let interval = Duration::from_micros(watchdog_usec / 2);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
            }
        });
        tracing::info!("systemd watchdog enabled ({}s)", watchdog_usec / 2_000_000);
    }

    tracing::info!("Bot is starting...");

    // Main event loop with shutdown handling.
    //
    // The timeout around `stream.next()` is a stall watchdog. twilight
    // 0.15's zombied-connection recovery sends a close frame and then waits
    // for the peer to hang up, disabling its own heartbeat timer in the
    // process (`Shard::disconnect`). If that close frame dies on a
    // dead-but-ESTABLISHED TCP connection, no timer is left to ever wake
    // the shard and the stream goes silent forever while the process looks
    // healthy (observed 2026-08-02: "connection is failed or zombied", then
    // 24h of deafness). Any abnormal end here must exit non-zero so systemd
    // restarts us into a clean identify.
    let mut event_shutdown_rx = shutdown_rx.clone();
    let mut abnormal_exit: Option<String> = None;
    loop {
        tokio::select! {
            next = tokio::time::timeout(GATEWAY_STALL_TIMEOUT, stream.next()) => {
                let event = match next {
                    Err(_) => {
                        abnormal_exit = Some(format!(
                            "gateway silent for {}s (zombied connection?)",
                            GATEWAY_STALL_TIMEOUT.as_secs()
                        ));
                        break;
                    }
                    Ok(Some((_, Ok(event)))) => event,
                    Ok(Some((_, Err(e)))) => {
                        // Display is terse ("websocket connection error");
                        // the useful detail lives in the error's source.
                        tracing::error!("Shard error: {e} ({e:?})");
                        if e.is_fatal() {
                            abnormal_exit = Some(format!("fatal shard error: {e}"));
                            break;
                        }
                        continue;
                    }
                    Ok(None) => {
                        abnormal_exit = Some("shard event stream ended".to_string());
                        break;
                    }
                };

                // Feed Songbird every gateway event so it sees voice state /
                // voice server updates. Cheap when voice is disabled (the
                // Arc is just None) and required when it's on — Songbird's
                // `join()` future awaits these events. We also update our
                // own voice-state cache off VoiceStateUpdate so `!voice
                // join` can look up which channel the invoker is in.
                if let Some(bridge) = voice_bridge.as_ref() {
                    voice::process_gateway_event(bridge, &event).await;
                    if let twilight_gateway::Event::VoiceStateUpdate(vs) = &event {
                        bridge.voice_states.apply_update(&vs.0);
                    }
                }

                let http = Arc::clone(&http);
                let pool = pool.clone();
                let automod = Arc::clone(&automod);
                let ai = Arc::clone(&ai_processor);
                let chat = chat_runtime.clone();
                let voice = voice_bridge.clone();
                let channel_state = Arc::clone(&channel_state);

                tokio::spawn(async move {
                    events::handle_event(
                        event,
                        http,
                        pool,
                        automod,
                        ai,
                        chat,
                        voice,
                        bot_user_id,
                        channel_state,
                        chat_min_reply_gap,
                    )
                    .await;
                });
            }
            _ = event_shutdown_rx.changed() => {
                tracing::info!("Shutdown signal received, exiting event loop");
                break;
            }
        }
    }

    // Graceful shutdown: drop the stream to release the mutable borrow on shards
    drop(stream);
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);

    // Close all shards cleanly. Time-boxed: when we're here because the
    // connection wedged, this close frame would go into the same dead socket.
    tracing::info!("Closing {} shard(s)...", shard_count);
    for shard in &mut shards {
        match tokio::time::timeout(SHARD_CLOSE_TIMEOUT, shard.close(CloseFrame::NORMAL)).await {
            Ok(Err(e)) => tracing::warn!("Error closing shard: {}", e),
            Err(_) => tracing::warn!("Timed out closing shard"),
            Ok(Ok(_)) => {}
        }
    }

    // Close the database pool
    tracing::info!("Closing database pool...");
    pool.close().await;

    if let Some(reason) = abnormal_exit {
        // Exit non-zero so systemd's Restart= policy brings us back up.
        tracing::error!("Exiting for restart: {reason}");
        return Err(anyhow::anyhow!(reason));
    }

    tracing::info!("Bot shut down cleanly");
    Ok(())
}
