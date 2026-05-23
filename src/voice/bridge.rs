//! Audio bridge between Discord voice (via Songbird) and an ElevenLabs
//! Conversational AI session.
//!
//! Per-call lifecycle:
//! 1. `start_call` joins a voice channel, opens an ElevenLabs session, and
//!    spawns the three pipeline tasks (RX→ElevenLabs, ElevenLabs→TX, event
//!    logging).
//! 2. While the call is active, Discord mic audio flows out to ElevenLabs,
//!    and the agent's audio flows back into the voice channel.
//! 3. `stop_call` aborts the pipeline tasks and leaves the voice channel.
//!
//! Songbird's TX side wants a `songbird::input::Input` that yields the audio
//! we want played. We feed it a chunked PCM stream built from an mpsc receiver
//! that the ElevenLabs→TX task writes to.

use std::io::{Read, Result as IoResult, Seek, SeekFrom};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use songbird::events::{Event, EventContext, EventHandler as VoiceEventHandler};
use songbird::input::{Input, RawAdapter};
use songbird::{CoreEvent, Songbird};
use symphonia_core::io::MediaSource;
use tokio::sync::mpsc;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker},
    Id,
};

use super::elevenlabs::{self, AgentEvent};
use super::resample;
use super::voice_state::VoiceStateCache;

/// Tracks a single active voice call so we can tear it down cleanly later.
pub struct ActiveCall {
    #[allow(dead_code)]
    guild_id: Id<GuildMarker>,
    eleven_to_tx_task: tokio::task::JoinHandle<()>,
    ws_writer_task: tokio::task::JoinHandle<()>,
    ws_reader_task: tokio::task::JoinHandle<()>,
}

/// Manager for voice calls — wraps Songbird and keeps per-guild state.
pub struct VoiceBridge {
    pub songbird: Arc<Songbird>,
    pub voice_states: Arc<VoiceStateCache>,
    agent_id: String,
    api_key: Option<String>,
    active: Mutex<std::collections::HashMap<Id<GuildMarker>, ActiveCall>>,
}

impl VoiceBridge {
    pub fn new(songbird: Arc<Songbird>, agent_id: String, api_key: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            songbird,
            voice_states: Arc::new(VoiceStateCache::new()),
            agent_id,
            api_key,
            active: Mutex::new(Default::default()),
        })
    }

    /// Join a voice channel and start the ElevenLabs bridge. Replaces any
    /// existing call in the same guild.
    pub async fn start_call(
        self: &Arc<Self>,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    ) -> Result<()> {
        // Tear down any pre-existing call so we don't leak tasks.
        self.stop_call(guild_id).await.ok();

        // Songbird's GuildId/ChannelId have From impls for the matching
        // twilight Id types, so we can pass them through unchanged.
        let call = self
            .songbird
            .join(guild_id, channel_id)
            .await
            .context("songbird join failed")?;

        // Open the ElevenLabs session up front, then decompose it so we can
        // hand the audio sender into the Discord RX handler while the event
        // receiver feeds the TX (playback) task.
        let session = elevenlabs::connect(&self.agent_id, self.api_key.as_deref())
            .await
            .context("ElevenLabs connect failed")?;
        let elevenlabs::SessionParts {
            audio_tx,
            events_rx,
            writer_task: ws_writer_task,
            reader_task: ws_reader_task,
        } = session.into_parts();

        // Wire up Discord RX → ElevenLabs.
        let rx_handler = DiscordRxHandler {
            audio_tx: audio_tx.clone(),
        };
        {
            let mut driver = call.lock().await;
            driver.add_global_event(CoreEvent::VoiceTick.into(), rx_handler);
        }

        // Build the TX pipeline: a shared queue of agent PCM samples (16k
        // mono) that we'll resample to 48k stereo just-in-time inside a Read
        // adapter that Songbird pulls from.
        let agent_pcm_queue: Arc<Mutex<std::collections::VecDeque<u8>>> =
            Arc::new(Mutex::new(Default::default()));
        let tx_reader = PcmQueueReader {
            queue: agent_pcm_queue.clone(),
        };

        // Songbird wants something Symphonia can demux. `RawAdapter` wraps a
        // `Read` of raw interleaved PCM — we tell it 48 kHz, stereo.
        let adapter = RawAdapter::new(tx_reader, 48_000, 2);
        let input: Input = adapter.into();
        {
            let mut driver = call.lock().await;
            driver.play_input(input);
        }

        // ElevenLabs → TX: pull events, on Audio resample and append to the
        // shared queue. On Interruption, clear the queue so we don't keep
        // talking over the user.
        let eleven_to_tx_task = {
            let queue = agent_pcm_queue.clone();
            let mut events_rx = events_rx;
            tokio::spawn(async move {
                while let Some(evt) = events_rx.recv().await {
                    match evt {
                        AgentEvent::Audio(pcm16k_mono) => {
                            // Songbird's RawAdapter consumes interleaved f32
                            // PCM, so we widen each i16 sample to f32 in the
                            // -1.0..1.0 range before pushing to the queue.
                            let pcm48k_stereo =
                                resample::up_16k_mono_to_48k_stereo(&pcm16k_mono);
                            let mut bytes = Vec::with_capacity(pcm48k_stereo.len() * 4);
                            for s in pcm48k_stereo {
                                let f = (s as f32) / (i16::MAX as f32 + 1.0);
                                bytes.extend_from_slice(&f.to_le_bytes());
                            }
                            queue.lock().extend(bytes);
                        }
                        AgentEvent::Interruption => {
                            queue.lock().clear();
                        }
                        AgentEvent::UserTranscript(t) => {
                            tracing::info!("[user→agent] {}", t);
                        }
                        AgentEvent::AgentResponse(t) => {
                            tracing::info!("[agent→user] {}", t);
                        }
                    }
                }
                tracing::info!("ElevenLabs event stream ended");
            })
        };

        self.active.lock().insert(
            guild_id,
            ActiveCall {
                guild_id,
                eleven_to_tx_task,
                ws_writer_task,
                ws_reader_task,
            },
        );

        Ok(())
    }

    /// Stop the call: abort tasks, leave the voice channel.
    pub async fn stop_call(self: &Arc<Self>, guild_id: Id<GuildMarker>) -> Result<()> {
        let call = self.active.lock().remove(&guild_id);
        if let Some(call) = call {
            call.eleven_to_tx_task.abort();
            call.ws_writer_task.abort();
            call.ws_reader_task.abort();
        }
        self.songbird
            .remove(guild_id)
            .await
            .map_err(|e| anyhow!("songbird remove failed: {e}"))?;
        Ok(())
    }
}

/// Songbird VoiceTick handler: pulls decoded 48 kHz stereo PCM from each
/// speaking SSRC and forwards a resampled 16 kHz mono frame to ElevenLabs.
struct DiscordRxHandler {
    audio_tx: mpsc::Sender<Vec<i16>>,
}

#[async_trait]
impl VoiceEventHandler for DiscordRxHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoiceTick(tick) = ctx {
            // Mix every speaking user's frame into a single mono stream. For a
            // voice agent we usually want one speaker at a time, but mixing
            // means group conversations still flow without us having to pick.
            let mut mixed: Vec<i32> = Vec::new();
            for (_ssrc, data) in tick.speaking.iter() {
                let Some(decoded) = data.decoded_voice.as_ref() else {
                    continue;
                };
                if mixed.is_empty() {
                    mixed = decoded.iter().map(|&s| s as i32).collect();
                } else {
                    for (i, &s) in decoded.iter().enumerate() {
                        if let Some(slot) = mixed.get_mut(i) {
                            *slot += s as i32;
                        }
                    }
                }
            }
            if mixed.is_empty() {
                return None;
            }
            let pcm48: Vec<i16> = mixed
                .into_iter()
                .map(|s| s.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .collect();
            let pcm16 = resample::down_48k_stereo_to_16k_mono(&pcm48);
            // try_send: drop frames rather than block the voice tick if
            // ElevenLabs is backed up. The agent will recover.
            if let Err(e) = self.audio_tx.try_send(pcm16) {
                tracing::trace!("dropped mic frame ({}): ElevenLabs queue full", e);
            }
        }
        None
    }
}

/// Blocking `Read` adapter over a shared byte queue. Songbird's `RawAdapter`
/// drives this from a blocking thread, so returning 0 would mean "stream
/// ended" — we never want that, so empty queues produce zero-filled bytes
/// (silence) instead. The 10 ms sleep when empty prevents a busy loop while
/// the agent isn't talking.
///
/// Must also satisfy Symphonia's `MediaSource`: non-seekable, unknown length.
struct PcmQueueReader {
    queue: Arc<Mutex<std::collections::VecDeque<u8>>>,
}

impl Read for PcmQueueReader {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        {
            let mut q = self.queue.lock();
            if !q.is_empty() {
                let n = buf.len().min(q.len());
                for slot in buf.iter_mut().take(n) {
                    *slot = q.pop_front().unwrap();
                }
                return Ok(n);
            }
        }
        // Empty queue: hand back a small block of silence. Cap at 7680 bytes
        // (one Discord 20 ms stereo frame at f32: 480 samples × 2 ch × 4 B)
        // so reads stay short and we re-poll the queue often.
        let n = buf.len().min(7680);
        for slot in buf.iter_mut().take(n) {
            *slot = 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(n)
    }
}

impl Seek for PcmQueueReader {
    fn seek(&mut self, _pos: SeekFrom) -> IoResult<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "PcmQueueReader is a live stream and cannot seek",
        ))
    }
}

impl MediaSource for PcmQueueReader {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}
