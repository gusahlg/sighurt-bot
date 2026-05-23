//! Voice mode: bridges Discord voice channels to an ElevenLabs Conversational
//! AI agent. `init` builds the Songbird-backed bridge; `commands` wires
//! `!voice join` / `!voice leave` text commands.

pub mod bridge;
pub mod commands;
pub mod elevenlabs;
pub mod resample;
pub mod voice_state;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use songbird::shards::TwilightMap;
use songbird::Songbird;
use twilight_gateway::Shard;
use twilight_model::id::{marker::UserMarker, Id};

pub use bridge::VoiceBridge;
pub use voice_state::VoiceStateCache;

pub struct VoiceConfig {
    pub agent_id: String,
    pub api_key: Option<String>,
}

impl VoiceConfig {
    /// `ELEVENLABS_AGENT_ID` is required; `ELEVENLABS_API_KEY` is optional and
    /// only needed for private agents. Missing agent id = voice disabled (not
    /// an error — the bot still runs without voice).
    pub fn from_env() -> Option<Self> {
        let agent_id = std::env::var("ELEVENLABS_AGENT_ID").ok().filter(|s| !s.is_empty())?;
        Some(Self {
            agent_id,
            api_key: std::env::var("ELEVENLABS_API_KEY").ok().filter(|s| !s.is_empty()),
        })
    }
}

/// Build the voice bridge. We need each shard's `MessageSender` so Songbird
/// can send VOICE_STATE_UPDATE opcodes back through the gateway; that's how
/// the bot tells Discord which voice channel to join.
pub fn init(
    user_id: Id<UserMarker>,
    shards: &[Shard],
    cfg: VoiceConfig,
) -> Result<Arc<VoiceBridge>> {
    let mut senders: HashMap<u64, twilight_gateway::MessageSender> = HashMap::new();
    for shard in shards.iter() {
        senders.insert(shard.id().number(), shard.sender());
    }
    let twilight_map = Arc::new(TwilightMap::new(senders));
    let songbird = Arc::new(Songbird::twilight(twilight_map, user_id));
    Ok(VoiceBridge::new(songbird, cfg.agent_id, cfg.api_key))
}

/// Hand a Twilight gateway event to Songbird so it sees voice state / voice
/// server updates. Non-voice events are no-ops inside Songbird.
pub async fn process_gateway_event(bridge: &VoiceBridge, event: &twilight_gateway::Event) {
    bridge.songbird.process(event).await;
}
