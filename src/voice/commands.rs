//! Text command handlers for voice mode. Parsed in `events/message.rs` before
//! the chat/automod pipeline so admins can summon Bertil from any channel
//! they have access to without @-mentioning the bot.

use std::sync::Arc;

use twilight_http::Client;
use twilight_model::channel::Message;

use super::VoiceBridge;

/// Recognised voice commands.
#[derive(Debug, PartialEq, Eq)]
pub enum VoiceCommand {
    Join,
    Leave,
    Status,
}

/// Parse a message body. Accepts `!voice join`, `!voice leave`, `!voice
/// status`, and bare `!voice` (= status). Word-boundary aware so `!voiceless`
/// won't trigger.
pub fn parse(content: &str) -> Option<VoiceCommand> {
    let trimmed = content.trim();
    let rest = trimmed.strip_prefix("!voice")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.trim().to_ascii_lowercase().as_str() {
        "join" => Some(VoiceCommand::Join),
        "leave" | "stop" | "disconnect" => Some(VoiceCommand::Leave),
        "status" | "" => Some(VoiceCommand::Status),
        _ => None,
    }
}

/// Dispatch a parsed command. The bridge knows how to start/stop a call; we
/// just need to figure out which voice channel the invoker is in and post a
/// confirmation message back.
pub async fn handle(
    cmd: VoiceCommand,
    message: &Message,
    http: &Client,
    bridge: &Arc<VoiceBridge>,
) {
    // Voice only makes sense in a guild context.
    let Some(guild_id) = message.guild_id else {
        reply(http, message, "Voice commands only work inside a server.").await;
        return;
    };

    match cmd {
        VoiceCommand::Join => {
            // Voice state for "which channel is the invoker in?" comes from
            // VOICE_STATE_UPDATE gateway events, cached in the bridge.
            match bridge
                .voice_states
                .channel_for(guild_id, message.author.id)
            {
                Some(channel_id) => match bridge.start_call(guild_id, channel_id).await {
                    Ok(_) => reply(http, message, "Joining voice — say hi to Bertil 🎙️").await,
                    Err(e) => {
                        tracing::warn!("voice start_call failed: {}", e);
                        reply(http, message, &format!("Voice join failed: {e}")).await;
                    }
                },
                None => reply(http, message, "You need to be in a voice channel first.").await,
            }
        }
        VoiceCommand::Leave => match bridge.stop_call(guild_id).await {
            Ok(_) => reply(http, message, "Left voice. 👋").await,
            Err(e) => {
                tracing::warn!("voice stop_call failed: {}", e);
                reply(http, message, &format!("Voice leave failed: {e}")).await;
            }
        },
        VoiceCommand::Status => {
            reply(
                http,
                message,
                "Voice mode is wired up. `!voice join` while in a voice channel to start.",
            )
            .await;
        }
    }
}

async fn reply(http: &Client, message: &Message, text: &str) {
    match http.create_message(message.channel_id).content(text) {
        Ok(builder) => {
            if let Err(e) = builder.await {
                tracing::warn!("voice command reply send failed: {}", e);
            }
        }
        Err(e) => tracing::warn!("voice command reply content invalid: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_matches() {
        assert_eq!(parse("!voice join"), Some(VoiceCommand::Join));
        assert_eq!(parse("  !voice   JOIN  "), Some(VoiceCommand::Join));
        assert_eq!(parse("!voice leave"), Some(VoiceCommand::Leave));
        assert_eq!(parse("!voice stop"), Some(VoiceCommand::Leave));
        assert_eq!(parse("!voice"), Some(VoiceCommand::Status));
        assert_eq!(parse("!voice status"), Some(VoiceCommand::Status));
    }

    #[test]
    fn parse_rejects_non_matches() {
        assert_eq!(parse("!voiceless"), None);
        assert_eq!(parse("voice join"), None);
        assert_eq!(parse("hello !voice join"), None);
        assert_eq!(parse("!voice bogus"), None);
    }
}
