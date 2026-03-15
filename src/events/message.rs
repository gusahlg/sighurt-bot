use crate::ai::AiProcessor;
use crate::automod::{AutoMod, AutoModAction};
use anyhow::Result;
use twilight_http::Client;
use twilight_model::channel::Message;

pub async fn handle_message(
    message: &Message,
    _http: &Client,
    automod: &AutoMod,
    ai: &AiProcessor,
) -> Result<()> {
    // Check automod first (fast path)
    let action = automod.check_message(message).await?;

    match action {
        AutoModAction::None => {
            // If automod passes and AI is enabled, check with AI
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
                            // For now, just log AI detections
                            // Future: integrate with automod actions
                        }
                    }
                }
            }
        }
        _ => {
            automod.execute_action(action, message).await?;
        }
    }

    Ok(())
}
