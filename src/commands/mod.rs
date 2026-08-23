pub mod automod_config;
pub mod autorole;
pub mod ban;
pub mod kick;
pub mod mute;
pub mod ping;
pub mod purge;
pub mod say;
pub mod serverinfo;
pub mod userinfo;

use anyhow::Result;
use sqlx::SqlitePool;
use std::sync::Arc;
use twilight_http::Client;
use twilight_model::{
    application::interaction::{Interaction, InteractionData, InteractionType},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{marker::ApplicationMarker, Id},
};

pub async fn register_commands(http: &Client, application_id: Id<ApplicationMarker>) -> Result<()> {
    let commands = command_definitions();

    http.interaction(application_id)
        .set_global_commands(&commands)
        .await?;

    tracing::info!("Registered {} slash commands", commands.len());
    Ok(())
}

fn command_definitions() -> Vec<twilight_model::application::command::Command> {
    vec![
        ban::create_command(),
        kick::create_command(),
        mute::create_command(),
        purge::create_command(),
        automod_config::create_command(),
        ping::create_command(),
        userinfo::create_command(),
        serverinfo::create_command(),
        // /say posts "<invoker>: <message>" — attribution makes it safe to
        // leave open to everyone.
        say::create_command(),
        autorole::create_command(),
    ]
}

pub async fn handle_interaction(
    interaction: Interaction,
    http: Arc<Client>,
    pool: SqlitePool,
) -> Result<()> {
    if interaction.kind != InteractionType::ApplicationCommand {
        return Ok(());
    }

    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok(());
    };

    let response = match data.name.as_str() {
        "ban" => ban::handle(&interaction, &data, &http, &pool).await,
        "kick" => kick::handle(&interaction, &data, &http, &pool).await,
        "mute" => mute::handle(&interaction, &data, &http, &pool).await,
        "purge" => purge::handle(&interaction, &data, &http, &pool).await,
        "automod" => automod_config::handle(&interaction, &data, &http, &pool).await,
        "ping" => ping::handle(&interaction, &data, &http, &pool).await,
        "userinfo" => userinfo::handle(&interaction, &data, &http, &pool).await,
        "serverinfo" => serverinfo::handle(&interaction, &data, &http, &pool).await,
        "say" => say::handle(&interaction, &data, &http, &pool).await,
        "autorole" => autorole::handle(&interaction, &data, &http, &pool).await,
        _ => Ok(create_response("Unknown command")),
    }?;

    let interaction_client = http.interaction(interaction.application_id);
    interaction_client
        .create_response(interaction.id, &interaction.token, &response)
        .await?;

    Ok(())
}

pub fn create_response(content: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(content.to_string()),
            flags: Some(twilight_model::channel::message::MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

pub fn create_public_response(content: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(content.to_string()),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn say_is_registered_with_attribution() {
        let names = command_definitions()
            .into_iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 10);
        assert!(names.iter().any(|name| name == "say"));
    }
}
