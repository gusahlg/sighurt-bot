//! `/moderation on|off|status` — flips the reply-filter runtime toggle that
//! screens Sig's OWN outgoing replies (see `reply_filter.rs`).
//!
//! Gated two ways on the Discord **Administrator** permission: the command
//! carries `default_member_permissions = ADMINISTRATOR` (so Discord hides it
//! from non-admins and the server owner can re-grant it per-role), AND the
//! handler re-checks that the invoking member actually holds Administrator
//! (or is the guild owner) before mutating anything. `status` is read-only and
//! open to everyone.

use super::{admin_or_owner, create_response, on_off_status_command, toggle_choice, ToggleChoice};
use crate::chat::ChatRuntime;
use anyhow::Result;
use twilight_model::{
    application::command::Command,
    application::interaction::{application_command::CommandData, Interaction},
    http::interaction::InteractionResponse,
};

pub fn create_command() -> Command {
    on_off_status_command(
        "moderation",
        "Turn Sig's reply filter (self-moderation) on or off, or read its state",
    )
}

pub async fn handle(
    interaction: &Interaction,
    data: &CommandData,
    chat: &ChatRuntime,
) -> Result<InteractionResponse> {
    let filter = chat.filter();
    let choice = toggle_choice(data);

    // Status is read-only: anyone may check whether the filter is engaged.
    if !matches!(choice, ToggleChoice::Status) && !admin_or_owner(interaction) {
        return Ok(create_response(
            "You need the **Administrator** permission to change the reply filter.",
        ));
    }

    let reply = match choice {
        ToggleChoice::Status => format!(
            "Reply filter (self-moderation): **{}**. AI judge: {}.",
            on_off(filter.is_enabled()),
            filter.judge_description()
        ),
        ToggleChoice::On => {
            let was_on = filter.set_enabled(true);
            if was_on {
                "Reply filter is already **ON**.".to_string()
            } else {
                "Reply filter: **ON** — Sig's own replies are now screened again.".to_string()
            }
        }
        ToggleChoice::Off => {
            let was_on = filter.set_enabled(false);
            if was_on {
                "Reply filter: **OFF** — Sig now speaks unfiltered.".to_string()
            } else {
                "Reply filter is already **OFF**.".to_string()
            }
        }
    };
    Ok(create_response(&reply))
}

fn on_off(v: bool) -> &'static str {
    if v {
        "ON"
    } else {
        "OFF"
    }
}
