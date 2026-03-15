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
        default_member_permissions: None,
        dm_permission: Some(false),
        description: "Get information about a user".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "userinfo".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![CommandOption {
            autocomplete: None,
            channel_types: None,
            choices: None,
            description: "The user to get info about (defaults to yourself)".to_string(),
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
    let Some(guild_id) = interaction.guild_id else {
        return Ok(create_response("This command can only be used in a server"));
    };

    // Get target user ID from options or default to command user
    let target_user_id = data
        .options
        .iter()
        .find_map(|o| {
            if o.name == "user" {
                if let CommandOptionValue::User(id) = o.value {
                    return Some(id);
                }
            }
            None
        })
        .or_else(|| interaction.member.as_ref()?.user.as_ref().map(|u| u.id));

    let Some(user_id) = target_user_id else {
        return Ok(create_response("Could not determine user"));
    };

    // Get user info
    let user = match http.user(user_id).await {
        Ok(response) => match response.model().await {
            Ok(user) => user,
            Err(e) => return Ok(create_response(&format!("Failed to get user: {}", e))),
        },
        Err(e) => return Ok(create_response(&format!("Failed to fetch user: {}", e))),
    };

    // Get member info
    let member = http.guild_member(guild_id, user_id).await.ok();
    let member_model = if let Some(resp) = member {
        resp.model().await.ok()
    } else {
        None
    };

    // Build response
    let mut info = format!(
        "**User Info: {}**\n\
        **ID:** {}\n\
        **Username:** {}\n\
        **Bot:** {}\n",
        user.name,
        user.id,
        user.name,
        if user.bot { "Yes" } else { "No" }
    );

    if let Some(member) = member_model {
        if let Some(nick) = member.nick {
            info.push_str(&format!("**Nickname:** {}\n", nick));
        }
        info.push_str(&format!("**Joined Server:** <t:{}:R>\n", member.joined_at.as_secs()));
        if !member.roles.is_empty() {
            let roles: Vec<String> = member.roles.iter().map(|r| format!("<@&{}>", r)).collect();
            info.push_str(&format!("**Roles:** {}\n", roles.join(", ")));
        }
    }

    // Discord epoch for calculating account creation
    let discord_epoch = 1420070400000u64;
    let created_timestamp = ((user.id.get() >> 22) + discord_epoch) / 1000;
    info.push_str(&format!("**Account Created:** <t:{}:R>", created_timestamp));

    Ok(create_response(&info))
}
