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
        dm_permission: Some(false),
        description: "Get information about this server".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "serverinfo".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![],
        version: Id::new(1),
    }
}

pub async fn handle(
    interaction: &Interaction,
    _data: &CommandData,
    http: &Client,
    _pool: &SqlitePool,
) -> Result<InteractionResponse> {
    let Some(guild_id) = interaction.guild_id else {
        return Ok(create_response("This command can only be used in a server"));
    };

    // Get guild info
    let guild = match http.guild(guild_id).await {
        Ok(response) => match response.model().await {
            Ok(guild) => guild,
            Err(e) => return Ok(create_response(&format!("Failed to get server: {}", e))),
        },
        Err(e) => return Ok(create_response(&format!("Failed to fetch server: {}", e))),
    };

    // Discord epoch for calculating creation
    let discord_epoch = 1420070400000u64;
    let created_timestamp = ((guild_id.get() >> 22) + discord_epoch) / 1000;

    let boost_level = match guild.premium_tier {
        twilight_model::guild::PremiumTier::None => 0,
        twilight_model::guild::PremiumTier::Tier1 => 1,
        twilight_model::guild::PremiumTier::Tier2 => 2,
        twilight_model::guild::PremiumTier::Tier3 => 3,
        _ => 0,
    };

    let info = format!(
        "**Server Info: {}**\n\
        **ID:** {}\n\
        **Owner:** <@{}>\n\
        **Members:** {}\n\
        **Roles:** {}\n\
        **Channels:** {}\n\
        **Boost Level:** {}\n\
        **Created:** <t:{}:R>",
        guild.name,
        guild.id,
        guild.owner_id,
        guild.approximate_member_count.unwrap_or(0),
        guild.roles.len(),
        guild.channels.len(),
        boost_level,
        created_timestamp
    );

    Ok(create_response(&info))
}
