//! Tiny in-memory voice state cache.
//!
//! twilight-http 0.15 doesn't expose a `GET /guilds/{id}/voice-states/{user}`
//! helper, so to know which voice channel a `!voice join` invoker is sitting
//! in we track VoiceStateUpdate events from the gateway. This is exactly what
//! discord clients do — voice state is push-only.

use std::collections::HashMap;

use parking_lot::RwLock;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, UserMarker},
    Id,
};

/// Key = (guild, user). Value = channel they're currently in. Removed when
/// the user disconnects (channel_id = None).
#[derive(Default)]
pub struct VoiceStateCache {
    inner: RwLock<HashMap<(Id<GuildMarker>, Id<UserMarker>), Id<ChannelMarker>>>,
}

impl VoiceStateCache {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Apply a VoiceStateUpdate. `channel_id = None` means the user left voice
    /// entirely; we drop the entry so a subsequent `!voice join` correctly
    /// fails with "you need to be in a voice channel first".
    pub fn apply_update(&self, voice_state: &twilight_model::voice::VoiceState) {
        let Some(guild_id) = voice_state.guild_id else {
            return;
        };
        let user_id = voice_state.user_id;
        let mut map = self.inner.write();
        match voice_state.channel_id {
            Some(channel_id) => {
                map.insert((guild_id, user_id), channel_id);
            }
            None => {
                map.remove(&(guild_id, user_id));
            }
        }
    }

    /// Lookup the current voice channel for a user in a guild.
    pub fn channel_for(
        &self,
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
    ) -> Option<Id<ChannelMarker>> {
        self.inner.read().get(&(guild_id, user_id)).copied()
    }
}
