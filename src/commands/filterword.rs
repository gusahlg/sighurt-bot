//! `/filterword add|remove|list` — manages the admin-editable (JUDGED-tier)
//! deny-list of the reply filter (`reply_filter.rs`). Migrated from the legacy
//! `!filter add|remove|words` text commands. Administrator-gated (managing the
//! deny-list mutates moderation; listing is admin-only too).

use super::{admin_or_owner, create_response};
use crate::chat::ChatRuntime;
use crate::reply_filter::TermEdit;
use anyhow::Result;
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

fn word_option(description: &str) -> CommandOption {
    CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: None,
        description: description.to_string(),
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
    }
}

fn subcommand(name: &str, description: &str, options: Option<Vec<CommandOption>>) -> CommandOption {
    CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: None,
        description: description.to_string(),
        description_localizations: None,
        kind: CommandOptionType::SubCommand,
        max_length: None,
        max_value: None,
        min_length: None,
        min_value: None,
        name: name.to_string(),
        name_localizations: None,
        options,
        required: None,
    }
}

pub fn create_command() -> Command {
    Command {
        application_id: None,
        default_member_permissions: Some(Permissions::ADMINISTRATOR),
        dm_permission: Some(false),
        description: "Manage Sig's reply-filter deny-list (add/remove/list terms)".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "filterword".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![
            subcommand(
                "add",
                "Add a term to the reply-filter deny-list",
                Some(vec![word_option("The term (or phrase) to filter")]),
            ),
            subcommand(
                "remove",
                "Remove an admin-added term from the deny-list",
                Some(vec![word_option("The term (or phrase) to remove")]),
            ),
            subcommand("list", "List the admin-added deny-list terms", None),
        ],
        version: Id::new(1),
    }
}

pub async fn handle(
    interaction: &Interaction,
    data: &CommandData,
    chat: &ChatRuntime,
) -> Result<InteractionResponse> {
    if interaction.guild_id.is_none() {
        return Ok(create_response("This command can only be used in a server."));
    }
    if !admin_or_owner(interaction) {
        return Ok(create_response(
            "You need the **Administrator** permission to manage the deny-list.",
        ));
    }

    let Some(subcommand) = data.options.first() else {
        return Ok(create_response("Invalid command."));
    };
    let filter = chat.filter();

    let reply = match subcommand.name.as_str() {
        "list" => {
            let terms = filter.extra_terms();
            if terms.is_empty() {
                "No admin-added filter terms yet (built-in terms aren't listed).".to_string()
            } else {
                format!(
                    "Admin-added filter terms ({}): {}",
                    terms.len(),
                    terms.join(", ")
                )
            }
        }
        verb @ ("add" | "remove") => {
            let CommandOptionValue::SubCommand(options) = &subcommand.value else {
                return Ok(create_response("Invalid command."));
            };
            let word = options.iter().find_map(|o| {
                if o.name == "word" {
                    if let CommandOptionValue::String(ref v) = o.value {
                        return Some(v.clone());
                    }
                }
                None
            });
            let Some(term) = word else {
                return Ok(create_response("Please specify a term."));
            };
            if verb == "add" {
                match filter.add_term(&term) {
                    TermEdit::Added => format!("Added {term:?} to the deny-list."),
                    TermEdit::AlreadyPresent => format!("{term:?} is already filtered."),
                    TermEdit::NoWordsFile => {
                        "No filter.words_file is configured, so there's nowhere to save terms."
                            .to_string()
                    }
                    _ => format!("Couldn't add {term:?}."),
                }
            } else {
                match filter.remove_term(&term) {
                    TermEdit::Removed => format!("Removed {term:?} from the deny-list."),
                    TermEdit::NotFound => format!("{term:?} isn't in the admin deny-list."),
                    TermEdit::BuiltIn => {
                        format!("{term:?} is a built-in term and can't be removed via command.")
                    }
                    TermEdit::NoWordsFile => "No filter.words_file is configured.".to_string(),
                    _ => format!("Couldn't remove {term:?}."),
                }
            }
        }
        _ => "Unknown subcommand.".to_string(),
    };
    Ok(create_response(&reply))
}
