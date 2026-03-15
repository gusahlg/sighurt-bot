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
    guild::Permissions,
    http::interaction::InteractionResponse,
    id::Id,
};

pub fn create_command() -> Command {
    Command {
        application_id: None,
        default_member_permissions: Some(Permissions::MANAGE_MESSAGES),
        dm_permission: Some(false),
        description: "Make the bot say something".to_string(),
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

    // Send the message (single call - no double-send)
    match http.create_message(channel.id).content(&message) {
        Ok(request) => {
            if let Err(e) = request.await {
                return Ok(create_response(&format!("Failed to send message: {}", e)));
            }
        }
        Err(e) => return Ok(create_response(&format!("Invalid message: {}", e))),
    }

    Ok(create_response("Message sent!"))
}
