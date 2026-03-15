use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use twilight_model::id::{marker::GuildMarker, Id};

/// Raid detection using sliding window algorithm.
/// Tracks member joins per guild and triggers raid mode when threshold exceeded.
pub struct RaidDetector {
    /// Map of guild_id -> list of join timestamps
    joins: Mutex<HashMap<Id<GuildMarker>, Vec<Instant>>>,
    /// Guilds currently in raid mode with activation time
    raid_mode: Mutex<HashMap<Id<GuildMarker>, Instant>>,
}

/// Duration of raid mode once activated (10 minutes)
const RAID_MODE_DURATION: Duration = Duration::from_secs(600);
/// Maximum age of join records to keep (5 minutes)
const MAX_JOIN_AGE: Duration = Duration::from_secs(300);
/// Maximum timestamps to keep per guild to prevent unbounded memory growth
const MAX_TIMESTAMPS_PER_GUILD: usize = 200;

impl RaidDetector {
    pub fn new() -> Self {
        Self {
            joins: Mutex::new(HashMap::new()),
            raid_mode: Mutex::new(HashMap::new()),
        }
    }

    /// Check if a member join triggers raid detection.
    /// Returns true if threshold exceeded (raid detected).
    #[inline]
    pub fn check(
        &self,
        guild_id: Id<GuildMarker>,
        threshold: u32,
        interval_secs: u64,
    ) -> bool {
        let is_raid;
        {
            let mut joins = self.joins.lock();
            let now = Instant::now();

            // Use checked_sub to avoid panic if interval_secs is very large or system just booted
            let cutoff = now.checked_sub(Duration::from_secs(interval_secs));

            let timestamps = joins.entry(guild_id).or_insert_with(Vec::new);

            // Remove old timestamps
            if let Some(cutoff) = cutoff {
                timestamps.retain(|&t| t > cutoff);
            } else {
                timestamps.clear();
            }

            // Add current join
            timestamps.push(now);

            // Cap unbounded growth
            if timestamps.len() > MAX_TIMESTAMPS_PER_GUILD {
                let excess = timestamps.len() - MAX_TIMESTAMPS_PER_GUILD;
                timestamps.drain(..excess);
            }

            // Check if threshold exceeded
            is_raid = timestamps.len() > threshold as usize;
        }
        // joins lock is dropped here before acquiring raid_mode lock (avoids potential deadlock)

        if is_raid {
            let mut raid_mode = self.raid_mode.lock();
            raid_mode.insert(guild_id, Instant::now());
        }

        is_raid
    }

    /// Check if a guild is currently in raid mode.
    pub fn is_raid_mode(&self, guild_id: Id<GuildMarker>) -> bool {
        let mut raid_mode = self.raid_mode.lock();

        if let Some(&started) = raid_mode.get(&guild_id) {
            if let Some(cutoff) = Instant::now().checked_sub(RAID_MODE_DURATION) {
                if started > cutoff {
                    return true;
                }
            }
            // Expired - remove stale entry
            raid_mode.remove(&guild_id);
            false
        } else {
            false
        }
    }

    /// Manually disable raid mode for a guild.
    pub fn disable_raid_mode(&self, guild_id: Id<GuildMarker>) {
        let mut raid_mode = self.raid_mode.lock();
        raid_mode.remove(&guild_id);
    }

    /// Clean up old entries to prevent memory growth.
    pub fn cleanup(&self) {
        let now = Instant::now();

        // Clean old join records
        {
            let mut joins = self.joins.lock();
            joins.retain(|_, timestamps| {
                if let Some(cutoff) = now.checked_sub(MAX_JOIN_AGE) {
                    timestamps.retain(|&t| t > cutoff);
                } else {
                    timestamps.clear();
                }
                !timestamps.is_empty()
            });

            // Shrink if capacity is much larger than length
            let len = joins.len();
            if joins.capacity() > len * 4 + 16 {
                joins.shrink_to(len * 2);
            }
        }

        // Clean expired raid modes
        {
            let mut raid_mode = self.raid_mode.lock();
            raid_mode.retain(|_, &mut started| {
                now.checked_sub(RAID_MODE_DURATION)
                    .map_or(true, |cutoff| started > cutoff)
            });

            let len = raid_mode.len();
            if raid_mode.capacity() > len * 4 + 16 {
                raid_mode.shrink_to(len * 2);
            }
        }
    }

    /// Get total number of tracked guilds (for testing)
    #[cfg(test)]
    fn get_tracked_guilds(&self) -> usize {
        self.joins.lock().len()
    }

    /// Get join count for a guild (for testing)
    #[cfg(test)]
    fn get_join_count(&self, guild_id: Id<GuildMarker>) -> usize {
        self.joins.lock().get(&guild_id).map(|v| v.len()).unwrap_or(0)
    }

    /// Get number of guilds in raid mode (for testing)
    #[cfg(test)]
    fn get_raid_mode_count(&self) -> usize {
        self.raid_mode.lock().len()
    }
}

impl Default for RaidDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn make_guild_id(id: u64) -> Id<GuildMarker> {
        Id::new(id)
    }

    #[test]
    fn test_new_detector() {
        let detector = RaidDetector::new();
        assert_eq!(detector.get_tracked_guilds(), 0);
        assert_eq!(detector.get_raid_mode_count(), 0);
    }

    #[test]
    fn test_single_join_no_raid() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // Single join should not trigger raid (threshold 10)
        assert!(!detector.check(guild, 10, 10));
        assert!(!detector.is_raid_mode(guild));
        assert_eq!(detector.get_join_count(guild), 1);
    }

    #[test]
    fn test_below_threshold() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // 10 joins with threshold 10 should NOT trigger (need > 10)
        for _ in 0..10 {
            assert!(!detector.check(guild, 10, 60));
        }
        assert!(!detector.is_raid_mode(guild));
        assert_eq!(detector.get_join_count(guild), 10);
    }

    #[test]
    fn test_at_threshold_triggers() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // 11 joins with threshold 10 should trigger on the 11th
        for i in 0..11 {
            let is_raid = detector.check(guild, 10, 60);
            if i < 10 {
                assert!(!is_raid, "Join {} should not trigger raid", i);
            } else {
                assert!(is_raid, "Join {} should trigger raid", i);
            }
        }
        assert!(detector.is_raid_mode(guild));
    }

    #[test]
    fn test_different_guilds_isolated() {
        let detector = RaidDetector::new();
        let guild1 = make_guild_id(111);
        let guild2 = make_guild_id(222);

        // Trigger raid in guild1
        for _ in 0..15 {
            detector.check(guild1, 10, 60);
        }

        assert!(detector.is_raid_mode(guild1));
        assert!(!detector.is_raid_mode(guild2));

        // Guild2 should start fresh
        assert!(!detector.check(guild2, 10, 60));
        assert_eq!(detector.get_join_count(guild2), 1);
    }

    #[test]
    fn test_disable_raid_mode() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // Trigger raid mode
        for _ in 0..15 {
            detector.check(guild, 10, 60);
        }
        assert!(detector.is_raid_mode(guild));

        // Disable it
        detector.disable_raid_mode(guild);
        assert!(!detector.is_raid_mode(guild));
    }

    #[test]
    fn test_disable_raid_mode_does_not_affect_others() {
        let detector = RaidDetector::new();
        let guild1 = make_guild_id(111);
        let guild2 = make_guild_id(222);

        // Trigger raid in both guilds
        for _ in 0..15 {
            detector.check(guild1, 10, 60);
            detector.check(guild2, 10, 60);
        }

        assert!(detector.is_raid_mode(guild1));
        assert!(detector.is_raid_mode(guild2));

        // Disable only guild1
        detector.disable_raid_mode(guild1);

        assert!(!detector.is_raid_mode(guild1));
        assert!(detector.is_raid_mode(guild2));
    }

    #[test]
    fn test_cleanup_keeps_recent_entries() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        detector.check(guild, 10, 60);
        assert_eq!(detector.get_tracked_guilds(), 1);

        detector.cleanup();
        assert_eq!(detector.get_tracked_guilds(), 1);
    }

    #[test]
    fn test_cleanup_empty() {
        let detector = RaidDetector::new();
        // Should not panic on empty cleanup
        detector.cleanup();
        assert_eq!(detector.get_tracked_guilds(), 0);
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;

        let detector = Arc::new(RaidDetector::new());
        let mut handles = vec![];

        // Spawn multiple threads checking different guilds
        // Use thread_id + 1 to avoid zero (Id::new panics on 0)
        for thread_id in 1..=4 {
            let detector = Arc::clone(&detector);
            let handle = thread::spawn(move || {
                let guild = make_guild_id(thread_id as u64);
                for _ in 0..50 {
                    detector.check(guild, 100, 60);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().expect("Thread panicked");
        }

        assert_eq!(detector.get_tracked_guilds(), 4);
    }

    #[test]
    fn test_default_trait() {
        let detector = RaidDetector::default();
        assert_eq!(detector.get_tracked_guilds(), 0);
    }

    #[test]
    fn test_zero_threshold() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // Threshold 0 means any join triggers raid
        assert!(detector.check(guild, 0, 60));
        assert!(detector.is_raid_mode(guild));
    }

    #[test]
    fn test_raid_mode_persists() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // Trigger raid
        for _ in 0..15 {
            detector.check(guild, 10, 60);
        }

        // Raid mode should persist across multiple is_raid_mode checks
        assert!(detector.is_raid_mode(guild));
        assert!(detector.is_raid_mode(guild));
        assert!(detector.is_raid_mode(guild));
    }

    #[test]
    fn test_cleanup_preserves_raid_mode() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // Trigger raid
        for _ in 0..15 {
            detector.check(guild, 10, 60);
        }
        assert!(detector.is_raid_mode(guild));

        // Cleanup should preserve recent raid mode
        detector.cleanup();
        assert!(detector.is_raid_mode(guild));
    }

    #[test]
    fn test_rapid_joins() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // Simulate rapid joins (very common in raids)
        for _ in 0..100 {
            detector.check(guild, 10, 10);
        }

        assert!(detector.is_raid_mode(guild));
        // Capped at MAX_TIMESTAMPS_PER_GUILD
        assert!(detector.get_join_count(guild) <= MAX_TIMESTAMPS_PER_GUILD);
    }

    #[test]
    fn test_interval_filtering() {
        let detector = RaidDetector::new();
        let guild = make_guild_id(123);

        // With interval of 0, all previous joins should expire immediately
        detector.check(guild, 10, 0);
        thread::sleep(Duration::from_millis(1));

        // Previous join should have expired
        let result = detector.check(guild, 10, 0);
        assert!(!result); // Only 1 join remains, below threshold
    }
}
