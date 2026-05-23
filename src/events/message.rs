use std::sync::Arc;

use crate::ai::AiProcessor;
use crate::automod::{AutoMod, AutoModAction};
use crate::chat::ChatRuntime;
use crate::voice::{self, VoiceBridge};
use anyhow::Result;
use discord_bot::channel_log;
use twilight_http::Client;
use twilight_model::channel::Message;
use twilight_model::id::{Id, marker::UserMarker};

const DISCORD_MAX_MESSAGE_LEN: usize = 1900;

pub async fn handle_message(
    message: &Message,
    http: &Client,
    automod: &AutoMod,
    ai: &AiProcessor,
    chat: Option<&ChatRuntime>,
    voice_bridge: Option<&Arc<VoiceBridge>>,
    bot_user_id: Id<UserMarker>,
) -> Result<()> {
    // Log every message we see (including our own outgoing replies and
    // webhook messages) so the corpus captures the full picture. Failures
    // are non-fatal — we'd rather drop a log line than break message
    // handling. The scraper handles backfill; this is the forward edge.
    if let Err(e) = channel_log::append_live(message) {
        tracing::warn!("channel_log append failed: {}", e);
    }

    // Skip bot and webhook messages for the rest of the pipeline — they
    // shouldn't trigger automod, AI moderation, or chat replies.
    if message.author.bot || message.webhook_id.is_some() {
        return Ok(());
    }

    let action = automod.check_message(message).await?;
    match action {
        AutoModAction::None => {
            if ai.is_enabled() {
                if let Some(guild_id) = message.guild_id {
                    if let Some(response) = ai.should_moderate(&message.content, guild_id).await {
                        if response.should_moderate && response.is_high_confidence() {
                            tracing::info!(
                                "AI flagged message in guild {} (confidence: {:.2}): {:?}",
                                guild_id,
                                response.confidence,
                                response.reason
                            );
                        }
                    }
                }
            }
        }
        _ => {
            automod.execute_action(action, message).await?;
            return Ok(());
        }
    }

    // Admin command path: `!ai on|off|status`. Handled before the DM/mention
    // gate so admins can toggle from any channel without @-mentioning the bot.
    if let Some(chat) = chat {
        if let Some(cmd) = parse_ai_command(&message.content) {
            handle_ai_command(cmd, message, http, chat).await;
            return Ok(());
        }
    }

    // Voice command path: `!voice join|leave|status`. Same any-channel
    // ergonomics as `!ai`; voice runs entirely server-side so there's no
    // admin gate (server perms already control who can talk in voice).
    if let Some(bridge) = voice_bridge {
        if let Some(cmd) = voice::commands::parse(&message.content) {
            voice::commands::handle(cmd, message, http, bridge).await;
            return Ok(());
        }
    }

    // Chat path: only triggered in DMs or when @-mentioned.
    let is_dm = message.guild_id.is_none();
    let is_mention = message.mentions.iter().any(|m| m.id == bot_user_id);
    if !is_dm && !is_mention {
        return Ok(());
    }
    let Some(chat) = chat else {
        return Ok(());
    };
    if !chat.is_enabled() {
        return Ok(());
    }

    let prompt = strip_bot_mention(&message.content, bot_user_id);
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Ok(());
    }

    match chat
        .client()
        .reply(message.channel_id.get(), &message.author.name, prompt)
        .await
    {
        Ok(reply) => {
            let trimmed = reply.trim();
            if trimmed.is_empty() {
                tracing::debug!("LLM returned empty reply; skipping post");
                return Ok(());
            }
            let truncated: String = trimmed.chars().take(DISCORD_MAX_MESSAGE_LEN).collect();
            match http.create_message(message.channel_id).content(&truncated) {
                Ok(builder) => {
                    if let Err(e) = builder.await {
                        tracing::warn!("Failed to post chat reply: {}", e);
                    }
                }
                Err(e) => tracing::warn!("Invalid chat reply content: {}", e),
            }
        }
        Err(e) => tracing::warn!("LLM call failed: {}", e),
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum AiCommand {
    On,
    Off,
    Status,
}

fn parse_ai_command(content: &str) -> Option<AiCommand> {
    let trimmed = content.trim();
    let rest = trimmed.strip_prefix("!ai")?;
    // Require word boundary after `!ai` so `!aim` and similar don't match.
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.trim().to_ascii_lowercase().as_str() {
        "on" | "enable" => Some(AiCommand::On),
        "off" | "disable" => Some(AiCommand::Off),
        "status" | "" => Some(AiCommand::Status),
        _ => None,
    }
}

async fn handle_ai_command(
    cmd: AiCommand,
    message: &Message,
    http: &Client,
    chat: &ChatRuntime,
) {
    // Status is read-only and informational — anyone may run it. On/Off mutate
    // state and require an admin allowlist entry.
    let needs_admin = matches!(cmd, AiCommand::On | AiCommand::Off);
    if needs_admin {
        if !chat.has_admins() {
            post(http, message, "AI toggle is not configured (chat.admin_user_ids is empty in config.toml).").await;
            return;
        }
        if !chat.is_admin(message.author.id.get()) {
            post(http, message, "You're not authorized to toggle AI mode.").await;
            return;
        }
    }

    let reply = match cmd {
        AiCommand::On => {
            let was = chat.set_enabled(true);
            if was { "AI mode is already ON.".to_string() } else { "AI mode: ON.".to_string() }
        }
        AiCommand::Off => {
            let was = chat.set_enabled(false);
            if was { "AI mode: OFF.".to_string() } else { "AI mode is already OFF.".to_string() }
        }
        AiCommand::Status => {
            if chat.is_enabled() {
                "AI mode: ON.".to_string()
            } else {
                "AI mode: OFF.".to_string()
            }
        }
    };
    post(http, message, &reply).await;
}

async fn post(http: &Client, message: &Message, text: &str) {
    match http.create_message(message.channel_id).content(text) {
        Ok(builder) => {
            if let Err(e) = builder.await {
                tracing::warn!("Failed to post AI command reply: {}", e);
            }
        }
        Err(e) => tracing::warn!("Invalid AI command reply content: {}", e),
    }
}

fn strip_bot_mention(content: &str, bot_user_id: Id<UserMarker>) -> String {
    let id_str = bot_user_id.to_string();
    let m1 = format!("<@{}>", id_str);
    let m2 = format!("<@!{}>", id_str);
    content.replace(&m1, "").replace(&m2, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ai_command_matches() {
        assert_eq!(parse_ai_command("!ai on"), Some(AiCommand::On));
        assert_eq!(parse_ai_command("  !ai   ON  "), Some(AiCommand::On));
        assert_eq!(parse_ai_command("!ai off"), Some(AiCommand::Off));
        assert_eq!(parse_ai_command("!ai enable"), Some(AiCommand::On));
        assert_eq!(parse_ai_command("!ai disable"), Some(AiCommand::Off));
        assert_eq!(parse_ai_command("!ai status"), Some(AiCommand::Status));
        assert_eq!(parse_ai_command("!ai"), Some(AiCommand::Status));
    }

    #[test]
    fn parse_ai_command_rejects_non_matches() {
        assert_eq!(parse_ai_command("!aim for the moon"), None);
        assert_eq!(parse_ai_command("ai on"), None);
        assert_eq!(parse_ai_command("!ai bogus"), None);
        assert_eq!(parse_ai_command("hello !ai on"), None);
    }
}
