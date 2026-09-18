//! `/set config model:<…> computers:<…>` and `/set status` — owner-only
//! control over which bespoke model Sig runs and on which machine(s).
//!
//! This is THE OWNER'S switch. It is hard-gated to a single Discord user id
//! (not "any administrator"), because it moves the live brain across machines
//! and Wake-on-LANs hardware.
//!
//! How the pieces fit (see scripts/serve_mode.sh, scripts/server/*,
//! scripts/desktop/sig-config-agent in the artificial-stupidity repo):
//!   • `… server`  → 1.1B tensor-ash entirely on nixos-server (the bot host).
//!     Handled locally by ~/.local/bin/sig-server-mode.sh; wakes nothing.
//!   • anything else → a llama.cpp topology whose coordinator is the desktop.
//!     ~/.local/bin/sig-apply-config (ON THIS HOST, no Raspberry Pi) sends the
//!     Wake-on-LAN packets itself, publishes the desired state in
//!     ~/sig-state/desired.env, and the desktop's pull agent applies it and
//!     reports back in ~/sig-state/status.env. It runs as a DETACHED transient
//!     unit so the bot restart at the end of the bring-up can't kill it.
//!
//! Because the bring-up ends by restarting this very bot, the command does not
//! try to post a "done" follow-up — it acks what it kicked off; `/set status`
//! shows where things are.

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

/// The one and only user allowed to run `/set`.
const OWNER_ID: u64 = 367334632515043329;
/// Absolute systemd-run on NixOS (the bot's service PATH may not include it).
const SYSTEMD_RUN: &str = "/run/current-system/sw/bin/systemd-run";

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

fn subcommand(name: &str, description: &str, options: Vec<CommandOption>) -> CommandOption {
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
        options: Some(options),
        required: None,
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
            string_choice("Q3 (Qwen3-14B base, native tools — next gen)", "q3"),
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
        options: vec![
            subcommand("config", "Set which model runs on which machine(s)", vec![model, computers]),
            subcommand("status", "Show which brain is live and what the last switch did", vec![]),
        ],
        version: Id::new(1),
    }
}

enum Sub {
    Config { model: String, machines: String },
    Status,
}

fn parse_options(data: &CommandData) -> Option<Sub> {
    let sub = data.options.first()?;
    let CommandOptionValue::SubCommand(inner) = &sub.value else {
        return None;
    };
    match sub.name.as_str() {
        "status" => Some(Sub::Status),
        "config" => {
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
            Some(Sub::Config { model: model?, machines: computers? })
        }
        _ => None,
    }
}

pub async fn handle(interaction: &Interaction, data: &CommandData) -> Result<InteractionResponse> {
    // Hard owner gate.
    let invoker = interaction.author().map(|u| u.id.get());
    if invoker != Some(OWNER_ID) {
        return Ok(create_response("This command is owner-only."));
    }

    match parse_options(data) {
        Some(Sub::Status) => Ok(create_response(&status_report().await)),
        Some(Sub::Config { model, machines }) => Ok(create_response(&apply(&model, &machines).await)),
        None => Ok(create_response("Could not read the options.")),
    }
}

async fn apply(model: &str, machines: &str) -> String {
    // Client-side sanity so the owner gets an instant, clear rejection (the
    // scripts re-validate server-side as defense in depth).
    if machines == "server" && model != "1.1b" {
        return "**server** (GTX 1650, 4GB) only hosts the **1.1B**. For 3B/16B pick desktop or desktop+louise.".to_string();
    }
    if matches!(model, "16b" | "q3") && !machines.split(',').any(|m| m == "louise") {
        return format!("**{model}** needs at least **desktop + louise** (a single 8GB GPU can't hold it).");
    }
    if !machines.chars().all(|c| c.is_ascii_lowercase() || c == ',') || !matches!(model, "1.1b" | "3b" | "16b" | "q3") {
        return "Unknown model/computers value.".to_string();
    }

    let launched = if machines == "server" {
        spawn_detached("sig-server-mode", &["sig-server-mode.sh"]).await
    } else {
        spawn_detached("sig-apply", &["sig-apply-config", model, machines]).await
    };

    let pretty_machines = machines.replace(',', " + ");
    if launched {
        if machines == "server" {
            format!(
                "🛠️ Switching to **{model}** on **nixos-server** (tensor-ash). \
                 Telling the desktop to free its GPUs and repointing myself — I'll blip and be back in a few seconds."
            )
        } else {
            format!(
                "🛠️ Applying **{model}** on **{pretty_machines}**. \
                 Sending Wake-on-LAN from this box, then the desktop picks up the new config and loads the model — \
                 a minute or two, then I restart onto the new brain. `/set status` shows progress. \
                 (Fallback anytime: `/set config` → 1.1B · server.)"
            )
        }
    } else {
        format!(
            "⚠️ Couldn't kick off **{model}** on **{pretty_machines}** — the trigger process failed to start. \
             Check `journalctl --user -u discord-bot` and `~/.local/state/sig-apply-config.log` on the server."
        )
    }
}

/// Launch `~/.local/bin/<script> args…` as a detached transient user unit so
/// it survives this bot being restarted at the end of the bring-up.
async fn spawn_detached(unit_prefix: &str, script_and_args: &[&str]) -> bool {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/gusahlg".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut cmd = TokioCommand::new(SYSTEMD_RUN);
    cmd.args(["--user", "--collect", &format!("--unit={unit_prefix}-{stamp}")]);
    cmd.arg(format!("{home}/.local/bin/{}", script_and_args[0]));
    cmd.args(&script_and_args[1..]);
    run_briefly(cmd).await
}

/// Spawn a trigger and give it up to 2.5s to finish. Triggers are fast (the
/// heavy lifting is detached), so this normally returns their real exit status;
/// if it somehow runs long we leave it going and optimistically report success.
/// Returns false only if spawning itself failed.
async fn run_briefly(mut cmd: TokioCommand) -> bool {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => match tokio::time::timeout(Duration::from_millis(2500), child.wait()).await {
            Ok(Ok(status)) => status.success(),
            Ok(Err(_)) => false,
            Err(_) => true,
        },
        Err(e) => {
            tracing::error!("set config: failed to spawn trigger: {e}");
            false
        }
    }
}

fn kv(file: &str, key: &str) -> Option<String> {
    std::fs::read_to_string(file).ok()?.lines().find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Human summary of the live brain + the pull-agent state files.
async fn status_report() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/gusahlg".to_string());
    let config = std::fs::read_to_string("config.toml").unwrap_or_default();
    let grab = |key: &str| {
        config
            .lines()
            .find_map(|l| l.trim().strip_prefix(key).map(|r| r.trim_start_matches([' ', '=']).trim_matches('"').to_string()))
            .unwrap_or_else(|| "?".to_string())
    };
    let endpoint = grab("endpoint_url");
    let backend = grab("backend");
    let format = grab("tool_format");
    let healthy = {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(4)).build().ok();
        let mut ok = false;
        if let Some(c) = client {
            for path in ["/health", "/healthz"] {
                if c.get(format!("{}{path}", endpoint.trim_end_matches('/'))).send().await.is_ok_and(|r| r.status().is_success()) {
                    ok = true;
                    break;
                }
            }
        }
        ok
    };
    let desired = format!("{home}/sig-state/desired.env");
    let status = format!("{home}/sig-state/status.env");
    let mut out = format!(
        "**Live brain:** `{endpoint}` (backend `{backend}`, tools `{format}`) — {}\n",
        if healthy { "✅ healthy" } else { "❌ not answering" }
    );
    if let (Some(seq), Some(action)) = (kv(&desired, "SEQ"), kv(&desired, "ACTION")) {
        out.push_str(&format!(
            "**Last request:** #{seq} `{action}` {} {} at {}\n",
            kv(&desired, "MODEL").unwrap_or_default(),
            kv(&desired, "MACHINES").unwrap_or_default(),
            kv(&desired, "REQUESTED_AT").unwrap_or_default()
        ));
    }
    match (kv(&status, "SEQ"), kv(&status, "STATE")) {
        (Some(seq), Some(state)) => out.push_str(&format!(
            "**Desktop agent:** #{seq} **{state}** — {} ({})",
            kv(&status, "DETAIL").unwrap_or_default(),
            kv(&status, "UPDATED_AT").unwrap_or_default()
        )),
        _ => out.push_str("**Desktop agent:** no status reported yet"),
    }
    out
}
