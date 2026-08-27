//! `/ai on|off|status` — flips the runtime chat toggle (whether Sig responds
//! to DMs / @-mentions / replies at all). Migrated from the legacy `!ai` text
//! command; same Administrator gating as `/moderation`.

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
        "ai",
        "Turn Sig's AI chat responses on or off, or read the current state",
    )
}

pub async fn handle(
    interaction: &Interaction,
    data: &CommandData,
    chat: &ChatRuntime,
) -> Result<InteractionResponse> {
    let choice = toggle_choice(data);

    if !matches!(choice, ToggleChoice::Status) && !admin_or_owner(interaction) {
        return Ok(create_response(
            "You need the **Administrator** permission to toggle AI chat.",
        ));
    }

    let reply = match choice {
        ToggleChoice::Status => {
            format!("AI chat: **{}**.", on_off(chat.is_enabled()))
        }
        ToggleChoice::On => {
            let was_on = chat.set_enabled(true);
            if was_on {
                "AI chat is already **ON**.".to_string()
            } else {
                "AI chat: **ON** — Sig will respond again.".to_string()
            }
        }
        ToggleChoice::Off => {
            let was_on = chat.set_enabled(false);
            if was_on {
                "AI chat: **OFF** — Sig will stay quiet.".to_string()
            } else {
                "AI chat is already **OFF**.".to_string()
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
