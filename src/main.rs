mod ai;
mod automod;
mod channel_state;
mod chat;
mod commands;
mod config;
mod database;
mod events;
mod voice;

use anyhow::Result;
use futures_util::StreamExt;
use std::env;
use std::sync::Arc;
use tokio::sync::watch;
use twilight_gateway::{
    stream::{self, ShardEventStream},
    CloseFrame, Config as GatewayConfig, Intents, Shard,
};
use twilight_http::Client;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::id::Id;

use crate::ai::{AiConfig, AiProcessor, AiProviderConfig};
use crate::automod::AutoMod;
use crate::channel_state::ChannelState;
use crate::chat::{ChatClient, ChatRuntime};
use crate::config::Config;
use crate::voice::VoiceBridge;
use discord_bot::scrape;

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
        Ok(key) => match ChatClient::new(&config.chat, key) {
            Ok(client) => {
                let runtime = ChatRuntime::new(client, &config.chat);
                tracing::info!(
                    "Chat runtime ready (initial = {}); LLM endpoint = {}; admins = {}",
                    if config.chat.enabled { "ON" } else { "OFF" },
                    config.chat.endpoint_url,
                    config.chat.admin_user_ids.len(),
                );
                Some(runtime)
            }
            Err(e) => {
                tracing::error!("Failed to build chat client: {}; chat disabled", e);
                None
            }
        },
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
    let chat_min_reply_gap =
        std::time::Duration::from_secs(config.chat.min_seconds_between_replies);

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
        | Intents::GUILD_VOICE_STATES;

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

    tracing::info!("Bot is starting...");

    // Main event loop with shutdown handling
    let mut event_shutdown_rx = shutdown_rx.clone();
    loop {
        tokio::select! {
            next = stream.next() => {
                let event = match next {
                    Some((_, Ok(event))) => event,
                    Some((_, Err(e))) => {
                        tracing::error!("Shard error: {}", e);
                        if e.is_fatal() {
                            tracing::error!("Fatal shard error, shutting down");
                            break;
                        }
                        continue;
                    }
                    None => break,
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

    // Close all shards cleanly
    tracing::info!("Closing {} shard(s)...", shard_count);
    for shard in &mut shards {
        if let Err(e) = shard.close(CloseFrame::NORMAL).await {
            tracing::warn!("Error closing shard: {}", e);
        }
    }

    // Close the database pool
    tracing::info!("Closing database pool...");
    pool.close().await;

    tracing::info!("Bot shut down cleanly");
    Ok(())
}
