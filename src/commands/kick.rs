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
        default_member_permissions: Some(Permissions::KICK_MEMBERS),
        dm_permission: Some(false),
        description: "Kick a user from the server".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "kick".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "The user to kick".to_string(),
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
                description: "Reason for the kick".to_string(),
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
            _ => {}
        }
    }

    let Some(target_id) = target_user_id else {
        return Ok(create_response("Please specify a user to kick"));
    };

    // Safety checks
    if target_id == moderator_id {
        return Ok(create_response("You cannot kick yourself"));
    }

    // Perform the kick
    let kick_reason = reason.as_deref().unwrap_or("No reason provided");

    let request = http.remove_guild_member(guild_id, target_id);
    let request = match request.reason(kick_reason) {
        Ok(r) => r,
        Err(e) => return Ok(create_response(&format!("Invalid reason: {}", e))),
    };

    if let Err(e) = request.await {
        return Ok(create_response(&format!("Failed to kick user: {}", e)));
    }

    // Log the action
    if let Err(e) = log_mod_action(pool, guild_id, target_id, moderator_id, "kick", reason.as_deref()).await {
        tracing::error!("Failed to log mod action: {}", e);
    }

    Ok(create_public_response(&format!(
        "**Kicked** <@{}>\n**Reason:** {}",
        target_id, kick_reason
    )))
}
