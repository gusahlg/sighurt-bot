use super::create_response;
use crate::database::models::{
    add_filtered_word, get_filtered_words, get_guild_settings, remove_filtered_word,
    update_guild_settings,
};
use anyhow::Result;
use sqlx::SqlitePool;
use twilight_http::Client;
use twilight_model::{
    application::command::{Command, CommandOption, CommandOptionChoice, CommandOptionChoiceValue, CommandOptionType, CommandType},
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
        description: "Configure auto-moderation settings".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "automod".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![
            // Spam subcommand
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Configure spam detection".to_string(),
                description_localizations: None,
                kind: CommandOptionType::SubCommand,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "spam".to_string(),
                name_localizations: None,
                options: Some(vec![CommandOption {
                    autocomplete: None,
                    channel_types: None,
                    choices: Some(vec![
                        CommandOptionChoice {
                            name: "Enable".to_string(),
                            name_localizations: None,
                            value: CommandOptionChoiceValue::String("on".to_string()),
                        },
                        CommandOptionChoice {
                            name: "Disable".to_string(),
                            name_localizations: None,
                            value: CommandOptionChoiceValue::String("off".to_string()),
                        },
                    ]),
                    description: "Enable or disable spam detection".to_string(),
                    description_localizations: None,
                    kind: CommandOptionType::String,
                    max_length: None,
                    max_value: None,
                    min_length: None,
                    min_value: None,
                    name: "enabled".to_string(),
                    name_localizations: None,
                    options: None,
                    required: Some(true),
                }]),
                required: None,
            },
            // Raid subcommand
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Configure raid protection".to_string(),
                description_localizations: None,
                kind: CommandOptionType::SubCommand,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "raid".to_string(),
                name_localizations: None,
                options: Some(vec![CommandOption {
                    autocomplete: None,
                    channel_types: None,
                    choices: Some(vec![
                        CommandOptionChoice {
                            name: "Enable".to_string(),
                            name_localizations: None,
                            value: CommandOptionChoiceValue::String("on".to_string()),
                        },
                        CommandOptionChoice {
                            name: "Disable".to_string(),
                            name_localizations: None,
                            value: CommandOptionChoiceValue::String("off".to_string()),
                        },
                    ]),
                    description: "Enable or disable raid protection".to_string(),
                    description_localizations: None,
                    kind: CommandOptionType::String,
                    max_length: None,
                    max_value: None,
                    min_length: None,
                    min_value: None,
                    name: "enabled".to_string(),
                    name_localizations: None,
                    options: None,
                    required: Some(true),
                }]),
                required: None,
            },
            // Words subcommand group
            CommandOption {
                autocomplete: None,
                channel_types: None,
                choices: None,
                description: "Manage filtered words".to_string(),
                description_localizations: None,
                kind: CommandOptionType::SubCommandGroup,
                max_length: None,
                max_value: None,
                min_length: None,
                min_value: None,
                name: "words".to_string(),
                name_localizations: None,
                options: Some(vec![
                    CommandOption {
                        autocomplete: None,
                        channel_types: None,
                        choices: None,
                        description: "Add a word to the filter".to_string(),
                        description_localizations: None,
                        kind: CommandOptionType::SubCommand,
                        max_length: None,
                        max_value: None,
                        min_length: None,
                        min_value: None,
                        name: "add".to_string(),
                        name_localizations: None,
                        options: Some(vec![CommandOption {
                            autocomplete: None,
                            channel_types: None,
                            choices: None,
                            description: "The word to filter".to_string(),
                            description_localizations: None,
                            kind: CommandOptionType::String,
                            max_length: Some(100),
                            max_value: None,
                            min_length: Some(1),
                            min_value: None,
                            name: "word".to_string(),
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
                        description: "Remove a word from the filter".to_string(),
                        description_localizations: None,
                        kind: CommandOptionType::SubCommand,
                        max_length: None,
                        max_value: None,
                        min_length: None,
                        min_value: None,
                        name: "remove".to_string(),
                        name_localizations: None,
                        options: Some(vec![CommandOption {
                            autocomplete: None,
                            channel_types: None,
                            choices: None,
                            description: "The word to remove".to_string(),
                            description_localizations: None,
                            kind: CommandOptionType::String,
                            max_length: Some(100),
                            max_value: None,
                            min_length: Some(1),
                            min_value: None,
                            name: "word".to_string(),
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
                        description: "List all filtered words".to_string(),
                        description_localizations: None,
                        kind: CommandOptionType::SubCommand,
                        max_length: None,
                        max_value: None,
                        min_length: None,
                        min_value: None,
                        name: "list".to_string(),
                        name_localizations: None,
                        options: None,
                        required: None,
                    },
                ]),
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

    // Parse subcommand
    let Some(subcommand) = data.options.first() else {
        return Ok(create_response("Invalid command"));
    };

    match subcommand.name.as_str() {
        "spam" => {
            let CommandOptionValue::SubCommand(options) = &subcommand.value else {
                return Ok(create_response("Invalid command"));
            };

            let enabled = options.iter().find_map(|o| {
                if o.name == "enabled" {
                    if let CommandOptionValue::String(ref v) = o.value {
                        return Some(v == "on");
                    }
                }
                None
            });

            if let Some(enabled) = enabled {
                let mut settings = get_guild_settings(pool, guild_id).await?;
                settings.spam_enabled = enabled;
                update_guild_settings(pool, &settings).await?;

                let status = if enabled { "enabled" } else { "disabled" };
                Ok(create_response(&format!("Spam detection has been {}", status)))
            } else {
                Ok(create_response("Please specify on or off"))
            }
        }
        "raid" => {
            let CommandOptionValue::SubCommand(options) = &subcommand.value else {
                return Ok(create_response("Invalid command"));
            };

            let enabled = options.iter().find_map(|o| {
                if o.name == "enabled" {
                    if let CommandOptionValue::String(ref v) = o.value {
                        return Some(v == "on");
                    }
                }
                None
            });

            if let Some(enabled) = enabled {
                let mut settings = get_guild_settings(pool, guild_id).await?;
                settings.raid_enabled = enabled;
                update_guild_settings(pool, &settings).await?;

                let status = if enabled { "enabled" } else { "disabled" };
                Ok(create_response(&format!("Raid protection has been {}", status)))
            } else {
                Ok(create_response("Please specify on or off"))
            }
        }
        "words" => {
            let CommandOptionValue::SubCommandGroup(subcommands) = &subcommand.value else {
                return Ok(create_response("Invalid command"));
            };

            let Some(action) = subcommands.first() else {
                return Ok(create_response("Invalid command"));
            };

            match action.name.as_str() {
                "add" => {
                    let CommandOptionValue::SubCommand(options) = &action.value else {
                        return Ok(create_response("Invalid command"));
                    };

                    let word = options.iter().find_map(|o| {
                        if o.name == "word" {
                            if let CommandOptionValue::String(ref v) = o.value {
                                return Some(v.clone());
                            }
                        }
                        None
                    });

                    if let Some(word) = word {
                        let added = add_filtered_word(pool, guild_id, &word).await?;
                        if added {
                            Ok(create_response(&format!("Added '{}' to the word filter", word)))
                        } else {
                            Ok(create_response(&format!("'{}' is already in the word filter", word)))
                        }
                    } else {
                        Ok(create_response("Please specify a word to add"))
                    }
                }
                "remove" => {
                    let CommandOptionValue::SubCommand(options) = &action.value else {
                        return Ok(create_response("Invalid command"));
                    };

                    let word = options.iter().find_map(|o| {
                        if o.name == "word" {
                            if let CommandOptionValue::String(ref v) = o.value {
                                return Some(v.clone());
                            }
                        }
                        None
                    });

                    if let Some(word) = word {
                        let removed = remove_filtered_word(pool, guild_id, &word).await?;
                        if removed {
                            Ok(create_response(&format!("Removed '{}' from the word filter", word)))
                        } else {
                            Ok(create_response(&format!("'{}' was not in the word filter", word)))
                        }
                    } else {
                        Ok(create_response("Please specify a word to remove"))
                    }
                }
                "list" => {
                    let words = get_filtered_words(pool, guild_id).await?;
                    if words.is_empty() {
                        Ok(create_response("No words in the filter"))
                    } else {
                        let word_list = words.join(", ");
                        Ok(create_response(&format!("**Filtered words:** {}", word_list)))
                    }
                }
                _ => Ok(create_response("Unknown subcommand")),
            }
        }
        _ => Ok(create_response("Unknown subcommand")),
    }
}
