//! Per-channel message log.
//!
//! One TSV file per Discord channel at `data/channels/<bucket>/<channel_id>.tsv`,
//! where `<bucket>` is the guild id (string) or `"dm"` for direct messages.
//! Both the live event handler and the backfill scraper append here. A sidecar
//! `<channel_id>.cursor` JSON tracks the oldest+newest seen message ids so
//! backfill is resumable and live capture can dedupe.
//!
//! Concurrency: a single process-wide mutex serializes appends. Append volume
//! is tiny (a handful per second at peak), and the simplicity buys correctness
//! across the half-dozen tokio tasks that race to log each event.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use twilight_model::channel::Message;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;

const DATA_ROOT: &str = "data/channels";

fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn bucket_for_guild(guild_id: Option<Id<GuildMarker>>) -> String {
    match guild_id {
        Some(g) => g.get().to_string(),
        None => "dm".to_string(),
    }
}

fn tsv_path(bucket: &str, channel_id: u64) -> PathBuf {
    let mut p = PathBuf::from(DATA_ROOT);
    p.push(bucket);
    p.push(format!("{}.tsv", channel_id));
    p
}

fn cursor_path(bucket: &str, channel_id: u64) -> PathBuf {
    let mut p = PathBuf::from(DATA_ROOT);
    p.push(bucket);
    p.push(format!("{}.cursor", channel_id));
    p
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

/// Append a single message to a channel's TSV under the given bucket
/// (a stringified guild id, or `"dm"` for direct messages). Caller controls
/// the bucket because Discord's REST API does not populate `Message.guild_id`
/// on messages fetched via `GET /channels/{id}/messages` — only gateway
/// events do — so we can't reliably derive it from the message alone.
/// Idempotent only when paired with cursor dedup; raw `append` does not
/// deduplicate.
pub fn append(message: &Message, bucket: &str) -> Result<()> {
    let path = tsv_path(bucket, message.channel_id.get());
    let _guard = write_lock().lock();
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

/// Append-or-skip variant for the live event handler. Bucket is derived from
/// `message.guild_id`, which IS populated on gateway events. Skips when the
/// message id is `<= newest_seen_id` (dedup) and updates the cursor after
/// writing. The backfill scraper manages its own cursor via
/// `read_cursor` / `write_cursor` and calls `append` directly.
pub fn append_live(message: &Message) -> Result<()> {
    let bucket = bucket_for_guild(message.guild_id);
    let channel_id = message.channel_id.get();
    let (oldest, newest) = read_cursor(&bucket, channel_id)?;
    let msg_id = message.id.get();
    if let Some(n) = newest {
        if msg_id <= n {
            return Ok(());
        }
    }
    append(message, &bucket)?;
    write_cursor(&bucket, channel_id, oldest, Some(msg_id))?;
    Ok(())
}

/// Returns `(oldest_seen_id, newest_seen_id)`.
pub fn read_cursor(bucket: &str, channel_id: u64) -> Result<(Option<u64>, Option<u64>)> {
    let path = cursor_path(bucket, channel_id);
    if !path.exists() {
        return Ok((None, None));
    }
    let s = std::fs::read_to_string(&path)
        .with_context(|| format!("read cursor {:?}", path))?;
    Ok((parse_field(&s, "oldest_seen_id"), parse_field(&s, "newest_seen_id")))
}

pub fn write_cursor(
    bucket: &str,
    channel_id: u64,
    oldest: Option<u64>,
    newest: Option<u64>,
) -> Result<()> {
    let _guard = write_lock().lock();
    let path = cursor_path(bucket, channel_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create dir {:?}", parent))?;
    }
    let tmp = path.with_extension("cursor.tmp");
    let body = format!(
        r#"{{"oldest_seen_id":{},"newest_seen_id":{}}}"#,
        oldest
            .map(|v| format!("\"{}\"", v))
            .unwrap_or_else(|| "null".to_string()),
        newest
            .map(|v| format!("\"{}\"", v))
            .unwrap_or_else(|| "null".to_string()),
    );
    std::fs::write(&tmp, body).with_context(|| format!("write {:?}", tmp))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {:?}", path))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
