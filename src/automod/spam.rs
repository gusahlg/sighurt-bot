use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

/// Maximum timestamps to keep per user to prevent unbounded memory growth
const MAX_TIMESTAMPS_PER_USER: usize = 100;

/// Fast spam detection using sliding window algorithm.
/// Uses parking_lot::Mutex for 2-3x faster locking than std::sync::Mutex.
pub struct SpamDetector {
    /// Map of (guild_id, user_id) -> list of message timestamps
    messages: Mutex<HashMap<(Id<GuildMarker>, Id<UserMarker>), Vec<Instant>>>,
}

impl SpamDetector {
    pub fn new() -> Self {
        Self {
            messages: Mutex::new(HashMap::new()),
        }
    }

    /// Check if user is spamming. Returns true if threshold exceeded.
    /// Records the current message timestamp and removes expired ones.
    #[inline]
    pub fn check(
        &self,
        user_id: Id<UserMarker>,
        guild_id: Id<GuildMarker>,
        threshold: u32,
        interval_secs: u64,
    ) -> bool {
        let mut messages = self.messages.lock();
        let key = (guild_id, user_id);
        let now = Instant::now();

        // Use checked_sub to avoid panic if interval_secs is very large or system just booted
        let cutoff = now.checked_sub(Duration::from_secs(interval_secs));

        let timestamps = messages.entry(key).or_insert_with(Vec::new);

        // Remove old timestamps
        if let Some(cutoff) = cutoff {
            timestamps.retain(|&t| t > cutoff);
        } else {
            // If we can't compute a cutoff, all timestamps are considered expired
            timestamps.clear();
        }

        // Add current message
        timestamps.push(now);

        // Cap unbounded growth
        if timestamps.len() > MAX_TIMESTAMPS_PER_USER {
            let excess = timestamps.len() - MAX_TIMESTAMPS_PER_USER;
            timestamps.drain(..excess);
        }

        // Check if threshold exceeded
        timestamps.len() > threshold as usize
    }

    /// Clear spam tracking for a specific user in a guild.
    /// Useful after timeout or manual reset.
    pub fn clear_user(&self, user_id: Id<UserMarker>, guild_id: Id<GuildMarker>) {
        let mut messages = self.messages.lock();
        messages.remove(&(guild_id, user_id));
    }

    /// Clean up old entries to prevent memory growth.
    /// Should be called periodically (e.g., every 60 seconds).
    pub fn cleanup(&self) {
        let mut messages = self.messages.lock();
        let now = Instant::now();
        let max_age = Duration::from_secs(300); // 5 minutes

        messages.retain(|_, timestamps| {
            if let Some(cutoff) = now.checked_sub(max_age) {
                timestamps.retain(|&t| t > cutoff);
            } else {
                timestamps.clear();
            }
            !timestamps.is_empty()
        });

        // Shrink if capacity is much larger than length
        let len = messages.len();
        if messages.capacity() > len * 4 + 16 {
            messages.shrink_to(len * 2);
        }
    }

    /// Get the current message count for a user (for testing/debugging).
    #[cfg(test)]
    fn get_message_count(&self, user_id: Id<UserMarker>, guild_id: Id<GuildMarker>) -> usize {
        let messages = self.messages.lock();
        messages
            .get(&(guild_id, user_id))
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Get total number of tracked user sessions (for testing/debugging).
    #[cfg(test)]
    fn get_tracked_users(&self) -> usize {
        self.messages.lock().len()
    }
}

impl Default for SpamDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn make_user_id(id: u64) -> Id<UserMarker> {
        Id::new(id)
    }

    fn make_guild_id(id: u64) -> Id<GuildMarker> {
        Id::new(id)
    }

    #[test]
    fn test_new_detector() {
        let detector = SpamDetector::new();
        assert_eq!(detector.get_tracked_users(), 0);
    }

    #[test]
    fn test_single_message_no_spam() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        // Single message should not trigger spam (threshold 5)
        assert!(!detector.check(user, guild, 5, 10));
        assert_eq!(detector.get_message_count(user, guild), 1);
    }

    #[test]
    fn test_below_threshold() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        // 5 messages with threshold 5 should NOT trigger (need > 5)
        for _ in 0..5 {
            assert!(!detector.check(user, guild, 5, 10));
        }
        assert_eq!(detector.get_message_count(user, guild), 5);
    }

    #[test]
    fn test_at_threshold_triggers() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        // 6 messages with threshold 5 should trigger on the 6th
        for i in 0..6 {
            let is_spam = detector.check(user, guild, 5, 10);
            if i < 5 {
                assert!(!is_spam, "Message {} should not trigger spam", i);
            } else {
                assert!(is_spam, "Message {} should trigger spam", i);
            }
        }
    }

    #[test]
    fn test_different_users_isolated() {
        let detector = SpamDetector::new();
        let user1 = make_user_id(111);
        let user2 = make_user_id(222);
        let guild = make_guild_id(456);

        // User 1 sends many messages
        for _ in 0..10 {
            detector.check(user1, guild, 5, 10);
        }

        // User 2 should start fresh
        assert!(!detector.check(user2, guild, 5, 10));
        assert_eq!(detector.get_message_count(user1, guild), 10);
        assert_eq!(detector.get_message_count(user2, guild), 1);
    }

    #[test]
    fn test_different_guilds_isolated() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild1 = make_guild_id(111);
        let guild2 = make_guild_id(222);

        // User spams in guild1
        for _ in 0..10 {
            detector.check(user, guild1, 5, 10);
        }

        // Same user in guild2 should start fresh
        assert!(!detector.check(user, guild2, 5, 10));
        assert_eq!(detector.get_message_count(user, guild1), 10);
        assert_eq!(detector.get_message_count(user, guild2), 1);
    }

    #[test]
    fn test_clear_user() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        // Send some messages
        for _ in 0..5 {
            detector.check(user, guild, 5, 10);
        }
        assert_eq!(detector.get_message_count(user, guild), 5);

        // Clear user
        detector.clear_user(user, guild);
        assert_eq!(detector.get_message_count(user, guild), 0);
    }

    #[test]
    fn test_clear_user_does_not_affect_others() {
        let detector = SpamDetector::new();
        let user1 = make_user_id(111);
        let user2 = make_user_id(222);
        let guild = make_guild_id(456);

        for _ in 0..5 {
            detector.check(user1, guild, 5, 10);
            detector.check(user2, guild, 5, 10);
        }

        detector.clear_user(user1, guild);

        assert_eq!(detector.get_message_count(user1, guild), 0);
        assert_eq!(detector.get_message_count(user2, guild), 5);
    }

    #[test]
    fn test_cleanup_removes_old_entries() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        detector.check(user, guild, 5, 10);
        assert_eq!(detector.get_tracked_users(), 1);

        // Cleanup should keep recent entries
        detector.cleanup();
        assert_eq!(detector.get_tracked_users(), 1);
    }

    #[test]
    fn test_cleanup_empty() {
        let detector = SpamDetector::new();
        // Should not panic on empty cleanup
        detector.cleanup();
        assert_eq!(detector.get_tracked_users(), 0);
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;

        let detector = Arc::new(SpamDetector::new());
        let mut handles = vec![];

        // Spawn multiple threads accessing the detector
        // Use thread_id + 1 to avoid zero (Id::new panics on 0)
        for thread_id in 1..=4 {
            let detector = Arc::clone(&detector);
            let handle = thread::spawn(move || {
                let user = make_user_id(thread_id as u64);
                let guild = make_guild_id(1);

                for _ in 0..100 {
                    detector.check(user, guild, 50, 60);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().expect("Thread panicked");
        }

        // Should have 4 tracked users
        assert_eq!(detector.get_tracked_users(), 4);
    }

    #[test]
    fn test_default_trait() {
        let detector = SpamDetector::default();
        assert_eq!(detector.get_tracked_users(), 0);
    }

    #[test]
    fn test_zero_threshold() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        // Threshold 0 means any message triggers spam
        assert!(detector.check(user, guild, 0, 10));
    }

    #[test]
    fn test_high_threshold() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        // Very high threshold - 100 messages capped by MAX_TIMESTAMPS_PER_USER
        for _ in 0..100 {
            let result = detector.check(user, guild, 10000, 10);
            assert!(!result);
        }
        // Should be capped at MAX_TIMESTAMPS_PER_USER
        assert!(detector.get_message_count(user, guild) <= MAX_TIMESTAMPS_PER_USER);
    }

    #[test]
    fn test_message_expiration() {
        let detector = SpamDetector::new();
        let user = make_user_id(123);
        let guild = make_guild_id(456);

        // With interval of 0, all previous messages should expire
        detector.check(user, guild, 5, 0);
        // Small sleep to ensure timestamp difference
        thread::sleep(Duration::from_millis(1));

        // Previous message should have expired, this should be message #1
        detector.check(user, guild, 5, 0);
    }
}
