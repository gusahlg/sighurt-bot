//! Discord channel backfill + catch-up scraper.
//!
//! Walks every guild the bot is in, enumerates text-like channels *and*
//! threads (active via `GET /guilds/{id}/threads/active`, archived public via
//! `GET /channels/{id}/threads/archived/public`), then for each:
//!
//! 1. **Backward backfill** — pages `before(oldest_seen_id)` to the beginning
//!    of history. Resumable: the per-channel cursor in
//!    `data/channels/<bucket>/<id>.cursor` only widens after a batch lands,
//!    so a crash mid-scrape resumes at the right snowflake.
//! 2. **Forward catch-up** — pages `after(contiguous_newest_id)` to the
//!    present, healing any gap from time the bot spent offline. Crucially the
//!    anchor is `contiguous_newest_id` (highest id below which the log is
//!    known gap-free), NOT `newest_seen_id` (highest id ever written, which
//!    live capture bumps to any freshly-arrived message the instant the
//!    gateway delivers it) — anchoring on the latter would let one post-boot
//!    live message mark the whole offline gap below it as covered and lose it.
//!    Only this pass advances `contiguous_newest_id`, and only after fetching
//!    each gap-free run.
//!
//! All scraper-path appends dedup by TSV membership (a per-run
//! `channel_log::SeenIds` set), NOT by cursor-ceiling comparison, so a gap row
//! that happens to sit below a live-advanced ceiling is still written.
//!
//! Forum (and media) channels carry no messages themselves — their posts are
//! threads — so forum parents are only used for thread enumeration.
//! DMs are not scraped; they are captured live only.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use twilight_http::Client;
use twilight_http::error::ErrorType;
use twilight_model::channel::{Channel, ChannelType, Message};
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

use crate::channel_log;

/// 100 ms between API calls. Discord's per-route limit for message history is
/// roughly 5 req/s; twilight blocks on 429, so this is mostly to be polite.
const PACE: Duration = Duration::from_millis(100);

/// Backoff before the second and third attempt at a transiently-failing
/// fetch. Two entries = at most three attempts total.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];

/// GuildMedia (channel type 16) postdates twilight 0.15's `ChannelType`, so
/// it decodes as `Unknown(16)`. Like forums, its posts are threads.
const GUILD_MEDIA: ChannelType = ChannelType::Unknown(16);

/// Aggregate results of one scrape run, for the end-of-run summary.
#[derive(Debug, Default)]
pub struct ScrapeStats {
    /// Channels + threads we attempted to scrape.
    pub channels_scanned: u64,
    /// TSV rows actually written (dedup-skipped rows are not counted).
    pub new_messages: u64,
    /// Channel/thread ids we gave up on (403/404 or exhausted retries).
    pub skipped_channels: Vec<u64>,
}

impl ScrapeStats {
    fn merge(&mut self, other: ScrapeStats) {
        self.channels_scanned += other.channels_scanned;
        self.new_messages += other.new_messages;
        self.skipped_channels.extend(other.skipped_channels);
    }
}

/// Outcome for a single channel or thread.
pub enum ChannelOutcome {
    Scraped { new_messages: u64 },
    /// The channel is off-limits (403/404) or kept failing transiently; it
    /// was abandoned for this run. Already logged at WARN with the id.
    Skipped,
}

pub async fn scrape_all(http: Arc<Client>) -> Result<ScrapeStats> {
    let guilds = http
        .current_user_guilds()
        .await
        .context("list current user guilds")?
        .models()
        .await
        .context("decode guilds")?;

    tracing::info!("Scraping {} guild(s)", guilds.len());

    let mut stats = ScrapeStats::default();
    for guild in guilds {
        match scrape_guild(&http, guild.id).await {
            Ok(s) => stats.merge(s),
            Err(e) => tracing::warn!("guild {} failed: {}", guild.id, e),
        }
    }
    if stats.skipped_channels.is_empty() {
        tracing::info!(
            "Scrape complete: {} channel(s)/thread(s) scanned, {} new message(s), none skipped",
            stats.channels_scanned,
            stats.new_messages,
        );
    } else {
        tracing::info!(
            "Scrape complete: {} channel(s)/thread(s) scanned, {} new message(s), {} skipped: {:?}",
            stats.channels_scanned,
            stats.new_messages,
            stats.skipped_channels.len(),
            stats.skipped_channels,
        );
    }
    Ok(stats)
}

pub async fn scrape_guild(http: &Client, guild_id: Id<GuildMarker>) -> Result<ScrapeStats> {
    let channels = http
        .guild_channels(guild_id)
        .await
        .context("list guild channels")?
        .models()
        .await
        .context("decode channels")?;

    let bucket = channel_log::bucket_for_guild(Some(guild_id));
    let mut stats = ScrapeStats::default();

    // `guild_channels` never returns threads, so parents that can host them
    // (text, announcement, forum, media) feed the archived-thread listing
    // below. Forum/media parents hold no messages themselves and are NOT
    // message-scraped directly.
    let thread_parents: Vec<Id<ChannelMarker>> = channels
        .iter()
        .filter(|c| is_thread_parent(c.kind))
        .map(|c| c.id)
        .collect();

    tracing::info!(
        "guild {}: {} channel(s), {} thread parent(s)",
        guild_id,
        channels.len(),
        thread_parents.len()
    );

    for channel in &channels {
        if !is_message_channel(channel.kind) {
            continue;
        }
        let label = channel.name.as_deref().unwrap_or("?");
        scrape_one(http, &bucket, channel.id, label, &mut stats).await;
    }

    // Threads, best-effort: the guild-wide active listing plus each parent's
    // public archived listing. Private archived threads (which we may not
    // even be able to read) are intentionally left out; active private
    // threads DO appear in the active listing.
    let mut seen: HashSet<u64> = HashSet::new();
    let mut threads: Vec<(Id<ChannelMarker>, String)> = Vec::new();

    match http.active_threads(guild_id).await {
        Ok(resp) => match resp.model().await {
            Ok(listing) => {
                for t in listing.threads {
                    push_thread(&mut threads, &mut seen, &t);
                }
            }
            Err(e) => tracing::warn!("guild {}: decode active threads failed: {}", guild_id, e),
        },
        Err(e) => tracing::warn!("guild {}: active thread listing failed: {}", guild_id, e),
    }
    tokio::time::sleep(PACE).await;

    for parent in thread_parents {
        for t in archived_public_threads(http, parent).await {
            push_thread(&mut threads, &mut seen, &t);
        }
        tokio::time::sleep(PACE).await;
    }

    if !threads.is_empty() {
        tracing::info!("guild {}: {} thread(s) to scrape", guild_id, threads.len());
    }
    for (thread_id, name) in threads {
        scrape_one(http, &bucket, thread_id, &name, &mut stats).await;
    }

    Ok(stats)
}

fn push_thread(
    threads: &mut Vec<(Id<ChannelMarker>, String)>,
    seen: &mut HashSet<u64>,
    channel: &Channel,
) {
    if is_thread(channel.kind) && seen.insert(channel.id.get()) {
        let name = channel.name.clone().unwrap_or_else(|| "?".to_string());
        threads.push((channel.id, name));
    }
}

/// Scrape one channel/thread and fold the outcome into `stats`.
async fn scrape_one(
    http: &Client,
    bucket: &str,
    channel_id: Id<ChannelMarker>,
    label: &str,
    stats: &mut ScrapeStats,
) {
    stats.channels_scanned += 1;
    match scrape_channel(http, bucket, channel_id).await {
        Ok(ChannelOutcome::Scraped { new_messages }) => {
            if new_messages > 0 {
                tracing::info!("  #{} ({}): +{} msgs", label, channel_id, new_messages);
            }
            stats.new_messages += new_messages;
        }
        Ok(ChannelOutcome::Skipped) => stats.skipped_channels.push(channel_id.get()),
        Err(e) => {
            tracing::warn!("  #{} ({}) failed: {}", label, channel_id, e);
            stats.skipped_channels.push(channel_id.get());
        }
    }
}

/// All pages of a channel's public archived threads, best-effort: any error
/// (missing access, feature not applicable, decode) quietly ends the walk —
/// plenty of channels legitimately 403 this endpoint.
async fn archived_public_threads(http: &Client, channel_id: Id<ChannelMarker>) -> Vec<Channel> {
    let mut out: Vec<Channel> = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let req = match &before {
            Some(ts) => http.public_archived_threads(channel_id).before(ts),
            None => http.public_archived_threads(channel_id),
        };
        let listing = match req.await {
            Ok(resp) => match resp.model().await {
                Ok(l) => l,
                Err(e) => {
                    tracing::debug!("channel {}: decode archived threads failed: {}", channel_id, e);
                    break;
                }
            },
            Err(e) => {
                tracing::debug!(
                    "channel {}: archived thread listing unavailable: {}",
                    channel_id,
                    e
                );
                break;
            }
        };
        if listing.threads.is_empty() {
            break;
        }
        // Pages are ordered by archive_timestamp descending; the last entry's
        // timestamp is the `before` anchor for the next page.
        let next_before = listing
            .threads
            .last()
            .and_then(|t| t.thread_metadata.as_ref())
            .map(|m| m.archive_timestamp.iso_8601().to_string());
        let has_more = listing.has_more.unwrap_or(false);
        out.extend(listing.threads);
        match (has_more, next_before) {
            (true, Some(ts)) => before = Some(ts),
            _ => break,
        }
        tokio::time::sleep(PACE).await;
    }
    out
}

pub async fn scrape_channel(
    http: &Client,
    bucket: &str,
    channel_id: Id<ChannelMarker>,
) -> Result<ChannelOutcome> {
    let cid = channel_id.get();
    let cursor = channel_log::read_cursor(bucket, cid)?;
    let mut new_messages: u64 = 0;

    // Every scraper-path append (bootstrap, backward, forward catch-up) dedups
    // by TSV membership via this set, loaded once. Ceiling comparison is
    // WRONG for gap rows: a backfilled/gap message can be below the
    // live-advanced `newest_seen_id` yet still missing from disk (BUG 1/BUG 2).
    let mut seen = channel_log::SeenIds::load(bucket, cid)?;

    // Phase 0 — bootstrap: no cursor at all, i.e. we have never seen this
    // channel. Fetch the newest page (membership-deduped; live capture may be
    // appending the very same messages concurrently) to anchor the cursor,
    // then backfill below it. `bootstrap_top` is the highest id we fetched
    // here — the seed for the contiguous ceiling once the passes below finish.
    let mut backward_anchor: Option<u64> = cursor.oldest_seen_id.or(cursor.newest_seen_id);
    let mut bootstrap_top: Option<u64> = None;
    if backward_anchor.is_none() {
        let mut batch = match fetch_batch(http, channel_id, Anchor::Latest).await? {
            FetchResult::Batch(b) => b,
            FetchResult::Skip(reason) => {
                tracing::warn!("channel {}: skipped ({})", cid, reason);
                return Ok(ChannelOutcome::Skipped);
            }
        };
        if batch.is_empty() {
            // Empty channel; nothing to anchor. Live capture will seed the
            // cursor if a message ever arrives.
            return Ok(ChannelOutcome::Scraped { new_messages: 0 });
        }
        let full_page = batch.len() >= 100;
        sort_ascending(&mut batch);
        for m in &batch {
            if seen.append_if_new(m)? {
                new_messages += 1;
            }
        }
        let (lo, hi) = batch_span(&batch).expect("batch is non-empty");
        channel_log::widen_cursor(bucket, cid, lo, hi)?;
        bootstrap_top = Some(hi);
        // A short page means the whole history fit in one fetch.
        backward_anchor = full_page.then_some(lo);
        tokio::time::sleep(PACE).await;
    }

    // Phase 1 — backward backfill from the anchor to the channel's beginning.
    // Membership dedup keeps this correct even against legacy live-only cursors
    // (oldest unset, newest set) whose anchor sits at `newest`: any live rows
    // re-fetched here are already on disk and thus skipped.
    while let Some(a) = backward_anchor {
        let messages = match fetch_batch(http, channel_id, Anchor::Before(a)).await? {
            FetchResult::Batch(b) => b,
            FetchResult::Skip(reason) => {
                tracing::warn!("channel {}: skipped ({})", cid, reason);
                return Ok(ChannelOutcome::Skipped);
            }
        };
        if messages.is_empty() {
            break;
        }
        for m in &messages {
            if seen.append_if_new(m)? {
                new_messages += 1;
            }
        }
        let (lo, hi) = batch_span(&messages).expect("batch is non-empty");
        channel_log::widen_cursor(bucket, cid, lo, hi)?;
        backward_anchor = Some(lo);
        if messages.len() < 100 {
            break;
        }
        tokio::time::sleep(PACE).await;
    }

    // Phase 2 — forward catch-up: heal the gap above the CONTIGUOUS ceiling —
    // not `newest_seen_id`, which live capture bumps to any freshly-arrived
    // message id the instant the gateway delivers it. Anchoring on
    // `newest_seen_id` would let a single post-boot live message mark the whole
    // offline gap below it as covered, silently losing every gap message (BUG
    // 1). We instead page `after(contiguous_newest_id)` and advance that
    // ceiling only after actually fetching each gap-free run.
    //
    // Discord's `after` window returns the OLDEST messages above the anchor, so
    // advancing the anchor to each page's max walks upward with no gaps; every
    // id from the anchor up is fetched and membership-appended. On a brand-new
    // channel the ceiling seeds from this run's bootstrap top; on old cursor
    // files `read_cursor` defaulted it to `oldest_seen_id`, so the first pass
    // re-verifies from the floor (safe: membership dedup never duplicates).
    let cursor = channel_log::read_cursor(bucket, cid)?;
    let mut forward_anchor = cursor.contiguous_newest_id.or(bootstrap_top);
    while let Some(a) = forward_anchor {
        let mut messages = match fetch_batch(http, channel_id, Anchor::After(a)).await? {
            FetchResult::Batch(b) => b,
            FetchResult::Skip(reason) => {
                tracing::warn!("channel {}: skipped ({})", cid, reason);
                return Ok(ChannelOutcome::Skipped);
            }
        };
        if messages.is_empty() {
            // Nothing above the anchor: the ceiling is already at the present.
            // Stamp it so a missing-field old cursor stops re-verifying.
            channel_log::advance_contiguous(bucket, cid, a)?;
            break;
        }
        let full_page = messages.len() >= 100;
        sort_ascending(&mut messages);
        for m in &messages {
            if seen.append_if_new(m)? {
                new_messages += 1;
            }
        }
        let (_, hi) = batch_span(&messages).expect("batch is non-empty");
        // Everything from the old ceiling up to `hi` is now fetched gap-free.
        let new_ceiling = hi.max(a);
        channel_log::advance_contiguous(bucket, cid, new_ceiling)?;
        forward_anchor = Some(new_ceiling);
        if !full_page {
            break;
        }
        tokio::time::sleep(PACE).await;
    }

    Ok(ChannelOutcome::Scraped { new_messages })
}

/// Which end of the history to fetch a page from.
#[derive(Clone, Copy, Debug)]
enum Anchor {
    /// The newest messages in the channel.
    Latest,
    /// The 100 messages with ids strictly below this one.
    Before(u64),
    /// The 100 oldest messages with ids strictly above this one.
    After(u64),
}

enum FetchResult {
    Batch(Vec<Message>),
    /// Gave up on this channel: definite denial (403/404/...) or three
    /// transient failures in a row. Human-readable reason for the log.
    Skip(String),
}

/// One page of history with retry: transient failures (network, timeout,
/// 5xx) get up to three attempts with 1s/4s backoff; definite HTTP errors
/// (403/404/...) skip immediately.
async fn fetch_batch(
    http: &Client,
    channel_id: Id<ChannelMarker>,
    anchor: Anchor,
) -> Result<FetchResult> {
    let mut attempt: usize = 0;
    loop {
        let result = match anchor {
            Anchor::Latest => http.channel_messages(channel_id).limit(100)?.await,
            Anchor::Before(x) => {
                http.channel_messages(channel_id)
                    .before(Id::new(x))
                    .limit(100)?
                    .await
            }
            Anchor::After(x) => {
                http.channel_messages(channel_id)
                    .after(Id::new(x))
                    .limit(100)?
                    .await
            }
        };
        let err_text = match result {
            Ok(resp) => match resp.models().await {
                Ok(messages) => return Ok(FetchResult::Batch(messages)),
                // Body decode failure: could be a truncated response — retry.
                Err(e) => format!("decode messages: {}", e),
            },
            Err(e) => {
                if disposition(status_of(&e)) == FetchDisposition::Skip {
                    return Ok(FetchResult::Skip(format!("fetch denied: {}", e)));
                }
                format!("fetch: {}", e)
            }
        };
        if attempt >= RETRY_DELAYS.len() {
            return Ok(FetchResult::Skip(format!(
                "giving up after {} attempts; last error: {}",
                attempt + 1,
                err_text
            )));
        }
        tracing::debug!(
            "channel {}: transient error (attempt {}), retrying in {:?}: {}",
            channel_id,
            attempt + 1,
            RETRY_DELAYS[attempt],
            err_text
        );
        tokio::time::sleep(RETRY_DELAYS[attempt]).await;
        attempt += 1;
    }
}

/// Effective HTTP status of a twilight error, when one applies. `None` means
/// a transport-level failure (timeout, connection error, canceled request)
/// that never produced a response.
fn status_of(err: &twilight_http::Error) -> Option<u16> {
    match err.kind() {
        ErrorType::Response { status, .. } => Some(status.get()),
        ErrorType::ServiceUnavailable { .. } => Some(503),
        ErrorType::Unauthorized => Some(401),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchDisposition {
    Retry,
    Skip,
}

/// Retry policy: transport errors and 5xx are transient (retry, with
/// backoff); 429 likewise should one ever surface past twilight's built-in
/// ratelimit handling. Any other definite status (401/403/404/...) means the
/// channel is off-limits — skip it.
fn disposition(status: Option<u16>) -> FetchDisposition {
    match status {
        None => FetchDisposition::Retry,
        Some(429) => FetchDisposition::Retry,
        Some(s) if s >= 500 => FetchDisposition::Retry,
        Some(_) => FetchDisposition::Skip,
    }
}

/// Sort a fetched page into ascending-id (chronological) order. Discord
/// serves history pages newest-first; the forward pass needs oldest-first so
/// the cursor ceiling only ever rises. Sorting instead of assuming makes
/// both passes robust to either ordering.
fn sort_ascending(messages: &mut [Message]) {
    messages.sort_by_key(|m| m.id.get());
}

/// `(min, max)` message id across a batch in any order; `None` when empty.
fn batch_span(messages: &[Message]) -> Option<(u64, u64)> {
    messages.iter().fold(None, |span, m| {
        let id = m.id.get();
        Some(match span {
            None => (id, id),
            Some((lo, hi)) => (lo.min(id), hi.max(id)),
        })
    })
}

/// Channels whose message history is fetched directly. Thread kinds are
/// included defensively (listings hand us `Channel`s of those kinds), but
/// forum/media parents are NOT here — they hold no messages.
fn is_message_channel(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::GuildText
            | ChannelType::GuildAnnouncement
            | ChannelType::AnnouncementThread
            | ChannelType::PublicThread
            | ChannelType::PrivateThread
    )
}

/// Channels that can host threads (queried for archived public threads).
fn is_thread_parent(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::GuildText | ChannelType::GuildAnnouncement | ChannelType::GuildForum
    ) || kind == GUILD_MEDIA
}

fn is_thread(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::AnnouncementThread | ChannelType::PublicThread | ChannelType::PrivateThread
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_log::test_message;

    #[test]
    fn disposition_retries_transient_failures() {
        assert_eq!(disposition(None), FetchDisposition::Retry);
        assert_eq!(disposition(Some(500)), FetchDisposition::Retry);
        assert_eq!(disposition(Some(502)), FetchDisposition::Retry);
        assert_eq!(disposition(Some(503)), FetchDisposition::Retry);
        assert_eq!(disposition(Some(429)), FetchDisposition::Retry);
    }

    #[test]
    fn disposition_skips_definite_denials() {
        assert_eq!(disposition(Some(401)), FetchDisposition::Skip);
        assert_eq!(disposition(Some(403)), FetchDisposition::Skip);
        assert_eq!(disposition(Some(404)), FetchDisposition::Skip);
        assert_eq!(disposition(Some(400)), FetchDisposition::Skip);
    }

    #[test]
    fn batches_normalize_to_ascending_order() {
        // Discord serves pages newest-first; the forward pass must not
        // depend on that. Feed a shuffled page and check both helpers.
        let mut batch = vec![
            test_message(30, 1, "c"),
            test_message(10, 1, "a"),
            test_message(20, 1, "b"),
        ];
        assert_eq!(batch_span(&batch), Some((10, 30)));
        sort_ascending(&mut batch);
        let ids: Vec<u64> = batch.iter().map(|m| m.id.get()).collect();
        assert_eq!(ids, vec![10, 20, 30]);
    }

    #[test]
    fn batch_span_empty_is_none() {
        assert_eq!(batch_span(&[]), None);
    }

    #[test]
    fn channel_kind_classification() {
        // Forum/media parents host threads but are never message-scraped.
        assert!(is_thread_parent(ChannelType::GuildForum));
        assert!(is_thread_parent(GUILD_MEDIA));
        assert!(!is_message_channel(ChannelType::GuildForum));
        assert!(!is_message_channel(GUILD_MEDIA));

        assert!(is_message_channel(ChannelType::GuildText));
        assert!(is_thread_parent(ChannelType::GuildText));

        assert!(is_thread(ChannelType::PublicThread));
        assert!(is_thread(ChannelType::PrivateThread));
        assert!(is_thread(ChannelType::AnnouncementThread));
        assert!(!is_thread(ChannelType::GuildText));
        assert!(!is_thread(ChannelType::GuildForum));

        // Voice/category/DM kinds stay out of everything.
        assert!(!is_message_channel(ChannelType::GuildVoice));
        assert!(!is_thread_parent(ChannelType::GuildCategory));
        assert!(!is_message_channel(ChannelType::Private));
    }
}
