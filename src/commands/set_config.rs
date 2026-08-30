//! `/set config model:<…> computers:<…>` — owner-only control over which
//! bespoke model Sig runs and on which machine(s).
//!
//! This is THE OWNER'S switch. It is hard-gated to a single Discord user id
//! (not "any administrator"), because it moves the live brain across machines
//! and Wake-on-LANs hardware.
//!
//! How the pieces fit (see scripts/serve_mode.sh, scripts/pi/*):
//!   • `… server`  → 1.1B tensor-ash entirely on nixos-server (the bot host).
//!     Handled locally by ~/.local/bin/sig-server-mode.sh; wakes nothing.
//!   • anything else → a llama.cpp topology whose coordinator is the desktop.
//!     Fired at the always-on Pi (`sig-apply-config`) as a DETACHED transient
//!     unit, so the bot restart at the end of the bring-up can't kill the
//!     orchestration that is driving it. The Pi selectively Wake-on-LANs only
//!     the powerful machines the config needs.
//!
//! Because the bring-up ends by restarting this very bot, the command does not
//! try to post a "done" follow-up — it acks what it kicked off, then the bot
//! blips back on the new brain.

use super::create_response;
use anyhow::Result;
use std::time::Duration;
use tokio::process::Command as TokioCommand;
use twilight_model::{
    application::command::{
        Command, CommandOption, CommandOptionChoice, CommandOptionChoiceValue, CommandOptionType,
        CommandType,
    },
    application::interaction::{
        application_command::{CommandData, CommandOptionValue},
        Interaction,
    },
    guild::Permissions,
    http::interaction::InteractionResponse,
    id::Id,
};

/// The one and only user allowed to run `/set config`.
const OWNER_ID: u64 = 367334632515043329;
/// Absolute ssh on NixOS (the bot's service PATH may not include it).
const SSH_BIN: &str = "/run/current-system/sw/bin/ssh";

fn string_choice(name: &str, value: &str) -> CommandOptionChoice {
    CommandOptionChoice {
        name: name.to_string(),
        name_localizations: None,
        value: CommandOptionChoiceValue::String(value.to_string()),
    }
}

fn string_option(name: &str, description: &str, choices: Vec<CommandOptionChoice>) -> CommandOption {
    CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: Some(choices),
        description: description.to_string(),
        description_localizations: None,
        kind: CommandOptionType::String,
        max_length: None,
        max_value: None,
        min_length: None,
        min_value: None,
        name: name.to_string(),
        name_localizations: None,
        options: None,
        required: Some(true),
    }
}

pub fn create_command() -> Command {
    // model: which bespoke brain.
    let model = string_option(
        "model",
        "Which bespoke Sig model to run",
        vec![
            string_choice("1.1B (small fallback brain)", "1.1b"),
            string_choice("3B (bespoke, tools)", "3b"),
            string_choice("16B (bespoke, tools — the big brain)", "16b"),
        ],
    );
    // computers: which machine(s); the value IS the machines CSV serve_mode wants.
    let computers = string_option(
        "computers",
        "Which machine(s) to run it on (multiple = RPC together)",
        vec![
            string_choice("server (nixos-server · 1.1B only)", "server"),
            string_choice("desktop", "desktop"),
            string_choice("desktop + louise (RPC)", "desktop,louise"),
            string_choice("desktop + louise + server (RPC)", "desktop,louise,server"),
        ],
    );

    let config = CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: None,
        description: "Set which model runs on which machine(s)".to_string(),
        description_localizations: None,
        kind: CommandOptionType::SubCommand,
        max_length: None,
        max_value: None,
        min_length: None,
        min_value: None,
        name: "config".to_string(),
        name_localizations: None,
        options: Some(vec![model, computers]),
        required: None,
    };

    Command {
        application_id: None,
        // Hidden from non-admins in the UI; the handler additionally hard-checks
        // the exact owner id, so even other admins can't run it.
        default_member_permissions: Some(Permissions::ADMINISTRATOR),
        dm_permission: Some(true),
        description: "Owner-only SuperSighurt runtime controls".to_string(),
        description_localizations: None,
        guild_id: None,
        id: None,
        kind: CommandType::ChatInput,
        name: "set".to_string(),
        name_localizations: None,
        nsfw: Some(false),
        options: vec![config],
        version: Id::new(1),
    }
}

/// Pull the `model` + `computers` strings out of the `config` subcommand.
fn parse_options(data: &CommandData) -> Option<(String, String)> {
    let sub = data.options.iter().find(|o| o.name == "config")?;
    let CommandOptionValue::SubCommand(inner) = &sub.value else {
        return None;
    };
    let mut model = None;
    let mut computers = None;
    for opt in inner {
        if let CommandOptionValue::String(v) = &opt.value {
            match opt.name.as_str() {
                "model" => model = Some(v.clone()),
                "computers" => computers = Some(v.clone()),
                _ => {}
            }
        }
    }
    Some((model?, computers?))
}

pub async fn handle(interaction: &Interaction, data: &CommandData) -> Result<InteractionResponse> {
    // Hard owner gate.
    let invoker = interaction.author().map(|u| u.id.get());
    if invoker != Some(OWNER_ID) {
        return Ok(create_response("This command is owner-only."));
    }

    let Some((model, machines)) = parse_options(data) else {
        return Ok(create_response("Could not read the model/computers options."));
    };

    // Client-side sanity so the owner gets an instant, clear rejection (the
    // scripts re-validate server-side as defense in depth).
    if machines == "server" && model != "1.1b" {
        return Ok(create_response(
            "**server** (GTX 1650, 4GB) only hosts the **1.1B**. For 3B/16B pick desktop or desktop+louise.",
        ));
    }
    if model == "16b" && !machines.split(',').any(|m| m == "louise") {
        return Ok(create_response(
            "**16B** needs at least **desktop + louise** (a single 8GB GPU can't hold it).",
        ));
    }

    // Fire the orchestration. Both triggers return quickly (the slow work is
    // detached), so we stay well inside Discord's 3s response window.
    let launched = if machines == "server" {
        spawn_server_mode().await
    } else {
        spawn_pi_apply(&model, &machines).await
    };

    let pretty_machines = machines.replace(',', " + ");
    let msg = if launched {
        if machines == "server" {
            format!(
                "🛠️ Switching to **{model}** on **nixos-server** (tensor-ash). \
                 Freeing the desktop/louise GPUs and repointing myself — I'll blip and be back in a few seconds."
            )
        } else {
            format!(
                "🛠️ Applying **{model}** on **{pretty_machines}**. \
                 Waking the needed machines (Wake-on-LAN) and loading the model over RPC — \
                 this takes a minute or two, then I'll restart onto the new brain. \
                 (Fallback anytime: `/set config` → 1.1B · server.)"
            )
        }
    } else {
        format!(
            "⚠️ Couldn't kick off **{model}** on **{pretty_machines}** — the trigger process failed to start. \
             Check `journalctl --user -u discord-bot` and the Pi's `sig-apply-config`."
        )
    };
    Ok(create_response(&msg))
}

/// Local (nixos-server) switch to 1.1B tensor-ash. Returns true if the helper
/// launched. The helper schedules the bot restart on a delay itself.
async fn spawn_server_mode() -> bool {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/gusahlg".to_string());
    let script = format!("{home}/.local/bin/sig-server-mode.sh");
    run_briefly(TokioCommand::new(&script)).await
}

/// Kick the desktop-coordinated topology off at the Pi as a detached transient
/// unit, so it survives this bot being restarted at the end of the bring-up.
async fn spawn_pi_apply(model: &str, machines: &str) -> bool {
    // Unique unit name so back-to-back applies don't collide.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let remote = format!(
        "systemd-run --user --collect --unit=sig-apply-{stamp} \
         /home/gusahlg/.local/bin/sig-apply-config {model} {machines}"
    );
    let mut cmd = TokioCommand::new(SSH_BIN);
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ConnectTimeout=8",
        "gustav-pi",
        &remote,
    ]);
    run_briefly(cmd).await
}

/// Spawn a trigger and give it up to 2.5s to finish. Triggers are fast (the
/// heavy lifting is detached), so this normally returns their real exit status;
/// if it somehow runs long we leave it going (Child is not killed on drop) and
/// optimistically report success. Returns false only if spawning itself failed.
async fn run_briefly(mut cmd: TokioCommand) -> bool {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            match tokio::time::timeout(Duration::from_millis(2500), child.wait()).await {
                Ok(Ok(status)) => status.success(),
                Ok(Err(_)) => false,
                Err(_) => true, // still running; the detached job will carry on
            }
        }
        Err(e) => {
            tracing::error!("set config: failed to spawn trigger: {e}");
            false
        }
    }
}
