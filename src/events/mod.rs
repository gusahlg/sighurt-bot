pub mod message;

use crate::ai::AiProcessor;
use crate::automod::AutoMod;
use crate::commands;
use crate::database::models::get_autorole;
use sqlx::SqlitePool;
use std::sync::Arc;
use twilight_gateway::Event;
use twilight_http::Client;

pub async fn handle_event(
    event: Event,
    http: Arc<Client>,
    pool: SqlitePool,
    automod: Arc<AutoMod>,
    ai: Arc<AiProcessor>,
) {
    match event {
        Event::Ready(ready) => {
            tracing::info!(
                "Bot is ready! Logged in as {}#{}",
                ready.user.name,
                ready.user.discriminator
            );
        }
        Event::InteractionCreate(interaction) => {
            let http = Arc::clone(&http);
            let pool = pool.clone();

            tokio::spawn(async move {
                if let Err(e) = commands::handle_interaction(interaction.0, http, pool).await {
                    tracing::error!("Error handling interaction: {}", e);
                }
            });
        }
        Event::MessageCreate(msg) => {
            // Skip bot and webhook messages
            if msg.author.bot || msg.webhook_id.is_some() {
                return;
            }

            let http = Arc::clone(&http);
            let automod = Arc::clone(&automod);
            let ai = Arc::clone(&ai);

            tokio::spawn(async move {
                if let Err(e) = message::handle_message(&msg.0, &http, &automod, &ai).await {
                    tracing::error!("Error handling message: {}", e);
                }
            });
        }
        Event::MemberAdd(member) => {
            let automod = Arc::clone(&automod);
            let http = Arc::clone(&http);
            let pool = pool.clone();

            tokio::spawn(async move {
                // Check for raid
                match automod.check_member_join(&member).await {
                    Ok(action) => {
                        if let crate::automod::AutoModAction::RaidDetected = action {
                            tracing::warn!("Raid detected in guild {}", member.guild_id);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error checking member join: {}", e);
                    }
                }

                // Apply auto-role if configured
                match get_autorole(&pool, member.guild_id).await {
                    Ok(Some(role_id)) => {
                        if let Err(e) = http
                            .add_guild_member_role(member.guild_id, member.user.id, role_id)
                            .await
                        {
                            tracing::error!(
                                "Failed to add autorole {} to user {}: {}",
                                role_id,
                                member.user.id,
                                e
                            );
                        } else {
                            tracing::info!(
                                "Added autorole {} to new member {} in guild {}",
                                role_id,
                                member.user.id,
                                member.guild_id
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("Failed to get autorole: {}", e);
                    }
                }
            });
        }
        Event::GatewayReconnect => {
            tracing::info!("Gateway reconnecting...");
        }
        Event::Resumed => {
            tracing::info!("Session resumed");
        }
        _ => {}
    }
}
