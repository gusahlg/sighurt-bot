//! ElevenLabs Conversational AI WebSocket bridge.
//!
//! Connects to `wss://api.elevenlabs.io/v1/convai/conversation?agent_id=...`,
//! sends the `conversation_initiation_client_data` handshake, ships outbound
//! mic audio as base64-encoded `user_audio_chunk` frames, and surfaces inbound
//! agent audio + transcripts on a tokio channel. Ping/pong is handled here so
//! callers never see it.
//!
//! Audio in/out of this layer is **16 kHz mono signed-16 PCM**, which matches
//! ElevenLabs' wire format directly — resampling between Discord's 48 kHz
//! stereo and this format lives in the `resample` module so this file stays
//! transport-only.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};

/// Events emitted by the ElevenLabs session. Consumers downstream pick the
/// ones they care about (audio for playback, transcripts for logging).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 16 kHz mono PCM audio chunk from the agent.
    Audio(Vec<i16>),
    /// User speech, as transcribed by ElevenLabs ASR.
    UserTranscript(String),
    /// Agent text response (parallel to audio).
    AgentResponse(String),
    /// Agent's audio playback should be flushed — user interrupted.
    Interruption,
}

/// Handle to a live ElevenLabs session. The bridge code splits this into its
/// component channels via [`Session::into_parts`]; for ad-hoc users
/// [`Session::send_audio`] / [`Session::next_event`] cover the common path.
pub struct Session {
    audio_tx: mpsc::Sender<Vec<i16>>,
    events_rx: mpsc::Receiver<AgentEvent>,
    writer_task: tokio::task::JoinHandle<()>,
    reader_task: tokio::task::JoinHandle<()>,
}

/// Owned pieces of a session — the audio sink, event source, and the two
/// background tasks that need to stay alive for the session to function.
/// Drop the task handles (or `abort` them) to end the session.
pub struct SessionParts {
    pub audio_tx: mpsc::Sender<Vec<i16>>,
    pub events_rx: mpsc::Receiver<AgentEvent>,
    pub writer_task: tokio::task::JoinHandle<()>,
    pub reader_task: tokio::task::JoinHandle<()>,
}

impl Session {
    /// Push a chunk of 16 kHz mono PCM samples to the agent.
    #[allow(dead_code)]
    pub async fn send_audio(&self, pcm: Vec<i16>) -> Result<()> {
        self.audio_tx
            .send(pcm)
            .await
            .map_err(|_| anyhow!("ElevenLabs session writer closed"))
    }

    /// Pull the next agent event.
    #[allow(dead_code)]
    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.events_rx.recv().await
    }

    /// Decompose into raw parts so callers can keep the channels and tasks
    /// separately (e.g. moving the sender into one task and the receiver into
    /// another). The caller becomes responsible for aborting the tasks.
    pub fn into_parts(self) -> SessionParts {
        SessionParts {
            audio_tx: self.audio_tx,
            events_rx: self.events_rx,
            writer_task: self.writer_task,
            reader_task: self.reader_task,
        }
    }

    /// Close the session and stop the background tasks.
    #[allow(dead_code)]
    pub fn close(self) {
        self.writer_task.abort();
        self.reader_task.abort();
    }
}

/// Open a new ElevenLabs ConvAI session for the given agent ID. The optional
/// API key authenticates against private agents — public agents work without
/// it but supplying it never hurts.
pub async fn connect(agent_id: &str, api_key: Option<&str>) -> Result<Session> {
    let url = format!(
        "wss://api.elevenlabs.io/v1/convai/conversation?agent_id={}",
        agent_id
    );

    // Build the request so we can attach `xi-api-key` for private agents.
    let mut request = url
        .into_client_request()
        .context("invalid ElevenLabs WS url")?;
    if let Some(key) = api_key {
        request.headers_mut().insert(
            "xi-api-key",
            key.parse().context("invalid xi-api-key header value")?,
        );
    }

    let (ws_stream, _resp) = connect_async(request)
        .await
        .context("failed to connect to ElevenLabs ConvAI WebSocket")?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Handshake: ElevenLabs requires a `conversation_initiation_client_data`
    // frame before it'll accept audio. Empty overrides = use the agent's
    // configured prompt/voice/etc.
    let init = json!({ "type": "conversation_initiation_client_data" });
    ws_write
        .send(Message::Text(init.to_string()))
        .await
        .context("failed to send conversation init")?;

    // Outbound queue: caller pushes raw PCM, we encode + send.
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<i16>>(64);
    // Inbound queue: we parse events, caller pulls them.
    let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(64);
    // Ping/pong: the reader hands pong replies back to the writer via this
    // channel so we only have one writer task touching the socket.
    let (pong_tx, mut pong_rx) = mpsc::channel::<String>(8);

    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                // Prefer pong replies — small, latency-sensitive.
                Some(pong) = pong_rx.recv() => {
                    if let Err(e) = ws_write.send(Message::Text(pong)).await {
                        tracing::warn!("elevenlabs ws pong send failed: {}", e);
                        break;
                    }
                }
                Some(samples) = audio_rx.recv() => {
                    let bytes = pcm_i16_to_bytes(&samples);
                    let b64 = B64.encode(&bytes);
                    let msg = json!({ "user_audio_chunk": b64 });
                    if let Err(e) = ws_write.send(Message::Text(msg.to_string())).await {
                        tracing::warn!("elevenlabs ws audio send failed: {}", e);
                        break;
                    }
                }
                else => break,
            }
        }
        let _ = ws_write.close().await;
    });

    let reader_task = tokio::spawn(async move {
        while let Some(msg) = ws_read.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("elevenlabs ws read error: {}", e);
                    break;
                }
            };
            match msg {
                Message::Text(text) => {
                    if let Err(e) = handle_text_frame(&text, &events_tx, &pong_tx).await {
                        tracing::warn!("elevenlabs ws frame handler error: {}", e);
                    }
                }
                Message::Close(_) => {
                    tracing::info!("elevenlabs ws closed by remote");
                    break;
                }
                // We don't expect binary frames from ConvAI but ignoring them
                // beats killing the session if the API ever adds them.
                _ => {}
            }
        }
    });

    Ok(Session {
        audio_tx,
        events_rx,
        writer_task,
        reader_task,
    })
}

/// Parse a single text frame from ElevenLabs and route it. The ping/pong path
/// schedules a delayed pong back through `pong_tx` so the single writer task
/// stays the sole socket owner.
async fn handle_text_frame(
    text: &str,
    events_tx: &mpsc::Sender<AgentEvent>,
    pong_tx: &mpsc::Sender<String>,
) -> Result<()> {
    let v: Value = serde_json::from_str(text).context("non-JSON ws frame")?;
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "audio" => {
            let b64 = v
                .get("audio_event")
                .and_then(|e| e.get("audio_base_64"))
                .and_then(|s| s.as_str())
                .ok_or_else(|| anyhow!("audio event missing audio_base_64"))?;
            let bytes = B64.decode(b64).context("audio b64 decode failed")?;
            let samples = bytes_to_pcm_i16(&bytes);
            let _ = events_tx.send(AgentEvent::Audio(samples)).await;
        }
        "user_transcript" => {
            if let Some(t) = v
                .get("user_transcription_event")
                .and_then(|e| e.get("user_transcript"))
                .and_then(|s| s.as_str())
            {
                let _ = events_tx
                    .send(AgentEvent::UserTranscript(t.to_string()))
                    .await;
            }
        }
        "agent_response" => {
            if let Some(t) = v
                .get("agent_response_event")
                .and_then(|e| e.get("agent_response"))
                .and_then(|s| s.as_str())
            {
                let _ = events_tx
                    .send(AgentEvent::AgentResponse(t.to_string()))
                    .await;
            }
        }
        "interruption" => {
            let _ = events_tx.send(AgentEvent::Interruption).await;
        }
        "ping" => {
            // ElevenLabs wants us to wait `ping_ms` before replying — that's
            // how it measures round-trip latency. We honor it on a detached
            // task so we don't block the reader.
            let event_id = v
                .get("ping_event")
                .and_then(|e| e.get("event_id"))
                .cloned()
                .unwrap_or(Value::Null);
            let delay_ms = v
                .get("ping_event")
                .and_then(|e| e.get("ping_ms"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let pong_tx = pong_tx.clone();
            tokio::spawn(async move {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                let pong = json!({ "type": "pong", "event_id": event_id }).to_string();
                let _ = pong_tx.send(pong).await;
            });
        }
        // Other event types (conversation_initiation_metadata, vad_score,
        // internal_tentative_agent_response, etc.) — we don't act on them but
        // log at trace for debugging.
        other => {
            tracing::trace!("elevenlabs ws event ignored: {}", other);
        }
    }
    Ok(())
}

fn pcm_i16_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn bytes_to_pcm_i16(bytes: &[u8]) -> Vec<i16> {
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_pcm_bytes() {
        let samples = vec![-32768i16, -1, 0, 1, 32767, 12345];
        let bytes = pcm_i16_to_bytes(&samples);
        assert_eq!(bytes.len(), samples.len() * 2);
        let back = bytes_to_pcm_i16(&bytes);
        assert_eq!(back, samples);
    }
}
