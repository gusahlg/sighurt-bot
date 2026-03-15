mod ai;
mod automod;
mod commands;
mod config;
mod database;
mod events;

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
use twilight_model::id::Id;

use crate::ai::{AiConfig, AiProcessor, AiProviderConfig};
use crate::automod::AutoMod;
use crate::config::Config;

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

    // Load configuration
    let config = Config::load("config.toml").unwrap_or_default();
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

    // Create HTTP client
    let http = Arc::new(Client::new(token.clone()));

    // Register slash commands
    commands::register_commands(&http, Id::new(application_id)).await?;

    // Create automod
    let automod = Arc::new(AutoMod::new(pool.clone(), Arc::clone(&http)));

    // Configure gateway intents
    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MESSAGES
        | Intents::MESSAGE_CONTENT
        | Intents::DIRECT_MESSAGES;

    // Create gateway config
    let gateway_config = GatewayConfig::new(token.clone(), intents);

    // Create shards
    let mut shards: Vec<Shard> =
        stream::create_recommended(&http, gateway_config, |_, builder| builder.build())
            .await?
            .collect();

    let shard_count = shards.len();
    tracing::info!("Created {} shard(s)", shard_count);

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

                let http = Arc::clone(&http);
                let pool = pool.clone();
                let automod = Arc::clone(&automod);
                let ai = Arc::clone(&ai_processor);

                tokio::spawn(async move {
                    events::handle_event(event, http, pool, automod, ai).await;
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
