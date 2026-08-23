pub mod message;

use crate::ai::AiProcessor;
use crate::automod::AutoMod;
use crate::channel_state::ChannelState;
use crate::chat::ChatRuntime;
use crate::commands;
use crate::database::models::get_autorole;
use crate::voice::VoiceBridge;
use discord_bot::channel_log;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;
use twilight_gateway::Event;
use twilight_http::Client;
use twilight_model::channel::message::ReactionType;
use twilight_model::id::{Id, marker::UserMarker};

/// The loggable text form of a reaction emoji: the literal character(s) for
/// unicode emoji, the bare name for custom guild emoji. Names stay bare (no
/// `:colons:`) so downstream corpus cleaning doesn't rewrite them.
fn emoji_text(emoji: &ReactionType) -> String {
    match emoji {
        ReactionType::Unicode { name } => name.clone(),
        ReactionType::Custom { name, .. } => name.clone().unwrap_or_else(|| "custom".to_string()),
    }
}

pub async fn handle_event(
    event: Event,
    http: Arc<Client>,
    pool: SqlitePool,
    automod: Arc<AutoMod>,
    ai: Arc<AiProcessor>,
    chat: Option<Arc<ChatRuntime>>,
    voice: Option<Arc<VoiceBridge>>,
    bot_user_id: Id<UserMarker>,
    channel_state: Arc<ChannelState>,
    chat_min_reply_gap: Duration,
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
                    chat.as_ref(),
                    voice.as_ref(),
                    bot_user_id,
                    &channel_state,
                    chat_min_reply_gap,
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
                                "Failed to add autorole {} to user {}: {} \
                                 (403 = the bot's highest role must sit above \
                                 the autorole and have Manage Roles)",
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
        // Reactions feed the training corpus: humans reacting to messages is
        // exactly the signal that teaches Sig when (and how) to react. The
        // bot's own reactions are skipped — same self-imitation rule as chat.
        Event::ReactionAdd(reaction) => {
            if reaction.user_id != bot_user_id {
                if let Err(e) = channel_log::append_reaction(
                    reaction.guild_id,
                    reaction.channel_id.get(),
                    reaction.message_id.get(),
                    reaction.user_id.get(),
                    &emoji_text(&reaction.emoji),
                    1,
                ) {
                    tracing::warn!("reaction log append failed: {}", e);
                }
            }
        }
        Event::ReactionRemove(reaction) => {
            if reaction.user_id != bot_user_id {
                if let Err(e) = channel_log::append_reaction(
                    reaction.guild_id,
                    reaction.channel_id.get(),
                    reaction.message_id.get(),
                    reaction.user_id.get(),
                    &emoji_text(&reaction.emoji),
                    -1,
                ) {
                    tracing::warn!("reaction log append failed: {}", e);
                }
            }
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
