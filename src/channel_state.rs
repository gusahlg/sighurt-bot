//! Shared per-channel runtime state.
//!
//! Two jobs, one struct:
//! 1. Bot-chain loop guard — counts consecutive chat replies we've sent that
//!    were triggered by ANOTHER bot's message; any human message in the
//!    channel resets the counter. This is what stops two chatty bots from
//!    ping-ponging @-mentions at each other forever.
//! 2. Recent-authors map — author id -> display name for the last ~50 authors
//!    per channel, recorded on EVERY MessageCreate before any filtering. Used
//!    to humanize `<@id>` mentions we can't resolve from the message payload
//!    and to turn the LLM's `@name` output back into real pings.
//!
//! One process-wide instance lives in an Arc created in main. A single
//! parking_lot mutex over the whole map is plenty — MessageCreate volume is
//! tiny and the critical sections are a few Vec/HashMap ops.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, UserMarker};

/// Cap on remembered authors per channel. Eviction is insertion-order (oldest
/// first-seen author drops out), which is plenty at this size.
const RECENT_AUTHORS_CAP: usize = 50;

#[derive(Default)]
struct ChannelEntry {
    /// Consecutive chat replies sent in this channel where the trigger was
    /// another bot's message. Reset by any human message.
    consecutive_bot_replies: u32,
    /// author id -> display name, insertion-ordered for cheap eviction.
    recent_authors: Vec<(Id<UserMarker>, String)>,
    /// A chat LLM round-trip is currently in flight for this channel. Blocks
    /// concurrent triggers (DM-flood DoS guard) until it clears.
    chat_in_flight: bool,
    /// When the last chat trigger was admitted for this channel. Used to
    /// enforce a minimum spacing between replies.
    last_chat_trigger: Option<Instant>,
    /// Human messages seen since the bot last spoke in this channel. Drives
    /// unprompted replies ("jump in every ~N messages").
    messages_since_bot_reply: u32,
    /// The jittered message count at which the next unprompted reply fires.
    /// 0 = not yet drawn for the current cycle.
    unprompted_target: u32,
    /// When the bot last added a reaction in this channel (spam brake).
    last_reaction: Option<Instant>,
}

#[derive(Default)]
pub struct ChannelState {
    channels: Mutex<HashMap<Id<ChannelMarker>, ChannelEntry>>,
}

impl ChannelState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message's author. Called on EVERY MessageCreate before any
    /// filtering so the recent-authors map sees bots and webhooks too. A
    /// human author also resets the channel's bot-chain counter.
    pub fn record_message(
        &self,
        channel_id: Id<ChannelMarker>,
        author_id: Id<UserMarker>,
        display_name: &str,
        author_is_bot: bool,
    ) {
        let mut channels = self.channels.lock();
        let entry = channels.entry(channel_id).or_default();
        if !author_is_bot {
            entry.consecutive_bot_replies = 0;
            entry.messages_since_bot_reply = entry.messages_since_bot_reply.saturating_add(1);
        }
        if let Some(pos) = entry
            .recent_authors
            .iter()
            .position(|(id, _)| *id == author_id)
        {
            // Known author: refresh the name in place (nick changes).
            entry.recent_authors[pos].1 = display_name.to_string();
        } else {
            if entry.recent_authors.len() >= RECENT_AUTHORS_CAP {
                entry.recent_authors.remove(0);
            }
            entry
                .recent_authors
                .push((author_id, display_name.to_string()));
        }
    }

    /// Atomically reserve a bot-chain slot: if the current consecutive
    /// bot-reply count is below `max_chain`, increment it and return `true`
    /// (the caller owns one slot and may proceed with the LLM round-trip);
    /// otherwise return `false` and change nothing.
    ///
    /// This is a compare-and-increment under the same lock so that N
    /// concurrently-spawned MessageCreate tasks can't all observe
    /// `count < max` and each fire a reply — only `max_chain` reservations
    /// succeed between human messages. On a failed send (or empty reply /
    /// early return after reserving) the caller MUST call
    /// [`release_bot_chain_slot`](Self::release_bot_chain_slot) so a failed
    /// attempt doesn't permanently consume the budget.
    pub fn try_reserve_bot_chain(&self, channel_id: Id<ChannelMarker>, max_chain: u32) -> bool {
        let mut channels = self.channels.lock();
        let entry = channels.entry(channel_id).or_default();
        if entry.consecutive_bot_replies < max_chain {
            entry.consecutive_bot_replies += 1;
            true
        } else {
            false
        }
    }

    /// Release a previously reserved bot-chain slot (decrement, saturating at
    /// 0). Call this when a reservation from
    /// [`try_reserve_bot_chain`](Self::try_reserve_bot_chain) did NOT result in
    /// a delivered reply (send failed, empty reply, early return), so the
    /// budget reflects only replies actually sent.
    pub fn release_bot_chain_slot(&self, channel_id: Id<ChannelMarker>) {
        if let Some(entry) = self.channels.lock().get_mut(&channel_id) {
            entry.consecutive_bot_replies = entry.consecutive_bot_replies.saturating_sub(1);
        }
    }

    /// Try to admit a chat trigger for this channel. Denies (returns `false`)
    /// when a round-trip is already in flight for the channel OR when the last
    /// admitted trigger was less than `min_gap` ago. On success marks the
    /// channel in-flight and stamps the trigger time; the caller MUST later
    /// call [`end_chat`](Self::end_chat) to clear the in-flight flag.
    ///
    /// This is the DM-flood / concurrency guard on the CHAT TRIGGER itself
    /// (guild automod doesn't cover DMs): a scripted flood can't stack up N
    /// simultaneous LLM calls against the single-mutex server.
    pub fn try_begin_chat(&self, channel_id: Id<ChannelMarker>, min_gap: Duration) -> bool {
        let now = Instant::now();
        let mut channels = self.channels.lock();
        let entry = channels.entry(channel_id).or_default();
        if entry.chat_in_flight {
            return false;
        }
        if let Some(last) = entry.last_chat_trigger {
            if now.duration_since(last) < min_gap {
                return false;
            }
        }
        entry.chat_in_flight = true;
        entry.last_chat_trigger = Some(now);
        true
    }

    /// Clear the in-flight flag set by [`try_begin_chat`](Self::try_begin_chat).
    /// Idempotent and safe to call even if the entry was evicted.
    pub fn end_chat(&self, channel_id: Id<ChannelMarker>) {
        if let Some(entry) = self.channels.lock().get_mut(&channel_id) {
            entry.chat_in_flight = false;
        }
    }

    /// Atomically claim an unprompted-reply trigger. Fires once the channel
    /// has seen a jittered target of `base ± base/4` human messages since the
    /// bot last spoke, then resets the counter and draws a fresh target so
    /// the cadence stays organic instead of metronomic. `entropy` is any
    /// varying value (message snowflake timestamp bits work well) — this only
    /// needs jitter, not cryptographic randomness.
    pub fn try_claim_unprompted(
        &self,
        channel_id: Id<ChannelMarker>,
        base: u32,
        entropy: u64,
    ) -> bool {
        if base == 0 {
            return false;
        }
        let mut channels = self.channels.lock();
        let entry = channels.entry(channel_id).or_default();
        if entry.unprompted_target == 0 {
            let spread = (base / 2).max(1);
            entry.unprompted_target = base - base / 4 + (entropy % spread as u64) as u32;
        }
        if entry.messages_since_bot_reply >= entry.unprompted_target {
            entry.messages_since_bot_reply = 0;
            entry.unprompted_target = 0;
            true
        } else {
            false
        }
    }

    /// Note that the bot delivered a chat reply in this channel: the
    /// unprompted counter starts over (any bot message counts as the bot
    /// having spoken, prompted or not).
    pub fn note_bot_reply(&self, channel_id: Id<ChannelMarker>) {
        let mut channels = self.channels.lock();
        let entry = channels.entry(channel_id).or_default();
        entry.messages_since_bot_reply = 0;
        entry.unprompted_target = 0;
    }

    /// Claim a reaction opportunity if the per-channel cooldown has elapsed.
    /// Stamps the cooldown immediately so concurrent messages can't both fire.
    pub fn try_claim_reaction(&self, channel_id: Id<ChannelMarker>, min_gap: Duration) -> bool {
        let now = Instant::now();
        let mut channels = self.channels.lock();
        let entry = channels.entry(channel_id).or_default();
        if let Some(last) = entry.last_reaction {
            if now.duration_since(last) < min_gap {
                return false;
            }
        }
        entry.last_reaction = Some(now);
        true
    }

    /// Look up a recent author's display name by id.
    pub fn display_name(
        &self,
        channel_id: Id<ChannelMarker>,
        user_id: Id<UserMarker>,
    ) -> Option<String> {
        self.channels.lock().get(&channel_id).and_then(|e| {
            e.recent_authors
                .iter()
                .find(|(id, _)| *id == user_id)
                .map(|(_, name)| name.clone())
        })
    }

    /// Reverse lookup: case-insensitive display name -> id. Newest entry wins
    /// when two authors share a name.
    pub fn find_by_name(&self, channel_id: Id<ChannelMarker>, name: &str) -> Option<Id<UserMarker>> {
        let needle = name.to_lowercase();
        let channels = self.channels.lock();
        channels.get(&channel_id).and_then(|e| {
            e.recent_authors
                .iter()
                .rev()
                .find(|(_, n)| n.to_lowercase() == needle)
                .map(|(id, _)| *id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH: Id<ChannelMarker> = Id::new(100);

    #[test]
    fn bot_chain_blocks_after_limit_and_human_resets() {
        let state = ChannelState::new();
        // Fresh channel: three reservations succeed, the fourth is denied.
        assert!(state.try_reserve_bot_chain(CH, 3));
        assert!(state.try_reserve_bot_chain(CH, 3));
        assert!(state.try_reserve_bot_chain(CH, 3));
        // Counter hit the limit: blocked.
        assert!(!state.try_reserve_bot_chain(CH, 3));
        // A bot message does NOT reset the counter...
        state.record_message(CH, Id::new(1), "OtherBot", true);
        assert!(!state.try_reserve_bot_chain(CH, 3));
        // ...but a human message does.
        state.record_message(CH, Id::new(2), "fredrik", false);
        assert!(state.try_reserve_bot_chain(CH, 3));
    }

    #[test]
    fn bot_chain_is_per_channel() {
        let state = ChannelState::new();
        assert!(state.try_reserve_bot_chain(CH, 1));
        assert!(!state.try_reserve_bot_chain(CH, 1));
        assert!(state.try_reserve_bot_chain(Id::new(200), 1));
    }

    #[test]
    fn bot_chain_reserve_is_atomic_across_max() {
        // Simulate N concurrent triggers arriving within one round-trip: only
        // `max` reservations may succeed before any reply is confirmed.
        let state = ChannelState::new();
        let max = 3;
        let mut granted = 0;
        for _ in 0..10 {
            if state.try_reserve_bot_chain(CH, max) {
                granted += 1;
            }
        }
        assert_eq!(granted, max);
    }

    #[test]
    fn bot_chain_release_returns_slot() {
        let state = ChannelState::new();
        assert!(state.try_reserve_bot_chain(CH, 1));
        // Budget exhausted...
        assert!(!state.try_reserve_bot_chain(CH, 1));
        // ...but releasing the failed attempt frees it again.
        state.release_bot_chain_slot(CH);
        assert!(state.try_reserve_bot_chain(CH, 1));
    }

    #[test]
    fn bot_chain_release_saturates_at_zero() {
        let state = ChannelState::new();
        // Releasing with no reservation must not underflow.
        state.release_bot_chain_slot(CH);
        assert!(state.try_reserve_bot_chain(CH, 1));
    }

    #[test]
    fn chat_gate_blocks_concurrent_in_flight() {
        let state = ChannelState::new();
        // First trigger admitted; a zero cooldown isolates the in-flight test.
        assert!(state.try_begin_chat(CH, Duration::ZERO));
        // Second trigger while the first is in flight is denied.
        assert!(!state.try_begin_chat(CH, Duration::ZERO));
        // After the round-trip ends, a new trigger is admitted again.
        state.end_chat(CH);
        assert!(state.try_begin_chat(CH, Duration::ZERO));
    }

    #[test]
    fn chat_gate_enforces_cooldown() {
        let state = ChannelState::new();
        let gap = Duration::from_secs(3600);
        assert!(state.try_begin_chat(CH, gap));
        state.end_chat(CH);
        // Within the cooldown window: denied even though nothing is in flight.
        assert!(!state.try_begin_chat(CH, gap));
        // Zero cooldown always admits once the in-flight flag is clear.
        assert!(state.try_begin_chat(CH, Duration::ZERO));
    }

    #[test]
    fn chat_gate_is_per_channel() {
        let state = ChannelState::new();
        assert!(state.try_begin_chat(CH, Duration::ZERO));
        // A different channel is independent.
        assert!(state.try_begin_chat(Id::new(200), Duration::ZERO));
    }

    #[test]
    fn recent_authors_lookup_both_ways() {
        let state = ChannelState::new();
        state.record_message(CH, Id::new(5), "Gustav", false);
        assert_eq!(state.display_name(CH, Id::new(5)).as_deref(), Some("Gustav"));
        assert_eq!(state.find_by_name(CH, "gustav"), Some(Id::new(5)));
        assert_eq!(state.find_by_name(CH, "nobody"), None);
        // Name refresh in place (e.g. nick change).
        state.record_message(CH, Id::new(5), "Gurra", false);
        assert_eq!(state.display_name(CH, Id::new(5)).as_deref(), Some("Gurra"));
        assert_eq!(state.find_by_name(CH, "GURRA"), Some(Id::new(5)));
    }

    #[test]
    fn unprompted_fires_in_jitter_window_and_resets() {
        let state = ChannelState::new();
        let base = 30;
        // Never before base - base/4 human messages, always by base + base/4.
        let mut fired_at = None;
        for n in 1..=45u32 {
            state.record_message(CH, Id::new(2), "human", false);
            if state.try_claim_unprompted(CH, base, 7) {
                fired_at = Some(n);
                break;
            }
        }
        let n = fired_at.expect("unprompted reply never fired");
        assert!((23..=38).contains(&n), "fired at {n}, outside jitter window");
        // Counter reset: the very next message cannot fire again.
        state.record_message(CH, Id::new(2), "human", false);
        assert!(!state.try_claim_unprompted(CH, base, 7));
    }

    #[test]
    fn unprompted_disabled_with_zero_base_and_reset_by_bot_reply() {
        let state = ChannelState::new();
        for _ in 0..100 {
            state.record_message(CH, Id::new(2), "human", false);
        }
        assert!(!state.try_claim_unprompted(CH, 0, 1));
        // A delivered bot reply resets the accumulated count.
        state.note_bot_reply(CH);
        state.record_message(CH, Id::new(2), "human", false);
        assert!(!state.try_claim_unprompted(CH, 30, 1));
    }

    #[test]
    fn reaction_claim_respects_cooldown() {
        let state = ChannelState::new();
        assert!(state.try_claim_reaction(CH, Duration::from_secs(3600)));
        assert!(!state.try_claim_reaction(CH, Duration::from_secs(3600)));
        // Independent channel, independent cooldown.
        assert!(state.try_claim_reaction(Id::new(200), Duration::from_secs(3600)));
        // Zero gap always admits.
        assert!(state.try_claim_reaction(CH, Duration::ZERO));
    }

    #[test]
    fn recent_authors_evicts_oldest_at_cap() {
        let state = ChannelState::new();
        for i in 1..=(RECENT_AUTHORS_CAP as u64 + 1) {
            state.record_message(CH, Id::new(i), &format!("user{}", i), false);
        }
        // First inserted author fell out; latest is present.
        assert_eq!(state.display_name(CH, Id::new(1)), None);
        assert!(state.display_name(CH, Id::new(RECENT_AUTHORS_CAP as u64 + 1)).is_some());
    }
}
