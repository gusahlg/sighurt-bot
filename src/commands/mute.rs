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
    util::Timestamp,
};

pub fn create_command() -> Command {
    Command {
        application_id: None,
        default_member_permissions: Some(Permissions::MODERATE_MEMBERS),
        dm_permission: Some(false),
        description: "Timeout a user (mute)".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "mute".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "The user to mute".to_string(),
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
                description: "Duration in minutes (max 40320 = 28 days)".to_string(),
                description_localizations: None,
                kind: CommandOptionType::Integer,
                max_length: None,
                max_value: Some(twilight_model::application::command::CommandOptionValue::Integer(40320)),
                min_length: None,
                min_value: Some(twilight_model::application::command::CommandOptionValue::Integer(1)),
                name: "duration".to_string(),
                name_localizations: None,
                options: None,
                required: Some(true),
            },
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Reason for the mute".to_string(),
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
    let mut duration_minutes: i64 = 10;
    let mut reason = None;

    for option in &data.options {
        match option.name.as_str() {
            "user" => {
                if let CommandOptionValue::User(id) = option.value {
                    target_user_id = Some(id);
                }
            }
            "duration" => {
                if let CommandOptionValue::Integer(mins) = option.value {
                    duration_minutes = mins.clamp(1, 40320);
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
        return Ok(create_response("Please specify a user to mute"));
    };

    // Safety checks
    if target_id == moderator_id {
        return Ok(create_response("You cannot mute yourself"));
    }

    // Calculate timeout timestamp
    let timeout_until = calculate_timeout_timestamp(duration_minutes as u64 * 60);
    let timestamp = match Timestamp::parse(&timeout_until) {
        Ok(ts) => ts,
        Err(e) => return Ok(create_response(&format!("Failed to calculate timeout: {:?}", e))),
    };

    // Perform the timeout
    let mute_reason = reason.as_deref().unwrap_or("No reason provided");

    let request = match http
        .update_guild_member(guild_id, target_id)
        .communication_disabled_until(Some(timestamp))
    {
        Ok(r) => r,
        Err(e) => return Ok(create_response(&format!("Failed to create mute request: {}", e))),
    };

    let request = match request.reason(mute_reason) {
        Ok(r) => r,
        Err(e) => return Ok(create_response(&format!("Invalid reason: {}", e))),
    };

    if let Err(e) = request.await {
        return Ok(create_response(&format!("Failed to mute user: {}", e)));
    }

    // Log the action
    if let Err(e) = log_mod_action(pool, guild_id, target_id, moderator_id, "mute", reason.as_deref()).await {
        tracing::error!("Failed to log mod action: {}", e);
    }

    let duration_str = format_duration(duration_minutes as u64);
    Ok(create_public_response(&format!(
        "**Muted** <@{}> for {}\n**Reason:** {}",
        target_id, duration_str, mute_reason
    )))
}

fn calculate_timeout_timestamp(seconds: u64) -> String {
    use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

    let future = OffsetDateTime::now_utc() + Duration::seconds(seconds as i64);
    future.format(&Rfc3339).unwrap_or_else(|_| {
        // Fallback format if Rfc3339 somehow fails
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            future.year(),
            future.month() as u8,
            future.day(),
            future.hour(),
            future.minute(),
            future.second()
        )
    })
}

fn format_duration(minutes: u64) -> String {
    if minutes >= 1440 {
        let days = minutes / 1440;
        let remaining = minutes % 1440;
        if remaining > 0 {
            let hours = remaining / 60;
            format!("{} day(s) {} hour(s)", days, hours)
        } else {
            format!("{} day(s)", days)
        }
    } else if minutes >= 60 {
        let hours = minutes / 60;
        let remaining = minutes % 60;
        if remaining > 0 {
            format!("{} hour(s) {} minute(s)", hours, remaining)
        } else {
            format!("{} hour(s)", hours)
        }
    } else {
        format!("{} minute(s)", minutes)
    }
}
