use super::create_response;
use anyhow::Result;
use sqlx::SqlitePool;
use twilight_http::Client;
use twilight_model::{
    application::command::{Command, CommandType},
    application::interaction::{application_command::CommandData, Interaction},
    http::interaction::InteractionResponse,
    id::Id,
};

pub fn create_command() -> Command {
    Command {
        application_id: None,
        default_member_permissions: None,
        dm_permission: Some(true),
        description: "Check if the bot is responsive".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "ping".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![],
        version: Id::new(1),
    }
}

pub async fn handle(
    _interaction: &Interaction,
    _data: &CommandData,
    _http: &Client,
    _pool: &SqlitePool,
) -> Result<InteractionResponse> {
    Ok(create_response("Pong! Bot is online and responsive."))
}
