use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::ai::AiProcessor;
use crate::automod::{AutoMod, AutoModAction};
use crate::channel_state::ChannelState;
use crate::chat::{ChatContextMessage, ChatReplyTo, ChatRequest, ChatRuntime};
use crate::reply_filter::ReplyScreen;
use crate::voice::{self, VoiceBridge};
use crate::web_search::{explicit_search_query, WebSearchContext, WebSearchResult};
use anyhow::Result;
use discord_bot::channel_log;
use twilight_http::Client;
use twilight_model::channel::message::{AllowedMentions, Mention};
use twilight_model::channel::Message;
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

const DISCORD_MAX_MESSAGE_LEN: usize = 1900;

/// Minimum spacing between bot reactions per channel — an emoji here and
/// there is charming, a bot that reacts to everything is a nuisance.
const REACTION_MIN_GAP: Duration = Duration::from_secs(45);

/// Abort a spawned background task (e.g. the typing-indicator loop) when the
/// owning scope exits, on every path including `?` and panics.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn handle_message(
    message: &Message,
    http: &Arc<Client>,
    automod: &AutoMod,
    ai: &AiProcessor,
    chat: Option<&Arc<ChatRuntime>>,
    voice_bridge: Option<&Arc<VoiceBridge>>,
    bot_user_id: Id<UserMarker>,
    channel_state: &Arc<ChannelState>,
    chat_min_reply_gap: Duration,
) -> Result<()> {
    // Log every message we see (including our own outgoing replies and
    // webhook messages) so the corpus captures the full picture. Failures
    // are non-fatal — we'd rather drop a log line than break message
    // handling. The scraper handles backfill; this is the forward edge.
    if let Err(e) = channel_log::append_live(message) {
        tracing::warn!("channel_log append failed: {}", e);
    }

    // Track the author in per-channel state BEFORE any filtering: the
    // recent-authors map wants to see bots and webhooks too (mention
    // resolution), and a human message resets the bot-chain counter.
    channel_state.record_message(
        message.channel_id,
        message.author.id,
        &display_name_of(message),
        message.author.bot,
    );

    // Our own messages and webhooks never get past logging — replying to
    // ourselves is the shortest possible infinite loop.
    if message.author.id == bot_user_id || message.webhook_id.is_some() {
        return Ok(());
    }

    // Humans get the full pipeline. Other bots skip automod, the AI-moderation
    // stub and the `!ai`/`!voice` command parsing — the only thing a bot can
    // reach is the chat trigger below (gated by respond_to_bots + chain guard).
    if !message.author.bot {
        let action = automod.check_message(message).await?;
        match action {
            AutoModAction::None => {
                if ai.is_enabled() {
                    if let Some(guild_id) = message.guild_id {
                        if let Some(response) = ai.should_moderate(&message.content, guild_id).await
                        {
                            if response.should_moderate && response.is_high_confidence() {
                                tracing::info!(
                                    "AI flagged message in guild {} (confidence: {:.2}): {:?}",
                                    guild_id,
                                    response.confidence,
                                    response.reason
                                );
                            }
                        }
                    }
                }
            }
            _ => {
                automod.execute_action(action, message).await?;
                return Ok(());
            }
        }

        // Admin command path: `!ai on|off|status` and `!filter on|off|status`.
        // Handled before the DM/mention gate so admins can toggle from any
        // channel without @-mentioning the bot.
        if let Some(chat) = chat {
            if let Some(cmd) = parse_toggle_command(&message.content, "!ai") {
                handle_ai_command(cmd, message, http, chat).await;
                return Ok(());
            }
            if let Some(cmd) = parse_filter_word_command(&message.content) {
                handle_filter_word_command(cmd, message, http, chat).await;
                return Ok(());
            }
            if let Some(cmd) = parse_toggle_command(&message.content, "!filter") {
                handle_filter_command(cmd, message, http, chat).await;
                return Ok(());
            }
        }

        // Voice command path: `!voice join|leave|status`. Same any-channel
        // ergonomics as `!ai`; voice runs entirely server-side so there's no
        // admin gate (server perms already control who can talk in voice).
        if let Some(bridge) = voice_bridge {
            if let Some(cmd) = voice::commands::parse(&message.content) {
                voice::commands::handle(cmd, message, http, bridge).await;
                return Ok(());
            }
        }
    }

    // Chat path: triggered by DMs, @-mentions, Discord replies to the bot,
    // and an unprompted jump-in roughly every N human messages per channel.
    let is_dm = message.guild_id.is_none();
    let is_mention = message.mentions.iter().any(|m| m.id == bot_user_id);
    // A reply to the bot with "notify" off carries no mention entry — catch it
    // via the resolved referenced message so suppressed replies still work.
    let is_reply_to_bot = message
        .referenced_message
        .as_ref()
        .is_some_and(|referenced| referenced.author.id == bot_user_id);
    let Some(chat) = chat else {
        return Ok(());
    };
    if !chat.is_enabled() {
        return Ok(());
    }
    if !is_dm && !is_mention && !is_reply_to_bot {
        // Unprompted jump-in: humans only, and claimed atomically so two
        // concurrent messages can't both fire. Snowflake timestamp bits give
        // the jitter entropy (the low id bits are not random).
        let unprompted = !message.author.bot
            && channel_state.try_claim_unprompted(
                message.channel_id,
                chat.unprompted_reply_every(),
                message.id.get() >> 22,
            );
        if !unprompted {
            // Not talking this time — but maybe reacting. Fire-and-forget so
            // the event handler never blocks on a reaction round-trip.
            maybe_react(message, http, chat, channel_state, bot_user_id);
            return Ok(());
        }
    }

    // Bot-authored triggers are gated by a config switch first (cheap, no
    // reservation). The per-channel chain reservation happens below, after the
    // in-flight gate, so a denied bot doesn't churn the gate.
    if message.author.bot && !chat.respond_to_bots() {
        return Ok(());
    }

    // Chat-trigger flood/concurrency gate (DoS guard). Automod is guild-only,
    // so a scripted DM flood could otherwise stack N simultaneous LLM calls
    // against the single-mutex server. Admit at most one in-flight round-trip
    // per channel and no more than one per `chat_min_reply_gap`. Everything
    // past this point MUST clear the gate via `end_chat` — the closure below
    // makes every exit path do so.
    if !channel_state.try_begin_chat(message.channel_id, chat_min_reply_gap) {
        tracing::debug!(
            "Chat trigger skipped (in-flight or cooldown) in channel {}",
            message.channel_id
        );
        return Ok(());
    }

    let result = run_chat_reply(message, http, chat, bot_user_id, channel_state).await;
    channel_state.end_chat(message.channel_id);
    result
}

/// The chat round-trip proper. Runs only after the per-channel in-flight gate
/// has admitted this trigger; the caller clears the gate afterward. Bot-chain
/// budget is reserved here (compare-and-increment) at the decision point and
/// released if we don't actually deliver a reply.
async fn run_chat_reply(
    message: &Message,
    http: &Arc<Client>,
    chat: &ChatRuntime,
    bot_user_id: Id<UserMarker>,
    channel_state: &ChannelState,
) -> Result<()> {
    // Bot-chain reservation: atomically claim a slot BEFORE the LLM call so N
    // concurrent bot triggers can't all read count<max and each reply. A human
    // message resets the counter (in record_message). On any non-delivery we
    // release the slot so failures don't consume the budget.
    let reserved_bot_slot = if message.author.bot {
        if !channel_state.try_reserve_bot_chain(message.channel_id, chat.max_bot_chain()) {
            tracing::info!(
                "Bot chain limit ({}) reached in channel {}; not replying to bot {}",
                chat.max_bot_chain(),
                message.channel_id,
                message.author.id
            );
            return Ok(());
        }
        true
    } else {
        false
    };

    // Helper: release the bot-chain reservation on any early return that did
    // not deliver a reply.
    let release = || {
        if reserved_bot_slot {
            channel_state.release_bot_chain_slot(message.channel_id);
        }
    };

    let prompt = humanize_mentions(&message.content, &message.mentions, bot_user_id, |id| {
        channel_state.display_name(message.channel_id, id)
    });
    let prompt = prompt.trim();
    if prompt.is_empty() {
        release();
        return Ok(());
    }

    // Show "SuperSighurt is typing…" for the whole think. One trigger lasts
    // ~10s, so re-fire every 8s until the reply is posted (the task is
    // aborted on every exit path below via the guard's Drop).
    let _typing = {
        let http = Arc::clone(http);
        let channel_id = message.channel_id;
        AbortOnDrop(tokio::spawn(async move {
            loop {
                let _ = http.create_typing_trigger(channel_id).await;
                tokio::time::sleep(Duration::from_secs(8)).await;
            }
        }))
    };

    // Reply context: prefer the gateway-provided referenced message, fall
    // back to a best-effort REST fetch when only the bare reference is there.
    let referenced = load_referenced_message(message, http).await;
    let reply_to = referenced.as_ref().map(|r| {
        let text = humanize_mentions(&r.content, &r.mentions, bot_user_id, |id| {
            channel_state.display_name(message.channel_id, id)
        });
        ChatReplyTo {
            message_id: r.id.get(),
            user: display_name_of(r),
            user_id: r.author.id.get(),
            text: truncate_chars(text.trim(), chat.reply_context_max_chars()),
            is_bot: r.author.bot,
            is_self: r.author.id == bot_user_id,
        }
    });

    // A mention normally occurs at the end of an ambient channel exchange.
    // Fetch that exchange explicitly instead of asking the model to infer it
    // from the trigger alone. This is best-effort: a transient REST/permission
    // failure leaves an empty context but never blocks the reply path.
    let context = load_recent_context(
        message,
        http,
        bot_user_id,
        channel_state,
        chat.recent_context_messages(),
        chat.context_message_max_chars(),
    )
    .await;

    // Retrieval is opt-in by wording: ordinary conversation never creates
    // external traffic. A failed/empty search is still represented so the LLM
    // can be transparent instead of silently substituting stale model memory.
    let web_search = match (explicit_search_query(prompt), chat.web_search()) {
        (Some(query), Some(search_client)) => match search_client.search(&query).await {
            Ok(search) => {
                tracing::info!(
                    "Live search query {:?} returned {} usable results",
                    search.query,
                    search.results.len()
                );
                Some(search)
            }
            Err(error) => {
                tracing::warn!("Live search failed for {:?}: {:#}", query, error);
                Some(WebSearchContext {
                    query,
                    results: Vec::new(),
                })
            }
        },
        _ => None,
    };

    let request = ChatRequest {
        channel_id: message.channel_id.get(),
        user: display_name_of(message),
        user_id: message.author.id.get(),
        user_is_bot: message.author.bot,
        input: prompt.to_string(),
        context,
        reply_to,
        web_search,
        react: false,
    };

    match chat.client().reply(&request).await {
        Ok(reply) => {
            let trimmed = reply.trim();
            if trimmed.is_empty() {
                tracing::debug!("LLM returned empty reply; skipping post");
                release();
                return Ok(());
            }
            // Word filter: screen the model's own output BEFORE ping
            // resolution so names are still plain text. A rejected reply is
            // never posted; only the person being replied to learns why (DM).
            if let ReplyScreen::Rejected { matched, reason } =
                chat.filter().screen(trimmed).await
            {
                tracing::info!(
                    "Chat reply withheld by word filter in channel {} ({}; matched {:?})",
                    message.channel_id,
                    reason,
                    matched
                );
                notify_rejected_reply(http, message).await;
                // Public, in-character notice so the room can see the
                // moderation working (rate-limited to avoid spam).
                if chat.notify_on_rejection() && notice_allowed(chat, channel_state, message) {
                    post_notice(
                        http,
                        message,
                        "🛡️ nope — my own filter just yoinked that reply before it hit the chat. some things even i don't get to say.",
                    )
                    .await;
                }
                release();
                return Ok(());
            }
            // Turn the model's `@name` tokens into real pings. Truncate to a
            // safe length FIRST so ping resolution never operates on text that
            // a later cut would slice a `<@id>` token out of, then re-trim to a
            // `<@...>`-boundary-safe length after resolution.
            let with_sources = append_source_links(trimmed, request.web_search.as_ref());
            let safe_input = truncate_chars(&with_sources, DISCORD_MAX_MESSAGE_LEN);
            let (resolved_text, ping_ids) = resolve_outgoing_pings(
                &safe_input,
                message,
                referenced.as_ref(),
                channel_state,
                http,
                bot_user_id,
            )
            .await;
            let truncated = truncate_no_split_mention(&resolved_text, DISCORD_MAX_MESSAGE_LEN);
            // Ping exactly the users we resolved, plus the reply target. The
            // http client's default (set in main) suppresses everything, so
            // this override is the ONLY place the bot can ping anyone.
            let allowed = AllowedMentions {
                parse: Vec::new(),
                replied_user: true,
                roles: Vec::new(),
                users: ping_ids,
            };
            match http
                .create_message(message.channel_id)
                .reply(message.id)
                .allowed_mentions(Some(&allowed))
                .content(&truncated)
            {
                Ok(builder) => {
                    if let Err(e) = builder.await {
                        // Display alone is terse ("Parsing or sending the
                        // response failed"); the cause is in the source chain.
                        tracing::warn!("Failed to post chat reply: {e} ({e:?})");
                        // Send failed: don't let the failed attempt consume the
                        // bot-chain budget.
                        release();
                    } else {
                        // Reply delivered: the unprompted-reply counter starts
                        // over — the bot just spoke in this channel.
                        channel_state.note_bot_reply(message.channel_id);
                    }
                    // Success: the reservation stands (this is the "reply
                    // actually sent" case for bot-triggered replies).
                }
                Err(e) => {
                    tracing::warn!("Invalid chat reply content: {}", e);
                    release();
                }
            }
        }
        Err(e) => {
            tracing::warn!("LLM call failed: {:#}", e);
            // Surface the failure instead of going silent, so the room (and
            // you) can see something broke. Kept in-voice and rate-limited;
            // the diagnostic detail stays in the log above.
            if chat.notify_on_error() && notice_allowed(chat, channel_state, message) {
                post_notice(
                    http,
                    message,
                    "💥 ugh my brain just glitched out — the model backend errored on that one. poke me again in a sec.",
                )
                .await;
            }
            release();
        }
    }

    Ok(())
}

/// Rate-limit gate for moderation/error notices: at most one per channel per
/// `notice_cooldown_secs`. A cooldown of 0 disables the limit.
fn notice_allowed(chat: &ChatRuntime, channel_state: &ChannelState, message: &Message) -> bool {
    let cooldown = Duration::from_secs(chat.notice_cooldown_secs());
    if cooldown.is_zero() {
        return true;
    }
    channel_state.try_claim_notice(message.channel_id, cooldown)
}

/// Post a short public notice as a reply to the triggering message. Best-effort:
/// failures are logged, never propagated (a notice must not itself error out).
async fn post_notice(http: &Client, message: &Message, text: &str) {
    let allowed = AllowedMentions {
        parse: Vec::new(),
        replied_user: false,
        roles: Vec::new(),
        users: Vec::new(),
    };
    match http
        .create_message(message.channel_id)
        .reply(message.id)
        .allowed_mentions(Some(&allowed))
        .content(text)
    {
        Ok(builder) => {
            if let Err(e) = builder.await {
                tracing::debug!("Failed to post notice: {e}");
            }
        }
        Err(e) => tracing::debug!("Invalid notice content: {e}"),
    }
}

/// Occasionally offer a fresh human message to the LLM as a reaction
/// opportunity: the model answers with one emoji or "pass". Sampling is a
/// cheap deterministic roll on the snowflake's millisecond bits, the
/// per-channel cooldown stops emoji spam, and the whole round-trip runs in a
/// detached task so message handling never waits on it.
fn maybe_react(
    message: &Message,
    http: &Arc<Client>,
    chat: &Arc<ChatRuntime>,
    channel_state: &Arc<ChannelState>,
    bot_user_id: Id<UserMarker>,
) {
    if message.author.bot || message.content.trim().is_empty() {
        return;
    }
    let probability = chat.react_probability();
    if probability <= 0.0 {
        return;
    }
    let roll = ((message.id.get() >> 22) % 1000) as f64 / 1000.0;
    if roll >= probability {
        return;
    }
    if !channel_state.try_claim_reaction(message.channel_id, REACTION_MIN_GAP) {
        return;
    }

    let http = Arc::clone(http);
    let chat = Arc::clone(chat);
    let channel_state = Arc::clone(channel_state);
    let message = message.clone();
    tokio::spawn(async move {
        let prompt = humanize_mentions(&message.content, &message.mentions, bot_user_id, |id| {
            channel_state.display_name(message.channel_id, id)
        });
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        // A short context window is plenty for "is this reaction-worthy" and
        // keeps the extra prompt tokens (and GPU time) small.
        let context = load_recent_context(
            &message,
            &http,
            bot_user_id,
            &channel_state,
            chat.recent_context_messages().min(6),
            chat.context_message_max_chars(),
        )
        .await;
        let request = ChatRequest {
            channel_id: message.channel_id.get(),
            user: display_name_of(&message),
            user_id: message.author.id.get(),
            user_is_bot: false,
            input: prompt,
            context,
            reply_to: None,
            web_search: None,
            react: true,
        };
        let reply = match chat.client().reply(&request).await {
            Ok(reply) => reply,
            Err(e) => {
                tracing::debug!("React round-trip failed: {:#}", e);
                return;
            }
        };
        let Some(emoji) = extract_unicode_emoji(&reply) else {
            tracing::debug!("Model passed on reacting (reply: {:?})", reply);
            return;
        };
        let reaction = twilight_http::request::channel::reaction::RequestReactionType::Unicode {
            name: &emoji,
        };
        match http
            .create_reaction(message.channel_id, message.id, &reaction)
            .await
        {
            // The gateway echoes our own ReactionAdd, which the logger skips
            // (bot reactions must not enter the training data).
            Ok(_) => tracing::info!(
                "Reacted {} to message {} in channel {}",
                emoji,
                message.id,
                message.channel_id
            ),
            Err(e) => tracing::debug!("create_reaction failed: {:#}", e),
        }
    });
}

/// Pull the first unicode emoji sequence out of an LLM reply (emoji chars
/// plus ZWJ/variation-selector continuations), or `None` when the model
/// declined ("pass") or produced no usable emoji.
fn extract_unicode_emoji(reply: &str) -> Option<String> {
    fn is_emoji(c: char) -> bool {
        matches!(
            u32::from(c),
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF
        )
    }
    fn is_continuation(c: char) -> bool {
        matches!(u32::from(c), 0x200D | 0xFE0F)
    }
    let mut chars = reply.chars().peekable();
    while let Some(c) = chars.next() {
        if !is_emoji(c) {
            continue;
        }
        let mut out = String::new();
        out.push(c);
        while let Some(&next) = chars.peek() {
            let last_was_joiner = out.chars().last().is_some_and(is_continuation);
            if is_continuation(next) || (is_emoji(next) && last_was_joiner) {
                out.push(next);
                chars.next();
            } else {
                break;
            }
            if out.chars().count() >= 12 {
                break;
            }
        }
        return Some(out);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToggleCommand {
    On,
    Off,
    Status,
}

/// Parse `<name> [on|off|status]` commands (`!ai`, `!filter`). Bare `<name>`
/// reads as status.
fn parse_toggle_command(content: &str, name: &str) -> Option<ToggleCommand> {
    let trimmed = content.trim();
    let rest = trimmed.strip_prefix(name)?;
    // Require word boundary after the name so `!aim` and similar don't match.
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.trim().to_ascii_lowercase().as_str() {
        "on" | "enable" => Some(ToggleCommand::On),
        "off" | "disable" => Some(ToggleCommand::Off),
        "status" | "" => Some(ToggleCommand::Status),
        _ => None,
    }
}

/// Admin gate shared by the runtime toggles. Status is read-only and open to
/// everyone; On/Off mutate state and require an admin allowlist entry. Posts
/// the refusal itself and returns whether the caller may proceed.
async fn toggle_authorized(
    cmd: ToggleCommand,
    what: &str,
    message: &Message,
    http: &Client,
    chat: &ChatRuntime,
) -> bool {
    if matches!(cmd, ToggleCommand::Status) {
        return true;
    }
    if !chat.has_admins() {
        let text = format!(
            "The {what} toggle is not configured (chat.admin_user_ids is empty in config.toml)."
        );
        post(http, message, &text).await;
        return false;
    }
    if !chat.is_admin(message.author.id.get()) {
        post(http, message, &format!("You're not authorized to toggle {what}.")).await;
        return false;
    }
    true
}

/// Render the reply for a toggle that was applied: `set` returns the previous
/// state, so "already ON/OFF" falls out of comparing it with the request.
fn toggle_reply(what: &str, cmd: ToggleCommand, was_on: impl FnOnce(bool) -> bool) -> String {
    match cmd {
        ToggleCommand::On => {
            if was_on(true) {
                format!("{what} is already ON.")
            } else {
                format!("{what}: ON.")
            }
        }
        ToggleCommand::Off => {
            if was_on(false) {
                format!("{what}: OFF.")
            } else {
                format!("{what} is already OFF.")
            }
        }
        ToggleCommand::Status => unreachable!("status renders its own reply"),
    }
}

async fn handle_ai_command(
    cmd: ToggleCommand,
    message: &Message,
    http: &Client,
    chat: &ChatRuntime,
) {
    if !toggle_authorized(cmd, "AI mode", message, http, chat).await {
        return;
    }
    let reply = match cmd {
        ToggleCommand::Status => format!(
            "AI mode: {}.",
            if chat.is_enabled() { "ON" } else { "OFF" }
        ),
        cmd => toggle_reply("AI mode", cmd, |v| chat.set_enabled(v)),
    };
    post(http, message, &reply).await;
}

async fn handle_filter_command(
    cmd: ToggleCommand,
    message: &Message,
    http: &Client,
    chat: &ChatRuntime,
) {
    if !toggle_authorized(cmd, "the word filter", message, http, chat).await {
        return;
    }
    let filter = chat.filter();
    let reply = match cmd {
        ToggleCommand::Status => format!(
            "Word filter: {}. AI judge: {}.",
            if filter.is_enabled() { "ON" } else { "OFF" },
            filter.judge_description()
        ),
        cmd => toggle_reply("Word filter", cmd, |v| filter.set_enabled(v)),
    };
    post(http, message, &reply).await;
}

/// `!filter add <term>`, `!filter remove <term>`, `!filter words|list`. Term is
/// everything after the verb (may contain spaces → a phrase).
enum FilterWordCommand {
    Add(String),
    Remove(String),
    List,
}

fn parse_filter_word_command(content: &str) -> Option<FilterWordCommand> {
    let rest = content.trim().strip_prefix("!filter")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    let (verb, term) = match rest.split_once(char::is_whitespace) {
        Some((verb, term)) => (verb, term.trim()),
        None => (rest, ""),
    };
    match verb.to_ascii_lowercase().as_str() {
        "add" if !term.is_empty() => Some(FilterWordCommand::Add(term.to_string())),
        "remove" | "rm" | "delete" if !term.is_empty() => {
            Some(FilterWordCommand::Remove(term.to_string()))
        }
        "words" | "list" | "terms" => Some(FilterWordCommand::List),
        _ => None,
    }
}

async fn handle_filter_word_command(
    cmd: FilterWordCommand,
    message: &Message,
    http: &Client,
    chat: &ChatRuntime,
) {
    // Managing the deny-list mutates moderation, so it's admin-only (list too —
    // the deny-list is not something to hand out publicly).
    if !chat.has_admins() {
        post(http, message, "The word filter is not configured (chat.admin_user_ids is empty).").await;
        return;
    }
    if !chat.is_admin(message.author.id.get()) {
        post(http, message, "You're not authorized to manage the word filter.").await;
        return;
    }
    let filter = chat.filter();
    let reply = match cmd {
        FilterWordCommand::Add(term) => match filter.add_term(&term) {
            crate::reply_filter::TermEdit::Added => format!("Added {term:?} to the deny-list."),
            crate::reply_filter::TermEdit::AlreadyPresent => {
                format!("{term:?} is already filtered.")
            }
            crate::reply_filter::TermEdit::NoWordsFile => {
                "No filter.words_file is configured, so there's nowhere to save terms.".to_string()
            }
            _ => format!("Couldn't add {term:?}."),
        },
        FilterWordCommand::Remove(term) => match filter.remove_term(&term) {
            crate::reply_filter::TermEdit::Removed => format!("Removed {term:?} from the deny-list."),
            crate::reply_filter::TermEdit::NotFound => {
                format!("{term:?} isn't in the admin deny-list.")
            }
            crate::reply_filter::TermEdit::BuiltIn => {
                format!("{term:?} is a built-in term and can't be removed via command.")
            }
            crate::reply_filter::TermEdit::NoWordsFile => {
                "No filter.words_file is configured.".to_string()
            }
            _ => format!("Couldn't remove {term:?}."),
        },
        FilterWordCommand::List => {
            let terms = filter.extra_terms();
            if terms.is_empty() {
                "No admin-added filter terms yet (built-in terms aren't listed).".to_string()
            } else {
                format!("Admin-added filter terms ({}): {}", terms.len(), terms.join(", "))
            }
        }
    };
    post(http, message, &reply).await;
}

/// Tell ONLY the person the bot was replying to that the reply was withheld.
/// Guild triggers get a DM (the channel itself never sees a trace); DM
/// triggers are already private, so the notice lands right there. Best-effort:
/// closed DMs just drop the notice (never fall back to posting publicly).
async fn notify_rejected_reply(http: &Client, message: &Message) {
    const NOTICE: &str = "SuperSighurt's reply to your message was rejected because it \
potentially contained text that is against the guidelines.";
    if message.author.bot {
        return;
    }
    let channel_id = if message.guild_id.is_none() {
        message.channel_id
    } else {
        let response = match http.create_private_channel(message.author.id).await {
            Ok(response) => response,
            Err(e) => {
                tracing::debug!("Failed to open DM for rejection notice: {}", e);
                return;
            }
        };
        match response.model().await {
            Ok(channel) => channel.id,
            Err(e) => {
                tracing::debug!("Failed to decode DM channel for rejection notice: {}", e);
                return;
            }
        }
    };
    match http.create_message(channel_id).content(NOTICE) {
        Ok(builder) => {
            if let Err(e) = builder.await {
                tracing::debug!("Failed to send rejection notice: {}", e);
            }
        }
        Err(e) => tracing::debug!("Invalid rejection notice content: {}", e),
    }
}

async fn post(http: &Client, message: &Message, text: &str) {
    match http.create_message(message.channel_id).content(text) {
        Ok(builder) => {
            if let Err(e) = builder.await {
                tracing::warn!("Failed to post AI command reply: {}", e);
            }
        }
        Err(e) => tracing::warn!("Invalid AI command reply content: {}", e),
    }
}

/// Guild nick if present, else global display name, else username. Works for
/// both gateway messages (member populated) and REST-fetched ones (it isn't).
fn display_name_of(message: &Message) -> String {
    message
        .member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .or_else(|| message.author.global_name.clone())
        .unwrap_or_else(|| message.author.name.clone())
}

/// Attach provider URLs outside the model's output, so an answer cannot invent
/// or silently omit the evidence that was actually retrieved. Angle-bracket
/// links suppress Discord embeds and keep the response compact.
fn append_source_links(reply: &str, search: Option<&WebSearchContext>) -> String {
    let Some(search) = search else {
        return reply.to_string();
    };
    let mut seen = std::collections::HashSet::new();
    let mut parts = Vec::new();
    for (index, result) in search.results.iter().enumerate() {
        if seen.insert(result.url.as_str()) {
            let candidate = format!("[{}] <{}>", index + 1, result.url);
            let projected = parts.iter().map(String::len).sum::<usize>()
                + candidate.len()
                + parts.len() * 3;
            if projected > 900 {
                break;
            }
            parts.push(candidate);
        }
    }
    if parts.is_empty() {
        return reply.to_string();
    }
    let suffix = format!("\n\nSources: {}", parts.join(" · "));
    let suffix_chars = suffix.chars().count();
    let reply_budget = DISCORD_MAX_MESSAGE_LEN.saturating_sub(suffix_chars);
    format!("{}{}", truncate_chars(reply, reply_budget).trim_end(), suffix)
}

/// Char-boundary-safe truncation (`chars().take` never splits a codepoint).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Like `truncate_chars`, but never cuts inside a `<...>` mention token. If the
/// `max`-char cut would land inside an unterminated `<...>` span (an opening
/// `<` with no `>` before the cut), back off to just before that span's `<`.
/// This keeps a resolved `<@123456789>` ping from being sliced into a garbage
/// tail like `<@12345`. Covers `<@id>`, `<@!id>`, `<@&role>` and `<#chan>`.
fn truncate_no_split_mention(s: &str, max: usize) -> String {
    // Byte index where a plain `max`-char cut lands.
    let cut_byte = match s.char_indices().nth(max) {
        Some((idx, _)) => idx,
        // Fewer than `max` chars: nothing to cut.
        None => return s.to_string(),
    };
    let head = &s[..cut_byte];
    // A mention token straddles the boundary iff the last `<` before the cut
    // has no closing `>` before the cut — the cut fell inside (or right after
    // the `<` of) an unterminated `<...>` span. Back off to just before that
    // `<`. This covers `<@id>`, `<@!id>`, `<@&role>`, `<#chan>` alike, and the
    // degenerate case where the cut splits the `<@` prefix itself.
    if let Some(open_rel) = head.rfind('<') {
        if !head[open_rel..].contains('>') {
            return head[..open_rel].to_string();
        }
    }
    head.to_string()
}

/// The message this one replied to, if any. The gateway populates
/// `referenced_message` on replies; REST-fetched messages (and a few edge
/// cases like deleted-but-referenced) only carry the bare `reference`, so we
/// fetch best-effort and shrug on failure.
async fn load_referenced_message(message: &Message, http: &Client) -> Option<Message> {
    if let Some(referenced) = &message.referenced_message {
        return Some((**referenced).clone());
    }
    let id = message.reference.as_ref()?.message_id?;
    match http.message(message.channel_id, id).await {
        Ok(resp) => match resp.model().await {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::debug!("Failed to decode referenced message {}: {}", id, e);
                None
            }
        },
        Err(e) => {
            tracing::debug!("Failed to fetch referenced message {}: {}", id, e);
            None
        }
    }
}

/// Fetch recent ambient channel context, oldest first. The triggering message
/// itself is excluded because it already has a dedicated `input` field.
async fn load_recent_context(
    trigger: &Message,
    http: &Client,
    bot_user_id: Id<UserMarker>,
    channel_state: &ChannelState,
    max_messages: usize,
    max_chars: usize,
) -> Vec<ChatContextMessage> {
    if max_messages == 0 {
        return Vec::new();
    }
    // Anchor strictly before the trigger. Asking for the channel's latest page
    // can race with new arrivals and make the model see messages from the
    // future as if they preceded the trigger. Config validation caps this at 50.
    let limit = max_messages.min(100) as u16;
    let request = match http
        .channel_messages(trigger.channel_id)
        .before(trigger.id)
        .limit(limit)
    {
        Ok(request) => request,
        Err(error) => {
            tracing::debug!("Failed to build recent-context request: {}", error);
            return Vec::new();
        }
    };
    let mut messages = match request.await {
        Ok(response) => match response.models().await {
            Ok(messages) => messages,
            Err(error) => {
                tracing::debug!("Failed to decode recent channel context: {}", error);
                return Vec::new();
            }
        },
        Err(error) => {
            tracing::debug!("Failed to fetch recent channel context: {}", error);
            return Vec::new();
        }
    };

    // Discord returns newest first. Keep the newest N non-webhook ambient
    // messages, then reverse into conversational order.
    messages.retain(|message| message.webhook_id.is_none());
    messages.truncate(max_messages);
    messages.reverse();

    messages
        .into_iter()
        .filter_map(|message| {
            let text = humanize_mentions(&message.content, &message.mentions, bot_user_id, |id| {
                channel_state.display_name(trigger.channel_id, id)
            });
            let text = truncate_chars(text.trim(), max_chars);
            if text.is_empty() {
                return None;
            }
            Some(ChatContextMessage {
                message_id: message.id.get(),
                user: display_name_of(&message),
                user_id: message.author.id.get(),
                text,
                is_bot: message.author.bot,
                is_self: message.author.id == bot_user_id,
                reply_to_message_id: message
                    .reference
                    .as_ref()
                    .and_then(|r| r.message_id)
                    .map(|id| id.get()),
            })
        })
        .collect()
}

/// Rewrite raw `<@id>` / `<@!id>` tokens into human-readable `@Name` text so
/// the LLM never sees snowflakes. The bot's OWN mention is dropped entirely
/// (it's the trigger token, not content). Names come from the message's
/// mentions array (nick preferred), falling back to `fallback_name` (the
/// per-channel recent-authors map) for ids the array doesn't carry — e.g. in
/// REST-fetched referenced messages. Unresolvable tokens stay raw. Channel
/// (`<#id>`) and role (`<@&id>`) tokens pass through untouched.
fn humanize_mentions(
    content: &str,
    mentions: &[Mention],
    bot_user_id: Id<UserMarker>,
    fallback_name: impl Fn(Id<UserMarker>) -> Option<String>,
) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' && i + 1 < bytes.len() && bytes[i + 1] == b'@' {
            // Candidate user mention: <@123> or <@!123>. Role mentions
            // (`<@&id>`) fail the digit check and fall through untouched.
            let mut j = i + 2;
            if j < bytes.len() && bytes[j] == b'!' {
                j += 1;
            }
            let digits_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start && j < bytes.len() && bytes[j] == b'>' {
                // Snowflakes are never 0; a malformed `<@0>` falls through
                // raw rather than panicking Id::new.
                if let Ok(raw) = content[digits_start..j].parse::<u64>() {
                    if raw != 0 {
                        let id: Id<UserMarker> = Id::new(raw);
                        if id == bot_user_id {
                            // Drop our own mention entirely.
                        } else if let Some(name) = mentions
                            .iter()
                            .find(|m| m.id == id)
                            .map(|m| {
                                m.member
                                    .as_ref()
                                    .and_then(|p| p.nick.clone())
                                    .unwrap_or_else(|| m.name.clone())
                            })
                            .or_else(|| fallback_name(id))
                        {
                            out.push('@');
                            out.push_str(&name);
                        } else {
                            out.push_str(&content[i..=j]);
                        }
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        let ch = content[i..].chars().next().expect("i is a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// A candidate `@name` span found in the LLM's reply. `start..end` covers the
/// whole replaceable region including the `@` (and the optional detokenizer
/// space in `@ name`); `name` is the single word after it. `spaced` is true
/// for the `@ name` (with-space) detokenizer form, which is resolved against
/// LOCAL context only — never a guild-wide search — so prose like "meet me @
/// noon" can't ping an uninvolved member.
#[derive(Debug, PartialEq, Eq)]
struct PingCandidate {
    start: usize,
    end: usize,
    name: String,
    spaced: bool,
}

fn is_name_char(c: char) -> bool {
    // Discord usernames are [a-z0-9._]; nicks can be anything but we only
    // match single words, so alphanumeric plus the common connectors.
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Scan reply text for `@name` / `@ name` tokens. Skips `<@...>` (already a
/// raw mention) and email-ish `a@b` (an `@` glued to a preceding word char).
fn extract_ping_candidates(text: &str) -> Vec<PingCandidate> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().expect("i is a char boundary");
        if ch == '@' && !matches!(prev, Some(p) if p == '<' || is_name_char(p)) {
            // Allow one space between `@` and the name — the model's
            // detokenizer sometimes emits "@ name". Track whether the space was
            // present: the spaced form is local-context-only (no guild search).
            let mut name_start = i + 1;
            let spaced = text[name_start..].starts_with(' ');
            if spaced {
                name_start += 1;
            }
            let name_len: usize = text[name_start..]
                .chars()
                .take_while(|&c| is_name_char(c))
                .map(char::len_utf8)
                .sum();
            if name_len > 0 {
                let name = &text[name_start..name_start + name_len];
                out.push(PingCandidate {
                    start: i,
                    end: name_start + name_len,
                    name: name.to_string(),
                    spaced,
                });
                prev = name.chars().last();
                i = name_start + name_len;
                continue;
            }
        }
        prev = Some(ch);
        i += ch.len_utf8();
    }
    out
}

/// Lookup keys for a candidate name: the word as-is, plus a variant with
/// trailing `.`/`-` stripped. Both are valid username chars, so a
/// sentence-final "thanks @gustav." parses the dot into the name — try the
/// exact form first, then the trimmed one.
fn name_variants(name: &str) -> Vec<String> {
    let mut variants = vec![name.to_string()];
    let trimmed = name.trim_end_matches(['.', '-']);
    if trimmed.len() < name.len() && !trimmed.is_empty() {
        variants.push(trimmed.to_string());
    }
    variants
}

/// Rewrite resolved candidates as `<@id>` and collect the ids (deduped, in
/// order of first use). `resolved` maps LOWERCASED names to ids; unmatched
/// candidates are left exactly as written.
fn apply_ping_resolutions(
    text: &str,
    candidates: &[PingCandidate],
    resolved: &HashMap<String, Id<UserMarker>>,
) -> (String, Vec<Id<UserMarker>>) {
    let mut out = String::with_capacity(text.len());
    let mut used: Vec<Id<UserMarker>> = Vec::new();
    let mut pos = 0;
    for cand in candidates {
        let mut hit: Option<(Id<UserMarker>, usize)> = None;
        for variant in name_variants(&cand.name) {
            if let Some(id) = resolved.get(&variant.to_lowercase()) {
                // Shrink the span when the trimmed variant matched so the
                // trailing punctuation stays in the text.
                let end = cand.end - (cand.name.len() - variant.len());
                hit = Some((*id, end));
                break;
            }
        }
        if let Some((id, end)) = hit {
            out.push_str(&text[pos..cand.start]);
            out.push_str(&format!("<@{}>", id));
            pos = end;
            if !used.contains(&id) {
                used.push(id);
            }
        }
    }
    out.push_str(&text[pos..]);
    (out, used)
}

/// Resolve a name against local context, in priority order: triggering
/// author, users mentioned in the trigger, referenced-message author, then
/// the per-channel recent-authors map. Display names AND raw usernames both
/// count, case-insensitively.
///
/// Never resolves to the bot itself: the recent-authors map records our own
/// id+name, and the model happily emits its own name, so `@SuperSighurt` would
/// otherwise self-ping. A local match on `bot_user_id` is treated as "no
/// match" and the literal `@name` is left in the text.
fn resolve_name_local(
    name: &str,
    message: &Message,
    referenced: Option<&Message>,
    channel_state: &ChannelState,
    bot_user_id: Id<UserMarker>,
) -> Option<Id<UserMarker>> {
    let needle = name.to_lowercase();
    let eq = |candidate: &str| candidate.to_lowercase() == needle;

    let hit = (|| {
        if eq(&display_name_of(message)) || eq(&message.author.name) {
            return Some(message.author.id);
        }
        for m in &message.mentions {
            let nick_hit = m
                .member
                .as_ref()
                .and_then(|p| p.nick.as_deref())
                .is_some_and(&eq);
            if nick_hit || eq(&m.name) {
                return Some(m.id);
            }
        }
        if let Some(r) = referenced {
            if eq(&display_name_of(r)) || eq(&r.author.name) {
                return Some(r.author.id);
            }
        }
        channel_state.find_by_name(message.channel_id, name)
    })();
    // Drop any resolution that points back at us.
    hit.filter(|id| *id != bot_user_id)
}

/// Last-resort resolution: ask Discord's member search and accept only an
/// exact (case-insensitive) username or nick match.
///
/// `.limit(100)` is REQUIRED: Discord defaults the limit to 1 and prefix-
/// matches, so `@gustav` in a guild containing `gustavsson` could return only
/// `gustavsson` and the exact-match filter would then fail. With up to 100
/// results the true exact match is present to match against. The bot's own id
/// is excluded so the model emitting its own name can't self-ping.
async fn search_guild_member(
    http: &Client,
    guild_id: Id<GuildMarker>,
    name: &str,
    bot_user_id: Id<UserMarker>,
) -> Option<Id<UserMarker>> {
    let request = match http.search_guild_members(guild_id, name).limit(100) {
        Ok(request) => request,
        Err(e) => {
            // Should never happen (100 is a valid limit) but don't panic.
            tracing::debug!("Invalid member-search limit for {:?}: {}", name, e);
            return None;
        }
    };
    let members = match request.await {
        Ok(resp) => match resp.models().await {
            Ok(members) => members,
            Err(e) => {
                tracing::debug!("Failed to decode member search for {:?}: {}", name, e);
                return None;
            }
        },
        Err(e) => {
            tracing::debug!("Member search failed for {:?}: {}", name, e);
            return None;
        }
    };
    let needle = name.to_lowercase();
    members
        .iter()
        .find(|m| {
            m.user.id != bot_user_id
                && (m.user.name.to_lowercase() == needle
                    || m.nick
                        .as_deref()
                        .is_some_and(|n| n.to_lowercase() == needle))
        })
        .map(|m| m.user.id)
}

/// Max DISTINCT candidate names that may trigger a guild member-search per
/// reply. Caps the REST fan-out from a degenerate reply full of `@word` tokens
/// (each unresolved name would otherwise issue up to 2 search calls).
const MAX_GUILD_SEARCH_CANDIDATES: usize = 5;

/// Resolve the LLM's `@name` tokens into real `<@id>` pings. Returns the
/// rewritten text plus the ids to allow-list on the outgoing message.
///
/// Resolution never points back at the bot itself (self-ping guard). Guild
/// member-search is bounded: at most `MAX_GUILD_SEARCH_CANDIDATES` distinct
/// names trigger a search, failed lookups are cached in `searched` so repeats
/// don't re-hit the API, and the SPACED `@ name` detokenizer form is resolved
/// against local context only (never a guild-wide search).
async fn resolve_outgoing_pings(
    reply: &str,
    message: &Message,
    referenced: Option<&Message>,
    channel_state: &ChannelState,
    http: &Client,
    bot_user_id: Id<UserMarker>,
) -> (String, Vec<Id<UserMarker>>) {
    let candidates = extract_ping_candidates(reply);
    if candidates.is_empty() {
        return (reply.to_string(), Vec::new());
    }
    let mut resolved: HashMap<String, Id<UserMarker>> = HashMap::new();
    // Lowercased names we've already run (or refused to run) a guild search
    // for. Doubles as negative cache: a name in here that isn't in `resolved`
    // failed the search and won't be retried.
    let mut searched: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut guild_searches = 0usize;
    'candidates: for cand in &candidates {
        let variants = name_variants(&cand.name);
        // Already resolved (same name appearing twice)? Skip the lookups.
        if variants
            .iter()
            .any(|v| resolved.contains_key(&v.to_lowercase()))
        {
            continue;
        }
        for variant in &variants {
            if let Some(id) =
                resolve_name_local(variant, message, referenced, channel_state, bot_user_id)
            {
                resolved.insert(variant.to_lowercase(), id);
                continue 'candidates;
            }
        }
        // Guild-only fallback: hit the member search API. Skipped entirely for
        // the SPACED `@ name` form (prose like "meet me @ noon" must not ping a
        // member named `noon`). Unknown names stay plain text — better a dead
        // "@name" than a wrong ping.
        if cand.spaced {
            continue;
        }
        if let Some(guild_id) = message.guild_id {
            for variant in &variants {
                let key = variant.to_lowercase();
                // Negative cache: already searched this name and it missed.
                if searched.contains(&key) {
                    continue;
                }
                // Cap total distinct names that trigger a REST search.
                if guild_searches >= MAX_GUILD_SEARCH_CANDIDATES {
                    tracing::debug!(
                        "Guild member-search cap ({}) reached; leaving @{} unresolved",
                        MAX_GUILD_SEARCH_CANDIDATES,
                        variant
                    );
                    break;
                }
                searched.insert(key.clone());
                guild_searches += 1;
                if let Some(id) = search_guild_member(http, guild_id, variant, bot_user_id).await {
                    resolved.insert(key, id);
                    continue 'candidates;
                }
            }
        }
    }
    apply_ping_resolutions(reply, &candidates, &resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_model::guild::{MemberFlags, PartialMember};
    use twilight_model::user::UserFlags;
    use twilight_model::util::Timestamp;

    const BOT_ID: Id<UserMarker> = Id::new(999);

    fn mention(id: u64, name: &str, nick: Option<&str>) -> Mention {
        Mention {
            avatar: None,
            bot: false,
            discriminator: 0,
            id: Id::new(id),
            member: nick.map(|n| PartialMember {
                avatar: None,
                communication_disabled_until: None,
                deaf: false,
                flags: MemberFlags::empty(),
                joined_at: Timestamp::from_secs(1_700_000_000).unwrap(),
                mute: false,
                nick: Some(n.to_string()),
                permissions: None,
                premium_since: None,
                roles: Vec::new(),
                user: None,
            }),
            name: name.to_string(),
            public_flags: UserFlags::empty(),
        }
    }

    #[test]
    fn parse_toggle_command_matches() {
        let parse = |content| parse_toggle_command(content, "!ai");
        assert_eq!(parse("!ai on"), Some(ToggleCommand::On));
        assert_eq!(parse("  !ai   ON  "), Some(ToggleCommand::On));
        assert_eq!(parse("!ai off"), Some(ToggleCommand::Off));
        assert_eq!(parse("!ai enable"), Some(ToggleCommand::On));
        assert_eq!(parse("!ai disable"), Some(ToggleCommand::Off));
        assert_eq!(parse("!ai status"), Some(ToggleCommand::Status));
        assert_eq!(parse("!ai"), Some(ToggleCommand::Status));
        // The same parser drives `!filter`.
        assert_eq!(
            parse_toggle_command("!filter off", "!filter"),
            Some(ToggleCommand::Off)
        );
        assert_eq!(
            parse_toggle_command("!filter", "!filter"),
            Some(ToggleCommand::Status)
        );
    }

    #[test]
    fn parse_toggle_command_rejects_non_matches() {
        let parse = |content| parse_toggle_command(content, "!ai");
        assert_eq!(parse("!aim for the moon"), None);
        assert_eq!(parse("ai on"), None);
        assert_eq!(parse("!ai bogus"), None);
        assert_eq!(parse("hello !ai on"), None);
        assert_eq!(parse_toggle_command("!filtering", "!filter"), None);
    }

    #[test]
    fn toggle_reply_reports_state_transitions() {
        assert_eq!(toggle_reply("AI mode", ToggleCommand::On, |_| false), "AI mode: ON.");
        assert_eq!(
            toggle_reply("AI mode", ToggleCommand::On, |_| true),
            "AI mode is already ON."
        );
        assert_eq!(
            toggle_reply("Word filter", ToggleCommand::Off, |_| true),
            "Word filter: OFF."
        );
        assert_eq!(
            toggle_reply("Word filter", ToggleCommand::Off, |_| false),
            "Word filter is already OFF."
        );
    }

    #[test]
    fn humanize_mentions_plain_username() {
        let mentions = [mention(42, "gustav", None)];
        assert_eq!(
            humanize_mentions("hi <@42>!", &mentions, BOT_ID, |_| None),
            "hi @gustav!"
        );
        // <@!id> nickname-form token resolves the same way.
        assert_eq!(
            humanize_mentions("hi <@!42>", &mentions, BOT_ID, |_| None),
            "hi @gustav"
        );
    }

    #[test]
    fn humanize_mentions_prefers_nick() {
        let mentions = [mention(42, "gustav", Some("Gurra"))];
        assert_eq!(
            humanize_mentions("yo <@42>", &mentions, BOT_ID, |_| None),
            "yo @Gurra"
        );
    }

    #[test]
    fn humanize_mentions_unknown_id_falls_back_then_stays_raw() {
        // Fallback map hit (e.g. recent-authors).
        assert_eq!(
            humanize_mentions("<@77> hej", &[], BOT_ID, |id| {
                (id == Id::new(77)).then(|| "fredrik".to_string())
            }),
            "@fredrik hej"
        );
        // No mention entry, no fallback: raw token stays.
        assert_eq!(
            humanize_mentions("<@77> hej", &[], BOT_ID, |_| None),
            "<@77> hej"
        );
    }

    #[test]
    fn humanize_mentions_removes_bot_self() {
        assert_eq!(
            humanize_mentions("<@999> hello", &[], BOT_ID, |_| None),
            " hello"
        );
        assert_eq!(
            humanize_mentions("hello <@!999>", &[], BOT_ID, |_| None),
            "hello "
        );
    }

    #[test]
    fn humanize_mentions_leaves_channels_and_roles() {
        assert_eq!(
            humanize_mentions("see <#123> and <@&456>", &[], BOT_ID, |_| None),
            "see <#123> and <@&456>"
        );
    }

    #[test]
    fn extract_candidates_plain_and_spaced() {
        let c = extract_ping_candidates("hi @gustav and @ fredrik!");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "gustav");
        assert_eq!(
            &"hi @gustav and @ fredrik!"[c[0].start..c[0].end],
            "@gustav"
        );
        assert!(!c[0].spaced);
        assert_eq!(c[1].name, "fredrik");
        assert_eq!(
            &"hi @gustav and @ fredrik!"[c[1].start..c[1].end],
            "@ fredrik"
        );
        // The spaced detokenizer form is flagged so guild-search is skipped.
        assert!(c[1].spaced);
    }

    #[test]
    fn extract_candidates_skips_raw_mentions_and_emails() {
        assert!(extract_ping_candidates("already <@123> pinged").is_empty());
        assert!(extract_ping_candidates("mail me a@b.com").is_empty());
        assert!(extract_ping_candidates("trailing @").is_empty());
        // "@ word" IS a candidate (spaced detokenizer form) — it only turns
        // into a ping if the word actually resolves to someone, so ordinary
        // prose like this stays untouched end to end.
        let c = extract_ping_candidates("just an @ sign");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "sign");
    }

    #[test]
    fn apply_resolutions_rewrites_and_collects_ids() {
        let text = "hej @gustav och @ Fredrik";
        let candidates = extract_ping_candidates(text);
        let mut resolved = HashMap::new();
        resolved.insert("gustav".to_string(), Id::<UserMarker>::new(1));
        resolved.insert("fredrik".to_string(), Id::<UserMarker>::new(2));
        let (out, ids) = apply_ping_resolutions(text, &candidates, &resolved);
        assert_eq!(out, "hej <@1> och <@2>");
        assert_eq!(ids, vec![Id::new(1), Id::new(2)]);
    }

    #[test]
    fn apply_resolutions_leaves_unknown_names_alone() {
        let text = "sorry @nobody, ask @gustav";
        let candidates = extract_ping_candidates(text);
        let mut resolved = HashMap::new();
        resolved.insert("gustav".to_string(), Id::<UserMarker>::new(1));
        let (out, ids) = apply_ping_resolutions(text, &candidates, &resolved);
        assert_eq!(out, "sorry @nobody, ask <@1>");
        assert_eq!(ids, vec![Id::new(1)]);
    }

    #[test]
    fn apply_resolutions_handles_trailing_punctuation() {
        // '.' is a valid username char, so it lands inside the candidate
        // word; the trimmed variant should still match and keep the dot.
        let text = "thanks @gustav.";
        let candidates = extract_ping_candidates(text);
        let mut resolved = HashMap::new();
        resolved.insert("gustav".to_string(), Id::<UserMarker>::new(1));
        let (out, ids) = apply_ping_resolutions(text, &candidates, &resolved);
        assert_eq!(out, "thanks <@1>.");
        assert_eq!(ids, vec![Id::new(1)]);
    }

    #[test]
    fn apply_resolutions_dedupes_repeat_pings() {
        let text = "@gustav @gustav";
        let candidates = extract_ping_candidates(text);
        let mut resolved = HashMap::new();
        resolved.insert("gustav".to_string(), Id::<UserMarker>::new(1));
        let (out, ids) = apply_ping_resolutions(text, &candidates, &resolved);
        assert_eq!(out, "<@1> <@1>");
        assert_eq!(ids, vec![Id::new(1)]);
    }

    #[test]
    fn truncate_chars_is_boundary_safe() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("ab", 5), "ab");
    }

    #[test]
    fn live_search_sources_are_appended_outside_model_text() {
        let search = WebSearchContext {
            query: "Rust ownership".to_string(),
            results: vec![WebSearchResult {
                title: "Ownership".to_string(),
                url: "https://example.test/ownership".to_string(),
                snippet: "Each value has one owner.".to_string(),
            }],
        };
        let output = append_source_links("A value has one owner. [1]", Some(&search));
        assert!(output.starts_with("A value has one owner. [1]"));
        assert!(output.ends_with("Sources: [1] <https://example.test/ownership>"));
    }

    #[test]
    fn truncate_no_split_mention_backs_off_before_open_token() {
        // A cut that would land inside `<@123>` trims to before the `<`.
        // "hi " is 3 chars, cut at 4 lands inside the mention.
        let s = "hi <@123456>";
        assert_eq!(truncate_no_split_mention(s, 4), "hi ");
        // Same string, cut past the whole mention keeps everything.
        assert_eq!(truncate_no_split_mention(s, 100), s);
    }

    #[test]
    fn truncate_no_split_mention_keeps_completed_token() {
        // The mention is fully before the cut: a normal char-cut applies.
        let s = "<@123> tail text here";
        // 8 chars = "<@123> t"
        assert_eq!(truncate_no_split_mention(s, 8), "<@123> t");
    }

    #[test]
    fn truncate_no_split_mention_no_mention_is_plain_cut() {
        assert_eq!(truncate_no_split_mention("hello world", 5), "hello");
        assert_eq!(truncate_no_split_mention("héllo", 2), "hé");
        assert_eq!(truncate_no_split_mention("ab", 5), "ab");
    }
}
