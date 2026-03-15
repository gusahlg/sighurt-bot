use super::{create_response, create_public_response};
use crate::database::models::log_mod_action;
use anyhow::Result;
use sqlx::SqlitePool;
use twilight_http::{request::AuditLogReason, Client};
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
        default_member_permissions: Some(Permissions::BAN_MEMBERS),
        dm_permission: Some(false),
        description: "Ban a user from the server".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "ban".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "The user to ban".to_string(),
                description_localizations: None,
                kind: CommandOptionType::User,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "user".to_string(),
                name_localizations: None,
                options: None,
                required: Some(true),
            },
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Reason for the ban".to_string(),
                description_localizations: None,
                kind: CommandOptionType::String,
                max_length: Some(512),
                max_value: None,
                min_length: None,
                min_value: None,
                name: "reason".to_string(),
                name_localizations: None,
                options: None,
                required: Some(false),
            },
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Days of messages to delete (0-7)".to_string(),
                description_localizations: None,
                kind: CommandOptionType::Integer,
                max_length: None,
                max_value: Some(twilight_model::application::command::CommandOptionValue::Integer(7)),
                min_length: None,
                min_value: Some(twilight_model::application::command::CommandOptionValue::Integer(0)),
                name: "delete_days".to_string(),
                name_localizations: None,
                options: None,
                required: Some(false),
            },
        ],
        version: Id::new(1),
    }
}

pub async fn handle(
    interaction: &Interaction,
    data: &CommandData,
    http: &Client,
    pool: &SqlitePool,
) -> Result<InteractionResponse> {
    let Some(guild_id) = interaction.guild_id else {
        return Ok(create_response("This command can only be used in a server"));
    };

    let Some(member) = &interaction.member else {
        return Ok(create_response("Could not get member information"));
    };

    // Get moderator ID - return error instead of using fake ID
    let Some(moderator_id) = member.user.as_ref().map(|u| u.id) else {
        return Ok(create_response("Could not determine your user ID"));
    };

    // Get command options
    let mut target_user_id = None;
    let mut reason = None;
    let mut delete_days: u32 = 1;

    for option in &data.options {
        match option.name.as_str() {
            "user" => {
                if let CommandOptionValue::User(id) = option.value {
                    target_user_id = Some(id);
                }
            }
            "reason" => {
                if let CommandOptionValue::String(ref r) = option.value {
                    reason = Some(r.clone());
                }
            }
            "delete_days" => {
                if let CommandOptionValue::Integer(days) = option.value {
                    delete_days = days.clamp(0, 7) as u32;
                }
            }
            _ => {}
        }
    }

    let Some(target_id) = target_user_id else {
        return Ok(create_response("Please specify a user to ban"));
    };

    // Safety checks
    if target_id == moderator_id {
        return Ok(create_response("You cannot ban yourself"));
    }

    // Perform the ban
    let ban_reason = reason.as_deref().unwrap_or("No reason provided");

    let request = match http
        .create_ban(guild_id, target_id)
        .delete_message_seconds(delete_days * 86400)
    {
        Ok(r) => r,
        Err(e) => return Ok(create_response(&format!("Invalid delete days: {}", e))),
    };

    let request = match request.reason(ban_reason) {
        Ok(r) => r,
        Err(e) => return Ok(create_response(&format!("Invalid reason: {}", e))),
    };

    if let Err(e) = request.await {
        return Ok(create_response(&format!("Failed to ban user: {}", e)));
    }

    // Log the action
    if let Err(e) = log_mod_action(pool, guild_id, target_id, moderator_id, "ban", reason.as_deref()).await {
        tracing::error!("Failed to log mod action: {}", e);
    }

    Ok(create_public_response(&format!(
        "**Banned** <@{}>\n**Reason:** {}",
        target_id, ban_reason
    )))
}
