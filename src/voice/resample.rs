//! Tiny resampler for the Discord ↔ ElevenLabs audio bridge.
//!
//! Discord voice is 48 kHz stereo signed 16-bit PCM. ElevenLabs Conversational
//! AI expects (and emits) 16 kHz mono signed 16-bit PCM. The ratio is exactly
//! 3:1 so we don't need a real polyphase resampler — a 3-tap box filter on the
//! way down and zero-order hold (sample-and-hold ×3) on the way up are both
//! cheap and good enough for a voice agent. If audio quality ever becomes a
//! concern we can swap in `rubato` or similar later.

/// Convert 48 kHz stereo s16 → 16 kHz mono s16.
///
/// `input` length should be a multiple of 6 (3 stereo pairs per output sample);
/// any trailing partial group is dropped, which is fine because the next chunk
/// will pick up where we left off. Discord delivers frames in 20 ms (= 960
/// stereo samples = 1920 i16 values) so this divides evenly.
pub fn down_48k_stereo_to_16k_mono(input: &[i16]) -> Vec<i16> {
    let groups = input.len() / 6;
    let mut out = Vec::with_capacity(groups);
    for g in 0..groups {
        let base = g * 6;
        // Average each stereo pair, then average the three results. Using i32
        // accumulation avoids i16 overflow on loud frames.
        let s0 = (input[base] as i32 + input[base + 1] as i32) / 2;
        let s1 = (input[base + 2] as i32 + input[base + 3] as i32) / 2;
        let s2 = (input[base + 4] as i32 + input[base + 5] as i32) / 2;
        let avg = (s0 + s1 + s2) / 3;
        out.push(avg.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }
    out
}

/// Convert 16 kHz mono s16 → 48 kHz stereo s16. Each input sample becomes 3
/// output stereo pairs (sample-and-hold). For voice agent output the artifacts
/// are barely audible and the CPU savings on a Pi 4 are worth it.
pub fn up_16k_mono_to_48k_stereo(input: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(input.len() * 6);
    for &s in input {
        for _ in 0..3 {
            out.push(s); // L
            out.push(s); // R
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_drops_partial_group() {
        // 7 samples — not a multiple of 6 — should still produce 1 output (6/6)
        // and discard the 7th.
        let input = vec![100i16; 7];
        let out = down_48k_stereo_to_16k_mono(&input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], 100);
    }

    #[test]
    fn down_averages_constant_signal() {
        let input = vec![500i16; 60]; // 10 output samples expected
        let out = down_48k_stereo_to_16k_mono(&input);
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|&s| s == 500));
    }

    #[test]
    fn up_triples_and_widens_to_stereo() {
        let input = vec![1i16, 2, 3];
        let out = up_16k_mono_to_48k_stereo(&input);
        assert_eq!(out.len(), 18);
        // First 6 should all be `1` (3 stereo pairs of mono sample 1).
        assert_eq!(&out[0..6], &[1, 1, 1, 1, 1, 1]);
        assert_eq!(&out[6..12], &[2, 2, 2, 2, 2, 2]);
        assert_eq!(&out[12..18], &[3, 3, 3, 3, 3, 3]);
    }

    #[test]
    fn down_then_up_preserves_constant() {
        let input = vec![1234i16; 600];
        let mid = down_48k_stereo_to_16k_mono(&input);
        let back = up_16k_mono_to_48k_stereo(&mid);
        assert_eq!(back.len(), input.len());
        assert!(back.iter().all(|&s| s == 1234));
    }
}
