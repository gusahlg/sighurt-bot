pub mod message;

use crate::ai::AiProcessor;
use crate::automod::AutoMod;
use crate::chat::ChatRuntime;
use crate::commands;
use crate::database::models::get_autorole;
use crate::voice::VoiceBridge;
use sqlx::SqlitePool;
use std::sync::Arc;
use twilight_gateway::Event;
use twilight_http::Client;
use twilight_model::id::{Id, marker::UserMarker};

pub async fn handle_event(
    event: Event,
    http: Arc<Client>,
    pool: SqlitePool,
    automod: Arc<AutoMod>,
    ai: Arc<AiProcessor>,
    chat: Option<Arc<ChatRuntime>>,
    voice: Option<Arc<VoiceBridge>>,
    bot_user_id: Id<UserMarker>,
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
            // The bot/webhook filter has moved into handle_message so the
            // per-channel logger sees ALL messages (we want to capture our
            // own outgoing replies for training data).
            let http = Arc::clone(&http);
            let automod = Arc::clone(&automod);
            let ai = Arc::clone(&ai);
            let chat = chat.clone();
            let voice = voice.clone();

            tokio::spawn(async move {
                if let Err(e) = message::handle_message(
                    &msg.0,
                    &http,
                    &automod,
                    &ai,
                    chat.as_deref(),
                    voice.as_ref(),
                    bot_user_id,
                )
                .await
                {
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
