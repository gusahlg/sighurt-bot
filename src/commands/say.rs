use super::create_response;
use anyhow::Result;
use sqlx::SqlitePool;
use twilight_http::Client;
use twilight_model::{
    application::command::{Command, CommandOption, CommandOptionType, CommandType},
    application::interaction::{
        application_command::{CommandData, CommandOptionValue},
        Interaction,
    },
    http::interaction::InteractionResponse,
    id::Id,
};

pub fn create_command() -> Command {
    Command {
        application_id: None,
        // Open to everyone: the posted message always carries the invoker's
        // name, so there is no impersonation to guard against.
        default_member_permissions: None,
        dm_permission: Some(false),
        description: "Make the bot repeat a message, attributed to you".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "say".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![CommandOption {
            autocomplete: None,
            channel_types: None,
            choices: None,
            description: "The message to send".to_string(),
            description_localizations: None,
            kind: CommandOptionType::String,
            max_length: Some(2000),
            max_value: None,
            min_length: Some(1),
            min_value: None,
            name: "message".to_string(),
            name_localizations: None,
            options: None,
            required: Some(true),
        }],
        version: Id::new(1),
    }
}

pub async fn handle(
    interaction: &Interaction,
    data: &CommandData,
    http: &Client,
    _pool: &SqlitePool,
) -> Result<InteractionResponse> {
    let Some(channel) = interaction.channel.as_ref() else {
        return Ok(create_response("Could not determine channel"));
    };

    let message = data
        .options
        .iter()
        .find_map(|o| {
            if o.name == "message" {
                if let CommandOptionValue::String(ref s) = o.value {
                    return Some(s.clone());
                }
            }
            None
        });

    let Some(message) = message else {
        return Ok(create_response("Please provide a message"));
    };

    // Always attribute the invoker: "<who used the command>: <the message>".
    // Guild nickname wins, then global display name, then the username — the
    // point is that nobody can put words in the bot's own mouth anonymously.
    let invoker = interaction
        .member
        .as_ref()
        .and_then(|member| member.nick.clone())
        .or_else(|| {
            interaction
                .author()
                .and_then(|user| user.global_name.clone())
        })
        .or_else(|| interaction.author().map(|user| user.name.clone()))
        .unwrap_or_else(|| "someone".to_string());
    let attributed = format!("{}: {}", invoker, message);

    // Send the message (single call - no double-send)
    match http.create_message(channel.id).content(&attributed) {
        Ok(request) => {
            if let Err(e) = request.await {
                return Ok(create_response(&format!("Failed to send message: {}", e)));
            }
        }
        Err(e) => return Ok(create_response(&format!("Invalid message: {}", e))),
    }

    Ok(create_response("Message sent!"))
}
