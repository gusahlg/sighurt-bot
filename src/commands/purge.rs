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
        description: "Delete multiple messages at once".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "purge".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Number of messages to delete (1-100)".to_string(),
                description_localizations: None,
                kind: CommandOptionType::Integer,
                max_length: None,
                max_value: Some(twilight_model::application::command::CommandOptionValue::Integer(100)),
                min_length: None,
                min_value: Some(twilight_model::application::command::CommandOptionValue::Integer(1)),
                name: "count".to_string(),
                name_localizations: None,
                options: None,
                required: Some(true),
            },
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Only delete messages from this user".to_string(),
                description_localizations: None,
                kind: CommandOptionType::User,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "user".to_string(),
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
    _pool: &SqlitePool,
) -> Result<InteractionResponse> {
    // Ensure this is used in a guild
    if interaction.guild_id.is_none() {
        return Ok(create_response("This command can only be used in a server"));
    }

    let Some(channel_id) = interaction.channel.as_ref().map(|c| c.id) else {
        return Ok(create_response("Could not determine channel"));
    };

    // Get command options
    let mut count: u64 = 10;
    let mut filter_user = None;

    for option in &data.options {
        match option.name.as_str() {
            "count" => {
                if let CommandOptionValue::Integer(n) = option.value {
                    count = n.clamp(1, 100) as u64;
                }
            }
            "user" => {
                if let CommandOptionValue::User(id) = option.value {
                    filter_user = Some(id);
                }
            }
            _ => {}
        }
    }

    // Fetch messages
    let messages = match http.channel_messages(channel_id).limit(count as u16) {
        Ok(request) => match request.await {
            Ok(response) => response.models().await?,
            Err(e) => return Ok(create_response(&format!("Failed to fetch messages: {}", e))),
        },
        Err(e) => return Ok(create_response(&format!("Invalid request: {}", e))),
    };

    // Filter messages if user specified
    let message_ids: Vec<_> = messages
        .iter()
        .filter(|m| filter_user.map_or(true, |u| m.author.id == u))
        .map(|m| m.id)
        .collect();

    if message_ids.is_empty() {
        return Ok(create_response("No messages found to delete"));
    }

    // Delete messages
    let mut deleted_count = 0;

    if message_ids.len() == 1 {
        // Single message delete
        if let Err(e) = http.delete_message(channel_id, message_ids[0]).await {
            return Ok(create_response(&format!("Failed to delete message: {}", e)));
        }
        deleted_count = 1;
    } else {
        // Bulk delete (only works for messages < 14 days old)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("System clock before Unix epoch")
            .as_millis() as u64;

        let fourteen_days_ms = 14 * 24 * 60 * 60 * 1000;
        let discord_epoch = 1420070400000u64;

        let recent_ids: Vec<_> = message_ids
            .iter()
            .filter(|id| {
                let timestamp = (id.get() >> 22) + discord_epoch;
                now.saturating_sub(timestamp) < fourteen_days_ms
            })
            .copied()
            .collect();

        let old_ids: Vec<_> = message_ids
            .iter()
            .filter(|id| {
                let timestamp = (id.get() >> 22) + discord_epoch;
                now.saturating_sub(timestamp) >= fourteen_days_ms
            })
            .copied()
            .collect();

        // Bulk delete recent messages
        if recent_ids.len() >= 2 {
            match http.delete_messages(channel_id, &recent_ids) {
                Ok(request) => {
                    if let Err(e) = request.await {
                        return Ok(create_response(&format!("Failed to delete messages: {}", e)));
                    }
                    deleted_count += recent_ids.len();
                }
                Err(e) => {
                    return Ok(create_response(&format!("Failed to create delete request: {}", e)));
                }
            }
        } else if recent_ids.len() == 1 {
            if http.delete_message(channel_id, recent_ids[0]).await.is_ok() {
                deleted_count += 1;
            }
        }

        // Delete old messages one by one
        for id in &old_ids {
            if http.delete_message(channel_id, *id).await.is_ok() {
                deleted_count += 1;
            }
        }
    }

    let response_text = if let Some(user_id) = filter_user {
        format!("Deleted {} message(s) from <@{}>", deleted_count, user_id)
    } else {
        format!("Deleted {} message(s)", deleted_count)
    };

    Ok(create_response(&response_text))
}
