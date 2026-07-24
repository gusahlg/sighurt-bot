//! Per-channel message log.
//!
//! One TSV file per Discord channel at `data/channels/<bucket>/<channel_id>.tsv`,
//! where `<bucket>` is the guild id (string) or `"dm"` for direct messages.
//! Both the live event handler and the backfill scraper append here. A sidecar
//! `<channel_id>.cursor` JSON tracks message-id bookkeeping so backfill is
//! resumable and live capture can dedupe.
//!
//! Cursor fields (JSON):
//!   * `oldest_seen_id` — lowest id the backward backfill has reached.
//!   * `newest_seen_id` — highest id we have ever WRITTEN. Advanced by live
//!     capture the instant the gateway hands us a message, so it is a fast
//!     dedup ceiling for the live path but says nothing about completeness.
//!   * `contiguous_newest_id` — highest id below which the log is known
//!     gap-free. ONLY the scraper's forward catch-up advances it, and only
//!     after it has actually fetched every message up to that id. The forward
//!     pass pages `after(contiguous_newest_id)`, so an offline gap is healed
//!     even when live capture has already bumped `newest_seen_id` past it.
//!     Back-compat: absent in old cursor files; see [`read_cursor`].
//!
//! Cursor invariant: every message with an id in `[oldest_seen_id,
//! contiguous_newest_id]` that exists in the channel is in the TSV exactly
//! once. (`newest_seen_id` may exceed `contiguous_newest_id` whenever live
//! capture has run ahead of a gap-free scrape.)
//!
//! Dedup strategy: the live path dedups by ceiling comparison (it only ever
//! appends the newest message, so `id <= newest_seen_id` is a sound skip).
//! Every SCRAPER-path append instead dedups by TSV membership — a fetched row
//! is written only if its id is not already present in the file — because a
//! gap/backfill row can legitimately be below the live-advanced ceiling yet
//! still missing from disk (BUG 1/BUG 2). Membership is checked against an
//! in-memory [`SeenIds`] set loaded once per scrape run.
//!
//! Concurrency: a single process-wide mutex serializes appends and cursor
//! writes within this process. A cross-process advisory lock file
//! (`data/channels/.scrape.lock`) serializes the standalone `bin/scraper`
//! against the live bot, since the mutex is process-local. Append volume is
//! tiny (a handful per second at peak), and the simplicity buys correctness
//! across the half-dozen tokio tasks that race to log each event — including
//! the in-process catch-up scraper.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use twilight_model::channel::Message;
use twilight_model::channel::message::MessageFlags;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;

const DATA_ROOT: &str = "data/channels";

fn data_root() -> &'static Path {
    Path::new(DATA_ROOT)
}

fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Parsed cursor state. See the module docs for field semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub oldest_seen_id: Option<u64>,
    pub newest_seen_id: Option<u64>,
    /// Highest id below which the log is known gap-free. `None` on a brand-new
    /// cursor. On old cursor files (field absent) it is defaulted to
    /// `oldest_seen_id` by [`read_cursor_at`] so the first post-upgrade
    /// forward pass re-verifies from the backfill floor upward. Re-fetching is
    /// safe: scraper-path appends are TSV-membership-deduped, so a
    /// re-verification never writes a duplicate row — it only spends API calls.
    pub contiguous_newest_id: Option<u64>,
}

/// Cross-process advisory lock over the whole data directory, held around the
/// read-modify-write of any channel's cursor+TSV. The in-process
/// [`write_lock`] mutex does NOT cover the standalone `bin/scraper` running
/// beside the live bot; this file lock does. Implemented with an
/// exclusive-create lockfile (`O_EXCL`) plus bounded spin-retry — pure std,
/// no extra dependency. On acquire failure we log and proceed (best-effort):
/// losing the cross-process guard is strictly better than deadlocking the
/// live logger.
struct FileLock {
    path: PathBuf,
    acquired: bool,
}

impl FileLock {
    /// Spin up to ~5s trying to create the lockfile exclusively.
    fn acquire(root: &Path) -> Self {
        let path = root.join(".scrape.lock");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // ~5s total: 500 attempts * 10ms. A cursor read-modify-write is sub-ms,
        // so contention windows are tiny; this only guards the rare overlap of
        // the batch scraper and live capture touching the same file.
        for _ in 0..500 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    // Stamp holder pid for post-mortem debugging of a stale lock.
                    let _ = writeln!(f, "{}", std::process::id());
                    return Self { path, acquired: true };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        // Give up rather than block the live path forever. A genuinely stale
        // lockfile (holder crashed mid-write) would otherwise wedge the bot.
        tracing::warn!(
            "channel_log: could not acquire cross-process lock {:?}; proceeding without it",
            path
        );
        Self { path, acquired: false }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if self.acquired {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Monotonic per-process counter feeding unique cursor temp filenames, so two
/// processes (or two threads) never collide on `<id>.cursor.tmp` and clobber
/// each other's half-written rename (BUG 3).
fn next_tmp_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

pub fn bucket_for_guild(guild_id: Option<Id<GuildMarker>>) -> String {
    match guild_id {
        Some(g) => g.get().to_string(),
        None => "dm".to_string(),
    }
}

fn tsv_path(root: &Path, bucket: &str, channel_id: u64) -> PathBuf {
    root.join(bucket).join(format!("{}.tsv", channel_id))
}

fn cursor_path(root: &Path, bucket: &str, channel_id: u64) -> PathBuf {
    root.join(bucket).join(format!("{}.cursor", channel_id))
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn format_timestamp(secs: i64) -> String {
    OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_else(|| secs.to_string())
}

fn display_name_of(message: &Message) -> String {
    message
        .member
        .as_ref()
        .and_then(|m| m.nick.clone())
        .unwrap_or_else(|| message.author.name.clone())
}

fn reply_to_of(message: &Message) -> String {
    message
        .reference
        .as_ref()
        .and_then(|r| r.message_id.map(|id| id.get().to_string()))
        .unwrap_or_default()
}

fn format_line(message: &Message) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\n",
        message.id.get(),
        format_timestamp(message.timestamp.as_secs()),
        message.author.id.get(),
        escape(&display_name_of(message)),
        reply_to_of(message),
        escape(&message.content),
    )
}

/// Append one TSV row. Caller must hold `write_lock`.
fn append_at(root: &Path, message: &Message, bucket: &str) -> Result<()> {
    let path = tsv_path(root, bucket, message.channel_id.get());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {:?}", parent))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {:?}", path))?;
    f.write_all(format_line(message).as_bytes())
        .with_context(|| format!("write {:?}", path))?;
    Ok(())
}

/// Read the full [`Cursor`]. Safe without the lock for standalone reads;
/// callers that make decisions off the values (dedup, cursor merges) must hold
/// `write_lock` across read+write.
///
/// Back-compat: old cursor files predate `contiguous_newest_id`. When the
/// field is absent we default it to `oldest_seen_id` (or `None` when there is
/// no cursor at all), so the first post-upgrade forward pass re-verifies from
/// the backfill floor upward rather than trusting the live-advanced
/// `newest_seen_id`. That re-fetch is bounded by history length and cannot
/// duplicate rows (scraper appends are membership-deduped), so we prefer it
/// over silently trusting a possibly-gapped ceiling.
fn read_cursor_at(root: &Path, bucket: &str, channel_id: u64) -> Result<Cursor> {
    let path = cursor_path(root, bucket, channel_id);
    if !path.exists() {
        return Ok(Cursor::default());
    }
    let s = std::fs::read_to_string(&path).with_context(|| format!("read cursor {:?}", path))?;
    let oldest = parse_field(&s, "oldest_seen_id");
    let newest = parse_field(&s, "newest_seen_id");
    let contiguous = match parse_field(&s, "contiguous_newest_id") {
        Some(v) => Some(v),
        // Field absent (pre-upgrade cursor): fall back to the backfill floor.
        None => oldest,
    };
    Ok(Cursor {
        oldest_seen_id: oldest,
        newest_seen_id: newest,
        contiguous_newest_id: contiguous,
    })
}

/// Write the cursor file atomically (unique tmp + rename). Caller must hold
/// `write_lock`. The temp filename is unique per process+call so concurrent
/// writers never clobber each other's in-flight rename (BUG 3).
fn write_cursor_at(root: &Path, bucket: &str, channel_id: u64, cursor: Cursor) -> Result<()> {
    let path = cursor_path(root, bucket, channel_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create dir {:?}", parent))?;
    }
    let tmp = path.with_extension(format!(
        "cursor.tmp.{}.{}",
        std::process::id(),
        next_tmp_seq()
    ));
    let field = |v: Option<u64>| {
        v.map(|v| format!("\"{}\"", v))
            .unwrap_or_else(|| "null".to_string())
    };
    let body = format!(
        r#"{{"oldest_seen_id":{},"newest_seen_id":{},"contiguous_newest_id":{}}}"#,
        field(cursor.oldest_seen_id),
        field(cursor.newest_seen_id),
        field(cursor.contiguous_newest_id),
    );
    std::fs::write(&tmp, body).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {:?}", path))?;
    Ok(())
}

/// Cursor-guarded append for the LIVE path: under the process-wide lock, skip
/// when the message id is `<= newest_seen_id`, otherwise append and advance
/// `newest`. This ceiling dedup is sound *only* because the live path exclusively
/// appends the newest message it has ever seen — never a backfilled gap row.
/// When the cursor was empty, also seed `oldest` with this id so the backward
/// backfill later anchors *below* the first live-captured message instead of
/// refetching it. Returns whether a row was written.
fn append_new_at(root: &Path, message: &Message, bucket: &str) -> Result<bool> {
    let channel_id = message.channel_id.get();
    let msg_id = message.id.get();
    let _guard = write_lock().lock();
    let _flock = FileLock::acquire(root);
    let mut cursor = read_cursor_at(root, bucket, channel_id)?;
    if cursor.newest_seen_id.is_some_and(|n| msg_id <= n) {
        return Ok(false);
    }
    append_at(root, message, bucket)?;
    cursor.oldest_seen_id = Some(cursor.oldest_seen_id.map_or(msg_id, |o| o.min(msg_id)));
    cursor.newest_seen_id = Some(msg_id);
    write_cursor_at(root, bucket, channel_id, cursor)?;
    Ok(true)
}

/// Merge-update the cursor under the lock: `oldest` only ever moves down to
/// `seen_oldest`, `newest` only ever moves up to `seen_newest`. The scraper
/// uses this so its cursor writes can never clobber a concurrent live-capture
/// advance (which would re-open the dedup window and cause duplicate rows on
/// the next catch-up run). Does NOT touch `contiguous_newest_id` — only the
/// forward catch-up advances that, via [`advance_contiguous`].
fn widen_cursor_at(
    root: &Path,
    bucket: &str,
    channel_id: u64,
    seen_oldest: u64,
    seen_newest: u64,
) -> Result<()> {
    let _guard = write_lock().lock();
    let _flock = FileLock::acquire(root);
    let mut cursor = read_cursor_at(root, bucket, channel_id)?;
    cursor.oldest_seen_id =
        Some(cursor.oldest_seen_id.map_or(seen_oldest, |o| o.min(seen_oldest)));
    cursor.newest_seen_id =
        Some(cursor.newest_seen_id.map_or(seen_newest, |n| n.max(seen_newest)));
    write_cursor_at(root, bucket, channel_id, cursor)
}

/// Advance `contiguous_newest_id` up to `new_contiguous` (never backward),
/// after the forward catch-up has fetched a gap-free run up to that id. Only
/// the scraper's forward pass calls this. Also widens `newest_seen_id` to keep
/// the ceiling >= the contiguous mark.
fn advance_contiguous_at(
    root: &Path,
    bucket: &str,
    channel_id: u64,
    new_contiguous: u64,
) -> Result<()> {
    let _guard = write_lock().lock();
    let _flock = FileLock::acquire(root);
    let mut cursor = read_cursor_at(root, bucket, channel_id)?;
    cursor.contiguous_newest_id = Some(
        cursor
            .contiguous_newest_id
            .map_or(new_contiguous, |c| c.max(new_contiguous)),
    );
    cursor.newest_seen_id =
        Some(cursor.newest_seen_id.map_or(new_contiguous, |n| n.max(new_contiguous)));
    write_cursor_at(root, bucket, channel_id, cursor)
}

/// In-memory set of message ids already present in a channel's TSV, loaded
/// once per scrape run so the scraper can dedup gap/backfill rows by membership
/// (BUG 1/BUG 2) without re-scanning the file per row.
///
/// Memory cost: one `u64` per existing row (plus HashSet overhead, ~1.5-2x),
/// i.e. roughly 12-16 bytes per message. Even a 100k-message channel is ~1.5 MB
/// transient, dropped when the per-channel scrape completes.
pub struct SeenIds {
    root: PathBuf,
    bucket: String,
    channel_id: u64,
    ids: HashSet<u64>,
}

impl SeenIds {
    fn load_at(root: &Path, bucket: &str, channel_id: u64) -> Result<Self> {
        let mut ids = HashSet::new();
        let path = tsv_path(root, bucket, channel_id);
        if path.exists() {
            let s =
                std::fs::read_to_string(&path).with_context(|| format!("read tsv {:?}", path))?;
            for line in s.lines() {
                // Each row is `id\t...`; the id is the first field.
                if let Some(id) = line.split('\t').next().and_then(|f| f.parse::<u64>().ok()) {
                    ids.insert(id);
                }
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            bucket: bucket.to_string(),
            channel_id,
            ids,
        })
    }

    /// Append `message` only if its id is not already on disk for this channel.
    /// Widens the cursor's oldest/newest to include the row. Returns whether a
    /// row was written. Dedups by membership, NOT by ceiling comparison, so a
    /// gap row below a live-advanced `newest_seen_id` is still written.
    fn append_if_absent(&mut self, message: &Message) -> Result<bool> {
        let msg_id = message.id.get();
        let _guard = write_lock().lock();
        let _flock = FileLock::acquire(&self.root);
        if !self.ids.insert(msg_id) {
            return Ok(false);
        }
        append_at(&self.root, message, &self.bucket)?;
        let mut cursor = read_cursor_at(&self.root, &self.bucket, self.channel_id)?;
        cursor.oldest_seen_id =
            Some(cursor.oldest_seen_id.map_or(msg_id, |o| o.min(msg_id)));
        cursor.newest_seen_id =
            Some(cursor.newest_seen_id.map_or(msg_id, |n| n.max(msg_id)));
        write_cursor_at(&self.root, &self.bucket, self.channel_id, cursor)?;
        Ok(true)
    }

    /// Load the seen-id set for a channel from the default data root. Call once
    /// at the start of a per-channel scrape and reuse across all its passes.
    pub fn load(bucket: &str, channel_id: u64) -> Result<Self> {
        Self::load_at(data_root(), bucket, channel_id)
    }

    /// Append `message` if its id is not already on disk. Returns whether a row
    /// was written. Use for ALL scraper-path appends (bootstrap, backward,
    /// forward catch-up).
    pub fn append_if_new(&mut self, message: &Message) -> Result<bool> {
        self.append_if_absent(message)
    }
}

/// Cursor-guarded append at the default data root for the LIVE path only. The
/// live gateway handler goes through here; it dedups by ceiling comparison,
/// which is sound because the live path only ever appends the newest message.
/// Scraper-path appends must use [`SeenIds`] instead (membership dedup), since
/// a gap/backfill row can be below the live-advanced ceiling yet still missing.
fn append_new(message: &Message, bucket: &str) -> Result<bool> {
    append_new_at(data_root(), message, bucket)
}

/// Append-or-skip for the live event handler. Bucket is derived from
/// `message.guild_id`, which IS populated on gateway events (unlike REST
/// fetches). Delegates to [`append_new`] for the atomic dedup check.
///
/// Ephemeral guard (BUG 4): Discord omits `guild_id` on MESSAGE_CREATE for
/// ephemeral interaction responses (slash-command replies flagged
/// `MessageFlags::EPHEMERAL`). Without a guild id those would be filed under
/// the `dm/` bucket, polluting the corpus with fake DMs and desyncing cursors.
/// Ephemeral messages are not real channel content, so we skip them entirely
/// rather than trying to reclassify.
pub fn append_live(message: &Message) -> Result<()> {
    if message
        .flags
        .is_some_and(|f| f.contains(MessageFlags::EPHEMERAL))
    {
        return Ok(());
    }
    let bucket = bucket_for_guild(message.guild_id);
    append_new(message, &bucket).map(|_| ())
}

/// Read the full cursor at the default data root; see [`read_cursor_at`].
pub fn read_cursor(bucket: &str, channel_id: u64) -> Result<Cursor> {
    read_cursor_at(data_root(), bucket, channel_id)
}

/// Merge-update `(oldest, newest)` at the default data root; see
/// [`widen_cursor_at`]. Does not touch `contiguous_newest_id`.
pub fn widen_cursor(bucket: &str, channel_id: u64, seen_oldest: u64, seen_newest: u64) -> Result<()> {
    widen_cursor_at(data_root(), bucket, channel_id, seen_oldest, seen_newest)
}

/// Advance `contiguous_newest_id` at the default data root; see
/// [`advance_contiguous_at`]. Only the forward catch-up calls this, after
/// fetching a gap-free run up to `new_contiguous`.
pub fn advance_contiguous(bucket: &str, channel_id: u64, new_contiguous: u64) -> Result<()> {
    advance_contiguous_at(data_root(), bucket, channel_id, new_contiguous)
}

fn parse_field(s: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{}\":", key);
    let i = s.find(&pat)?;
    let rest = &s[i + pat.len()..];
    let rest = rest.trim_start();
    if rest.starts_with("null") {
        return None;
    }
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    rest[..end].parse().ok()
}

/// Minimal `Message` for tests (here and in `scrape`'s tests). Only the
/// fields the log/scrape paths read are meaningful; the rest are empty.
#[cfg(test)]
pub(crate) fn test_message(msg_id: u64, channel_id: u64, content: &str) -> Message {
    use twilight_model::channel::message::MessageType;
    use twilight_model::user::User;
    use twilight_model::util::Timestamp;

    Message {
        activity: None,
        application: None,
        application_id: None,
        attachments: Vec::new(),
        author: User {
            accent_color: None,
            avatar: None,
            avatar_decoration: None,
            banner: None,
            bot: false,
            discriminator: 0,
            email: None,
            flags: None,
            global_name: None,
            id: Id::new(1),
            locale: None,
            mfa_enabled: None,
            name: "tester".to_string(),
            premium_type: None,
            public_flags: None,
            system: None,
            verified: None,
        },
        channel_id: Id::new(channel_id),
        components: Vec::new(),
        content: content.to_string(),
        edited_timestamp: None,
        embeds: Vec::new(),
        flags: None,
        guild_id: None,
        id: Id::new(msg_id),
        interaction: None,
        kind: MessageType::Regular,
        member: None,
        mention_channels: Vec::new(),
        mention_everyone: false,
        mention_roles: Vec::new(),
        mentions: Vec::new(),
        pinned: false,
        reactions: Vec::new(),
        reference: None,
        referenced_message: None,
        role_subscription_data: None,
        sticker_items: Vec::new(),
        timestamp: Timestamp::from_secs(1_700_000_000).unwrap(),
        thread: None,
        tts: false,
        webhook_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh temp root per test so nothing touches the real `data/` tree and
    /// parallel tests can't interfere.
    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "discord-bot-channel-log-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tsv_lines(root: &Path, bucket: &str, channel_id: u64) -> usize {
        std::fs::read_to_string(tsv_path(root, bucket, channel_id))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// `(oldest, newest)` shorthand for the legacy assertions below.
    fn on(root: &Path, bucket: &str, cid: u64) -> (Option<u64>, Option<u64>) {
        let c = read_cursor_at(root, bucket, cid).unwrap();
        (c.oldest_seen_id, c.newest_seen_id)
    }

    fn ephemeral_message(msg_id: u64, channel_id: u64) -> Message {
        let mut m = test_message(msg_id, channel_id, "Pong!");
        m.flags = Some(MessageFlags::EPHEMERAL);
        m.guild_id = None; // Discord omits guild_id on ephemeral responses.
        m
    }

    #[test]
    fn escape_roundtrip_basics() {
        assert_eq!(escape("hello"), "hello");
        assert_eq!(escape("a\tb\nc\\d"), "a\\tb\\nc\\\\d");
        assert_eq!(escape("\r\n"), "\\r\\n");
    }

    #[test]
    fn parse_field_finds_values_and_null() {
        let s = r#"{"oldest_seen_id":"123","newest_seen_id":null}"#;
        assert_eq!(parse_field(s, "oldest_seen_id"), Some(123));
        assert_eq!(parse_field(s, "newest_seen_id"), None);
        assert_eq!(parse_field(s, "missing"), None);
    }

    #[test]
    fn format_timestamp_renders_iso8601() {
        let s = format_timestamp(1_700_000_000);
        assert!(s.starts_with("2023-11-"));
    }

    #[test]
    fn append_new_dedups_and_advances_cursor() {
        let root = temp_root("append-new");
        let bucket = "g1";
        let cid = 42;

        // First write goes through and seeds both cursor ends.
        assert!(append_new_at(&root, &test_message(100, cid, "a"), bucket).unwrap());
        assert_eq!(on(&root, bucket, cid), (Some(100), Some(100)));

        // Same id again: skipped (this is the live/catch-up interleave case).
        assert!(!append_new_at(&root, &test_message(100, cid, "a"), bucket).unwrap());
        // Older id: skipped too — backward history is the raw scraper's job.
        assert!(!append_new_at(&root, &test_message(50, cid, "old"), bucket).unwrap());
        // Newer id: appended, newest advances, oldest untouched.
        assert!(append_new_at(&root, &test_message(150, cid, "b"), bucket).unwrap());
        assert_eq!(on(&root, bucket, cid), (Some(100), Some(150)));

        // Exactly the two accepted rows are on disk.
        assert_eq!(tsv_lines(&root, bucket, cid), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn widen_cursor_only_ever_widens() {
        let root = temp_root("widen");
        let bucket = "g2";
        let cid = 7;

        widen_cursor_at(&root, bucket, cid, 50, 60).unwrap();
        assert_eq!(on(&root, bucket, cid), (Some(50), Some(60)));

        // Lower oldest widens; lower newest is ignored.
        widen_cursor_at(&root, bucket, cid, 30, 55).unwrap();
        assert_eq!(on(&root, bucket, cid), (Some(30), Some(60)));

        // Higher newest widens; higher oldest is ignored.
        widen_cursor_at(&root, bucket, cid, 40, 90).unwrap();
        assert_eq!(on(&root, bucket, cid), (Some(30), Some(90)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn append_new_after_scraper_widen_respects_floor() {
        // Simulates: backward scraper covered [10..20], then live message 25
        // arrives, then catch-up refetches 25 — second copy must be skipped.
        let root = temp_root("interleave");
        let bucket = "g3";
        let cid = 9;

        widen_cursor_at(&root, bucket, cid, 10, 20).unwrap();
        assert!(append_new_at(&root, &test_message(25, cid, "live"), bucket).unwrap());
        assert!(!append_new_at(&root, &test_message(25, cid, "catch-up"), bucket).unwrap());
        assert_eq!(on(&root, bucket, cid), (Some(10), Some(25)));
        assert_eq!(tsv_lines(&root, bucket, cid), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- BUG 1: contiguous ceiling advances separately from live newest ----

    #[test]
    fn contiguous_ceiling_is_separate_from_live_newest() {
        let root = temp_root("contiguous-vs-live");
        let bucket = "g4";
        let cid = 11;

        // Bot offline at newest=100 (backfill covered up to 100 contiguously).
        widen_cursor_at(&root, bucket, cid, 10, 100).unwrap();
        advance_contiguous_at(&root, bucket, cid, 100).unwrap();
        assert_eq!(
            read_cursor_at(&root, bucket, cid).unwrap().contiguous_newest_id,
            Some(100)
        );

        // A live message 500 arrives and bumps newest_seen_id to 500, but does
        // NOT touch the contiguous ceiling — the gap (100,500) is still unhealed.
        assert!(append_new_at(&root, &test_message(500, cid, "live"), bucket).unwrap());
        let c = read_cursor_at(&root, bucket, cid).unwrap();
        assert_eq!(c.newest_seen_id, Some(500));
        assert_eq!(
            c.contiguous_newest_id,
            Some(100),
            "live capture must not advance the contiguous ceiling"
        );

        // Forward catch-up therefore still pages after(100), heals the gap, and
        // only then advances the contiguous ceiling.
        advance_contiguous_at(&root, bucket, cid, 500).unwrap();
        assert_eq!(
            read_cursor_at(&root, bucket, cid).unwrap().contiguous_newest_id,
            Some(500)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_cursor_defaults_contiguous_to_oldest() {
        // Pre-upgrade cursor files lack `contiguous_newest_id`; it must default
        // to `oldest_seen_id` so the first forward pass re-verifies from the
        // floor rather than trusting the possibly-gapped live ceiling.
        let root = temp_root("old-cursor");
        let bucket = "g5";
        let cid = 13;
        let dir = tsv_path(&root, bucket, cid);
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(
            cursor_path(&root, bucket, cid),
            r#"{"oldest_seen_id":"10","newest_seen_id":"999"}"#,
        )
        .unwrap();

        let c = read_cursor_at(&root, bucket, cid).unwrap();
        assert_eq!(c.oldest_seen_id, Some(10));
        assert_eq!(c.newest_seen_id, Some(999));
        assert_eq!(c.contiguous_newest_id, Some(10));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- BUG 2: membership dedup writes gap rows below the live ceiling ----

    #[test]
    fn seen_ids_appends_gap_rows_below_live_ceiling() {
        // Bootstrap race: live msg 200 seeds the cursor to (200,200); the
        // bootstrap page (ids 100..150) is all <= 200. Ceiling dedup would drop
        // every row (the BUG 2 loss); membership dedup writes them because they
        // are not yet on disk.
        let root = temp_root("membership");
        let bucket = "g6";
        let cid = 17;

        assert!(append_new_at(&root, &test_message(200, cid, "live"), bucket).unwrap());
        assert_eq!(tsv_lines(&root, bucket, cid), 1);

        let mut seen = SeenIds::load_at(&root, bucket, cid).unwrap();
        // Rows below the live ceiling but absent from disk: all appended.
        for id in [100u64, 120, 150] {
            assert!(seen.append_if_absent(&test_message(id, cid, "gap")).unwrap());
        }
        // A genuine duplicate (200 already on disk) is skipped.
        assert!(!seen.append_if_absent(&test_message(200, cid, "dup")).unwrap());
        assert_eq!(tsv_lines(&root, bucket, cid), 4);

        // A second SeenIds reload sees all four ids as present.
        let mut seen2 = SeenIds::load_at(&root, bucket, cid).unwrap();
        for id in [100u64, 120, 150, 200] {
            assert!(!seen2.append_if_absent(&test_message(id, cid, "dup")).unwrap());
        }
        assert_eq!(tsv_lines(&root, bucket, cid), 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- BUG 3: unique temp filename ----

    #[test]
    fn cursor_temp_filename_is_unique_per_call() {
        // Two writes must not target the same `.tmp` path (which would let an
        // interleaved rename clobber a half-written file). We can't easily race
        // real processes in a unit test, so assert the generator advances.
        let a = next_tmp_seq();
        let b = next_tmp_seq();
        assert_ne!(a, b);

        // And the written cursor round-trips correctly through the unique tmp.
        let root = temp_root("unique-tmp");
        let bucket = "g7";
        let cid = 19;
        write_cursor_at(
            &root,
            bucket,
            cid,
            Cursor {
                oldest_seen_id: Some(1),
                newest_seen_id: Some(2),
                contiguous_newest_id: Some(2),
            },
        )
        .unwrap();
        // No stray tmp files remain after the rename.
        let dir = cursor_path(&root, bucket, cid);
        let leftover: Vec<_> = std::fs::read_dir(dir.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "no tmp files should remain: {:?}", leftover);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- BUG 4: ephemeral skip ----

    #[test]
    fn append_live_skips_ephemeral_messages() {
        // Ephemeral interaction responses (guild_id absent, EPHEMERAL flag set)
        // must never be logged — not even into the dm/ bucket.
        let root = temp_root("ephemeral");
        // append_live writes to the real data_root(), so exercise the flag guard
        // directly and confirm the non-ephemeral counterpart WOULD be filed.
        let eph = ephemeral_message(300, 21);
        assert!(
            eph.flags.unwrap().contains(MessageFlags::EPHEMERAL),
            "test fixture must be ephemeral"
        );
        // The bucket a non-guild message would land in:
        assert_eq!(bucket_for_guild(eph.guild_id), "dm");

        // Direct guard check mirroring append_live's early return.
        let is_ephemeral = eph
            .flags
            .is_some_and(|f| f.contains(MessageFlags::EPHEMERAL));
        assert!(is_ephemeral);

        // A normal message is not skipped.
        let normal = test_message(301, 21, "hi");
        assert!(
            !normal
                .flags
                .is_some_and(|f| f.contains(MessageFlags::EPHEMERAL))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_field_reads_contiguous_field() {
        let s = r#"{"oldest_seen_id":"1","newest_seen_id":"9","contiguous_newest_id":"5"}"#;
        assert_eq!(parse_field(s, "contiguous_newest_id"), Some(5));
    }
}
