//! `/help` — lists every registered slash command and who can use it. Kept in
//! sync by hand with `command_definitions()`; the test in `mod.rs` asserts the
//! registry size so a new command is a compile-visible reminder to update this.

use super::create_response;
use anyhow::Result;
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
        description: "List the bot's commands and who can use them".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "help".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: Vec::new(),
        version: Id::new(1),
    }
}

const HELP_TEXT: &str = "\
**SuperSighurt commands**

*Everyone*
• `/ping` — check the bot is responsive
• `/help` — this list
• `/userinfo [user]` — info about a user
• `/serverinfo` — info about this server
• `/say <message>` — post a message attributed to you
• `/ai status` — is AI chat on?
• `/moderation status` — is Sig's reply filter on?

*Moderators* (need the matching Discord permission)
• `/ban <user> [reason] [delete_days]` — Ban Members
• `/kick <user> [reason]` — Kick Members
• `/mute <user> <duration> [reason]` — Timeout Members
• `/purge <count>` — Manage Messages

*Administrators* (need the Administrator permission)
• `/ai on|off` — turn AI chat on/off
• `/moderation on|off` — turn Sig's self-filter on/off
• `/filterword add|remove|list` — manage the reply-filter deny-list
• `/automod ...` — configure user auto-moderation
• `/autorole ...` — configure the role given to new members";

pub async fn handle(
    _interaction: &Interaction,
    _data: &CommandData,
) -> Result<InteractionResponse> {
    Ok(create_response(HELP_TEXT))
}
