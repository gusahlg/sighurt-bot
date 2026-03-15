pub mod raid;
pub mod spam;
pub mod wordfilter;

use crate::database::models::{get_filtered_words, get_guild_settings, GuildSettings};
use anyhow::Result;
use lru::LruCache;
use parking_lot::Mutex;
use sqlx::SqlitePool;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use twilight_http::Client;
use twilight_model::{
    channel::Message,
    gateway::payload::incoming::MemberAdd,
    id::{marker::GuildMarker, Id},
    util::Timestamp,
};

pub use raid::RaidDetector;
pub use spam::SpamDetector;
pub use wordfilter::WordFilter;

/// Cache entry with TTL
struct CacheEntry<T> {
    value: T,
    expires_at: Instant,
}

impl<T> CacheEntry<T> {
    fn new(value: T, ttl: Duration) -> Self {
        Self {
            value,
            expires_at: Instant::now() + ttl,
        }
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// Cache for guild settings and filtered words to reduce database queries.
/// Uses LRU eviction with TTL-based expiration.
struct GuildCache {
    settings: Mutex<LruCache<Id<GuildMarker>, CacheEntry<GuildSettings>>>,
    words: Mutex<LruCache<Id<GuildMarker>, CacheEntry<Vec<String>>>>,
    ttl: Duration,
}

impl GuildCache {
    fn new(capacity: usize, ttl_secs: u64) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(100).unwrap());
        Self {
            settings: Mutex::new(LruCache::new(cap)),
            words: Mutex::new(LruCache::new(cap)),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    fn get_settings(&self, guild_id: Id<GuildMarker>) -> Option<GuildSettings> {
        let mut cache = self.settings.lock();
        if let Some(entry) = cache.get(&guild_id) {
            if !entry.is_expired() {
                return Some(entry.value.clone());
            }
            cache.pop(&guild_id);
        }
        None
    }

    fn set_settings(&self, guild_id: Id<GuildMarker>, settings: GuildSettings) {
        let mut cache = self.settings.lock();
        cache.put(guild_id, CacheEntry::new(settings, self.ttl));
    }

    fn get_words(&self, guild_id: Id<GuildMarker>) -> Option<Vec<String>> {
        let mut cache = self.words.lock();
        if let Some(entry) = cache.get(&guild_id) {
            if !entry.is_expired() {
                return Some(entry.value.clone());
            }
            cache.pop(&guild_id);
        }
        None
    }

    fn set_words(&self, guild_id: Id<GuildMarker>, words: Vec<String>) {
        let mut cache = self.words.lock();
        cache.put(guild_id, CacheEntry::new(words, self.ttl));
    }

    /// Invalidate cache for a guild (call after settings update)
    pub fn invalidate(&self, guild_id: Id<GuildMarker>) {
        self.settings.lock().pop(&guild_id);
        self.words.lock().pop(&guild_id);
    }
}

pub struct AutoMod {
    pub spam: SpamDetector,
    pub wordfilter: WordFilter,
    pub raid: RaidDetector,
    pool: SqlitePool,
    http: Arc<Client>,
    cache: GuildCache,
}

impl AutoMod {
    pub fn new(pool: SqlitePool, http: Arc<Client>) -> Self {
        Self {
            spam: SpamDetector::new(),
            wordfilter: WordFilter::new(),
            raid: RaidDetector::new(),
            pool,
            http,
            // Cache up to 100 guilds, 30 second TTL
            cache: GuildCache::new(100, 30),
        }
    }

    /// Get guild settings, using cache when available
    async fn get_cached_settings(&self, guild_id: Id<GuildMarker>) -> Result<GuildSettings> {
        if let Some(settings) = self.cache.get_settings(guild_id) {
            return Ok(settings);
        }

        let settings = get_guild_settings(&self.pool, guild_id).await?;
        self.cache.set_settings(guild_id, settings.clone());
        Ok(settings)
    }

    /// Get filtered words, using cache when available
    async fn get_cached_words(&self, guild_id: Id<GuildMarker>) -> Result<Vec<String>> {
        if let Some(words) = self.cache.get_words(guild_id) {
            return Ok(words);
        }

        let words = get_filtered_words(&self.pool, guild_id).await?;
        self.cache.set_words(guild_id, words.clone());
        Ok(words)
    }

    /// Invalidate cache for a guild after settings change
    pub fn invalidate_cache(&self, guild_id: Id<GuildMarker>) {
        self.cache.invalidate(guild_id);
    }

    pub async fn check_message(&self, message: &Message) -> Result<AutoModAction> {
        let Some(guild_id) = message.guild_id else {
            return Ok(AutoModAction::None);
        };

        let settings = self.get_cached_settings(guild_id).await?;

        // Check word filter first
        let words = self.get_cached_words(guild_id).await?;
        if self.wordfilter.check(&message.content, &words) {
            return Ok(AutoModAction::DeleteMessage {
                reason: "Message contains filtered word".to_string(),
            });
        }

        // Check spam
        if settings.spam_enabled {
            if self.spam.check(
                message.author.id,
                guild_id,
                settings.spam_threshold as u32,
                settings.spam_interval as u64,
            ) {
                return Ok(AutoModAction::TimeoutUser {
                    reason: "Spam detected".to_string(),
                    duration_seconds: 60,
                });
            }
        }

        Ok(AutoModAction::None)
    }

    pub async fn check_member_join(&self, member: &MemberAdd) -> Result<AutoModAction> {
        let settings = self.get_cached_settings(member.guild_id).await?;

        if settings.raid_enabled {
            if self.raid.check(
                member.guild_id,
                settings.raid_threshold as u32,
                settings.raid_interval as u64,
            ) {
                return Ok(AutoModAction::RaidDetected);
            }
        }

        Ok(AutoModAction::None)
    }

    pub async fn execute_action(
        &self,
        action: AutoModAction,
        message: &Message,
    ) -> Result<()> {
        let Some(guild_id) = message.guild_id else {
            return Ok(());
        };

        match action {
            AutoModAction::DeleteMessage { reason } => {
                self.http
                    .delete_message(message.channel_id, message.id)
                    .await?;

                tracing::info!(
                    "Deleted message from {} in guild {}: {}",
                    message.author.id,
                    guild_id,
                    reason
                );
            }
            AutoModAction::TimeoutUser {
                reason,
                duration_seconds,
            } => {
                // Delete the spam message
                let _ = self
                    .http
                    .delete_message(message.channel_id, message.id)
                    .await;

                // Timeout the user
                let timeout_until = timestamp_from_now(duration_seconds);
                if let Ok(timestamp) = Timestamp::parse(&timeout_until) {
                    if let Ok(request) = self.http
                        .update_guild_member(guild_id, message.author.id)
                        .communication_disabled_until(Some(timestamp))
                    {
                        let _ = request.await;
                    }
                }

                tracing::info!(
                    "Timed out {} in guild {} for {} seconds: {}",
                    message.author.id,
                    guild_id,
                    duration_seconds,
                    reason
                );
            }
            AutoModAction::RaidDetected => {
                tracing::warn!("Raid detected in guild {}", guild_id);
            }
            AutoModAction::None => {}
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum AutoModAction {
    None,
    DeleteMessage { reason: String },
    TimeoutUser { reason: String, duration_seconds: u64 },
    RaidDetected,
}

/// Generate ISO 8601 timestamp for a future time.
/// Uses the `time` crate for correct, efficient formatting.
#[inline]
fn timestamp_from_now(seconds: u64) -> String {
    use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

    let future = OffsetDateTime::now_utc() + Duration::seconds(seconds as i64);
    future.format(&Rfc3339).unwrap_or_else(|_| {
        // Fallback format if Rfc3339 somehow fails
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            future.year(),
            future.month() as u8,
            future.day(),
            future.hour(),
            future.minute(),
            future.second()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_format() {
        let ts = timestamp_from_now(0);
        // Should be valid ISO 8601 / RFC 3339 format
        assert!(ts.contains('T'));
        assert!(ts.ends_with('Z') || ts.contains('+') || ts.contains('-'));
        // Should have proper length (at least YYYY-MM-DDTHH:MM:SSZ)
        assert!(ts.len() >= 20);
    }

    #[test]
    fn test_timestamp_future() {
        let now = timestamp_from_now(0);
        let future = timestamp_from_now(3600); // 1 hour later

        // Future timestamp should be lexicographically greater
        assert!(future > now);
    }

    #[test]
    fn test_timestamp_parsing() {
        use twilight_model::util::Timestamp;

        let ts = timestamp_from_now(60);
        // Should be parseable by Twilight's Timestamp
        let result = Timestamp::parse(&ts);
        assert!(result.is_ok(), "Failed to parse timestamp: {}", ts);
    }

    #[test]
    fn test_timestamp_various_durations() {
        use twilight_model::util::Timestamp;

        // Test various timeout durations
        for seconds in [0, 60, 300, 3600, 86400, 604800] {
            let ts = timestamp_from_now(seconds);
            let result = Timestamp::parse(&ts);
            assert!(result.is_ok(), "Failed to parse {} seconds: {}", seconds, ts);
        }
    }

    #[test]
    fn test_cache_entry_expiration() {
        let entry = CacheEntry::new("test".to_string(), Duration::from_millis(10));
        assert!(!entry.is_expired());

        std::thread::sleep(Duration::from_millis(15));
        assert!(entry.is_expired());
    }

    #[test]
    fn test_cache_entry_not_expired() {
        let entry = CacheEntry::new("test".to_string(), Duration::from_secs(60));
        assert!(!entry.is_expired());
    }

    #[test]
    fn test_guild_cache_settings() {
        let cache = GuildCache::new(10, 60);
        let guild_id = Id::new(123);
        let settings = GuildSettings::default();

        assert!(cache.get_settings(guild_id).is_none());

        cache.set_settings(guild_id, settings.clone());
        let cached = cache.get_settings(guild_id);
        assert!(cached.is_some());
    }

    #[test]
    fn test_guild_cache_words() {
        let cache = GuildCache::new(10, 60);
        let guild_id = Id::new(123);
        let words = vec!["test".to_string(), "word".to_string()];

        assert!(cache.get_words(guild_id).is_none());

        cache.set_words(guild_id, words.clone());
        let cached = cache.get_words(guild_id);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap(), words);
    }

    #[test]
    fn test_guild_cache_invalidate() {
        let cache = GuildCache::new(10, 60);
        let guild_id = Id::new(123);

        cache.set_settings(guild_id, GuildSettings::default());
        cache.set_words(guild_id, vec!["test".to_string()]);

        assert!(cache.get_settings(guild_id).is_some());
        assert!(cache.get_words(guild_id).is_some());

        cache.invalidate(guild_id);

        assert!(cache.get_settings(guild_id).is_none());
        assert!(cache.get_words(guild_id).is_none());
    }

    #[test]
    fn test_guild_cache_ttl_expiration() {
        let cache = GuildCache::new(10, 0); // 0 second TTL = immediate expiration
        let guild_id = Id::new(123);

        cache.set_settings(guild_id, GuildSettings::default());
        std::thread::sleep(Duration::from_millis(1));

        // Should be expired
        assert!(cache.get_settings(guild_id).is_none());
    }

    #[test]
    fn test_guild_cache_lru_eviction() {
        let cache = GuildCache::new(2, 60); // Only 2 entries

        let guild1 = Id::new(1);
        let guild2 = Id::new(2);
        let guild3 = Id::new(3);

        cache.set_settings(guild1, GuildSettings::default());
        cache.set_settings(guild2, GuildSettings::default());
        cache.set_settings(guild3, GuildSettings::default());

        // guild1 should be evicted (LRU)
        assert!(cache.get_settings(guild1).is_none());
        assert!(cache.get_settings(guild2).is_some());
        assert!(cache.get_settings(guild3).is_some());
    }
}
