pub mod ai;
pub mod automod_config;
pub mod autorole;
pub mod ban;
pub mod filterword;
pub mod help;
pub mod kick;
pub mod moderation;
pub mod mute;
pub mod ping;
pub mod purge;
pub mod say;
pub mod set_config;
pub mod serverinfo;
pub mod userinfo;

use crate::chat::ChatRuntime;
use anyhow::Result;
use sqlx::SqlitePool;
use std::sync::Arc;
use twilight_http::Client;
use twilight_model::{
    application::command::{
        Command, CommandOption, CommandOptionChoice, CommandOptionChoiceValue, CommandOptionType,
        CommandType,
    },
    application::interaction::{
        application_command::{CommandData, CommandOptionValue},
        Interaction, InteractionData, InteractionType,
    },
    guild::Permissions,
    http::interaction::{
        InteractionResponse, InteractionResponseData, InteractionResponseType,
    },
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

fn command_definitions() -> Vec<Command> {
    vec![
        // Moderation (user-facing) — gated on the matching Discord permission.
        ban::create_command(),
        kick::create_command(),
        mute::create_command(),
        purge::create_command(),
        automod_config::create_command(),
        autorole::create_command(),
        // Sig runtime controls — Administrator-gated (perm + code check).
        ai::create_command(),
        moderation::create_command(),
        filterword::create_command(),
        // Open utilities.
        ping::create_command(),
        userinfo::create_command(),
        serverinfo::create_command(),
        // /say posts "<invoker>: <message>" — attribution makes it safe to
        // leave open to everyone.
        say::create_command(),
        help::create_command(),
        // Owner-only runtime control (hard user-id gate in the handler).
        set_config::create_command(),
    ]
}

pub async fn handle_interaction(
    interaction: Interaction,
    http: Arc<Client>,
    pool: SqlitePool,
    chat: Option<Arc<ChatRuntime>>,
) -> Result<()> {
    if interaction.kind != InteractionType::ApplicationCommand {
        return Ok(());
    }

    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok(());
    };

    // The three Sig-runtime controls need the ChatRuntime. If the bot booted
    // without an LLM_API_KEY there is no runtime to toggle — say so instead of
    // silently no-op'ing.
    let response = match data.name.as_str() {
        "ban" => ban::handle(&interaction, data, &http, &pool).await,
        "kick" => kick::handle(&interaction, data, &http, &pool).await,
        "mute" => mute::handle(&interaction, data, &http, &pool).await,
        "purge" => purge::handle(&interaction, data, &http, &pool).await,
        "automod" => automod_config::handle(&interaction, data, &http, &pool).await,
        "autorole" => autorole::handle(&interaction, data, &http, &pool).await,
        "ping" => ping::handle(&interaction, data, &http, &pool).await,
        "userinfo" => userinfo::handle(&interaction, data, &http, &pool).await,
        "serverinfo" => serverinfo::handle(&interaction, data, &http, &pool).await,
        "say" => say::handle(&interaction, data, &http, &pool).await,
        "set" => set_config::handle(&interaction, data).await,
        "help" => help::handle(&interaction, data).await,
        "ai" | "moderation" | "filterword" => match chat.as_deref() {
            Some(chat) => match data.name.as_str() {
                "ai" => ai::handle(&interaction, data, chat).await,
                "moderation" => moderation::handle(&interaction, data, chat).await,
                "filterword" => filterword::handle(&interaction, data, chat).await,
                _ => unreachable!(),
            },
            None => Ok(create_response(
                "Chat isn't configured on this instance (no LLM_API_KEY), so there's nothing to toggle.",
            )),
        },
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

/// The on/off/status choice supplied to a runtime-toggle command. Absent =
/// `Status` (bare `/moderation` reads state without changing it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleChoice {
    On,
    Off,
    Status,
}

/// Build the shared `<name> [state: on|off|status]` command shape used by
/// `/ai` and `/moderation`. Administrator-gated at the Discord level; the
/// handlers additionally re-check the invoker before mutating.
pub fn on_off_status_command(name: &str, description: &str) -> Command {
    let state_option = CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: Some(vec![
            CommandOptionChoice {
                name: "on".to_string(),
                name_localizations: None,
                value: CommandOptionChoiceValue::String("on".to_string()),
            },
            CommandOptionChoice {
                name: "off".to_string(),
                name_localizations: None,
                value: CommandOptionChoiceValue::String("off".to_string()),
            },
            CommandOptionChoice {
                name: "status".to_string(),
                name_localizations: None,
                value: CommandOptionChoiceValue::String("status".to_string()),
            },
        ]),
        description: "on, off, or status (default: status)".to_string(),
        description_localizations: None,
        kind: CommandOptionType::String,
        max_length: None,
        max_value: None,
        min_length: None,
        min_value: None,
        name: "state".to_string(),
        name_localizations: None,
        options: None,
        required: Some(false),
    };
    Command {
        application_id: None,
        default_member_permissions: Some(Permissions::ADMINISTRATOR),
        dm_permission: Some(false),
        description: description.to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: name.to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![state_option],
        version: Id::new(1),
    }
}

/// Read the `state` option of an on/off/status command; a missing/unknown value
/// reads as `Status` (read-only, the safe default).
pub fn toggle_choice(data: &CommandData) -> ToggleChoice {
    for option in &data.options {
        if option.name == "state" {
            if let CommandOptionValue::String(ref v) = option.value {
                return match v.as_str() {
                    "on" => ToggleChoice::On,
                    "off" => ToggleChoice::Off,
                    _ => ToggleChoice::Status,
                };
            }
        }
    }
    ToggleChoice::Status
}

/// True when the interaction's invoking member holds the Administrator
/// permission (which, for the guild owner, Discord always grants implicitly).
///
/// This is the "anyone with Discord Administrator" gate — NOT a hardcoded user
/// id and NOT a role literally named "admin". For a guild interaction Discord
/// resolves the member's *effective* permissions and puts them on
/// `interaction.member.permissions`; the guild owner (and anyone in a role with
/// the Administrator bit) has ADMINISTRATOR set there, so a single
/// `contains(ADMINISTRATOR)` check covers both. No permissions field (DM
/// interaction, or a non-guild context) fails closed.
pub fn admin_or_owner(interaction: &Interaction) -> bool {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.permissions)
        .is_some_and(|perms| perms.contains(Permissions::ADMINISTRATOR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_complete_and_consistent() {
        let names = command_definitions()
            .into_iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        // Bump this when adding/removing a command (and update /help + the
        // dispatch match in handle_interaction).
        assert_eq!(names.len(), 15, "registry size changed — update /help too");
        for expected in [
            "ban",
            "kick",
            "mute",
            "purge",
            "automod",
            "autorole",
            "ai",
            "moderation",
            "filterword",
            "ping",
            "userinfo",
            "serverinfo",
            "say",
            "help",
            "set",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing command {expected}"
            );
        }
    }

    #[test]
    fn runtime_toggles_require_administrator() {
        // The Sig runtime controls must be Administrator-gated at the Discord
        // level (the code-level re-check lives in each handler).
        for command in command_definitions() {
            if matches!(command.name.as_str(), "ai" | "moderation" | "filterword") {
                assert_eq!(
                    command.default_member_permissions,
                    Some(Permissions::ADMINISTRATOR),
                    "{} must be Administrator-gated",
                    command.name
                );
            }
        }
    }
}
