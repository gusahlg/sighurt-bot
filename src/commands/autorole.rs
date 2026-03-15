use super::create_response;
use crate::database::models::{get_autorole, set_autorole};
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
        default_member_permissions: Some(Permissions::ADMINISTRATOR),
        dm_permission: Some(false),
        description: "Configure auto-role for new members".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "autorole".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Set the role to give new members".to_string(),
                description_localizations: None,
                kind: CommandOptionType::SubCommand,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "set".to_string(),
                name_localizations: None,
                options: Some(vec![CommandOption {
                    autocomplete: None,
                    channel_types: None,
                    choices: None,
                    description: "The role to assign to new members".to_string(),
                    description_localizations: None,
                    kind: CommandOptionType::Role,
                    max_length: None,
                    max_value: None,
                    min_length: None,
                    min_value: None,
                    name: "role".to_string(),
                    name_localizations: None,
                    options: None,
                    required: Some(true),
                }]),
                required: None,
            },
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Disable auto-role".to_string(),
                description_localizations: None,
                kind: CommandOptionType::SubCommand,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "off".to_string(),
                name_localizations: None,
                options: None,
                required: None,
            },
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Show current auto-role setting".to_string(),
                description_localizations: None,
                kind: CommandOptionType::SubCommand,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "status".to_string(),
                name_localizations: None,
                options: None,
                required: None,
            },
        ],
        version: Id::new(1),
    }
}

pub async fn handle(
    interaction: &Interaction,
    data: &CommandData,
    _http: &Client,
    pool: &SqlitePool,
) -> Result<InteractionResponse> {
    let Some(guild_id) = interaction.guild_id else {
        return Ok(create_response("This command can only be used in a server"));
    };

    let Some(subcommand) = data.options.first() else {
        return Ok(create_response("Invalid command"));
    };

    match subcommand.name.as_str() {
        "set" => {
            let CommandOptionValue::SubCommand(options) = &subcommand.value else {
                return Ok(create_response("Invalid command"));
            };

            let role_id = options.iter().find_map(|o| {
                if o.name == "role" {
                    if let CommandOptionValue::Role(id) = o.value {
                        return Some(id);
                    }
                }
                None
            });

            let Some(role_id) = role_id else {
                return Ok(create_response("Please specify a role"));
            };

            set_autorole(pool, guild_id, Some(role_id)).await?;
            Ok(create_response(&format!(
                "Auto-role set! New members will receive <@&{}>",
                role_id
            )))
        }
        "off" => {
            set_autorole(pool, guild_id, None).await?;
            Ok(create_response("Auto-role disabled"))
        }
        "status" => {
            let autorole = get_autorole(pool, guild_id).await?;
            match autorole {
                Some(role_id) => Ok(create_response(&format!(
                    "Auto-role is set to <@&{}>",
                    role_id
                ))),
                None => Ok(create_response("Auto-role is not configured")),
            }
        }
        _ => Ok(create_response("Unknown subcommand")),
    }
}
