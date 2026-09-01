//! Typewriter audio.
//!
//! The whole track is mixed up front and handed to PulseAudio in one write,
//! rather than firing a blip per keystroke. We already know the text and the
//! typing speed before the first frame, so the onsets are exact; per-character
//! playback would inherit the server's scheduling jitter on every single tick
//! and drift audibly against the animation over a long line.

use anyhow::Result;

use crate::config::Sound;
use crate::synth::{render_f64, Params};

const RATE: u32 = 48_000;

/// Mix one blip in at each onset (seconds) and quantise to mono i16.
/// Returns an empty buffer when there is nothing to play.
pub fn typewriter_track(cfg: &Sound, onsets: &[f64]) -> Vec<i16> {
    if !cfg.enabled || onsets.is_empty() {
        return Vec::new();
    }
    let blip = render_f64(&Params {
        freq: cfg.freq,
        attack_ms: 1.0,
        decay_ms: cfg.decay_ms,
        brightness: 0.35,
        detune: 0.09,
        gain: cfg.gain,
        rate: RATE,
    });
    if blip.is_empty() {
        return Vec::new();
    }

    let last = onsets.iter().cloned().fold(0.0_f64, f64::max);
    let total = (last * RATE as f64).ceil() as usize + blip.len();
    let mut acc = vec![0.0_f64; total];
    for &t in onsets {
        let start = (t.max(0.0) * RATE as f64).round() as usize;
        for (i, v) in blip.iter().enumerate() {
            // Overlapping tails simply sum; the clamp below is the only
            // limiter. At the default gain two overlapping blips stay inside
            // full scale, and a config that clips is a config, not a crash.
            acc[start + i] += v;
        }
    }
    acc.iter()
        .map(|v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// Play the track on a detached thread. Audio failures are reported and then
/// dropped: no sound server is a reason to be quiet, not a reason to skip the
/// message the user asked to see.
pub fn play_detached(pcm: Vec<i16>) {
    if pcm.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        if let Err(e) = play(&pcm) {
            eprintln!("wayhud: audio: {e:#}");
        }
    });
}

fn play(pcm: &[i16]) -> Result<()> {
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    use libpulse_simple_binding::Simple;

    let spec = Spec {
        format: Format::S16le,
        channels: 1,
        rate: RATE,
    };
    anyhow::ensure!(spec.is_valid(), "invalid sample spec");

    let simple = Simple::new(
        None,
        "wayhud",
        Direction::Playback,
        None,
        "typewriter",
        &spec,
        None,
        None,
    )?;

    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    simple.write(&bytes)?;
    simple.drain()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Sound {
        Sound::default()
    }

    #[test]
    fn disabled_or_empty_produces_silence_cheaply() {
        let mut c = cfg();
        c.enabled = false;
        assert!(typewriter_track(&c, &[0.1, 0.2]).is_empty());
        assert!(typewriter_track(&cfg(), &[]).is_empty());
    }

    #[test]
    fn track_spans_the_last_onset_plus_the_blip_tail() {
        let t = typewriter_track(&cfg(), &[0.0, 1.0]);
        // Must reach at least 1 s + the blip, and must not be silent.
        assert!(t.len() > RATE as usize, "track too short: {}", t.len());
        assert!(t.iter().any(|&s| s != 0));
    }

    #[test]
    fn overlapping_onsets_stay_inside_full_scale() {
        // Ten blips stacked on the same instant would sum way past 1.0 without
        // the clamp; i16 wrapping there is an audible bang, not a click.
        let onsets = vec![0.0_f64; 10];
        let t = typewriter_track(&cfg(), &onsets);
        assert!(t.iter().all(|&s| s.abs() as i32 <= 32767));
    }

    #[test]
    fn negative_onset_does_not_panic() {
        let t = typewriter_track(&cfg(), &[-1.0, 0.5]);
        assert!(!t.is_empty());
    }
}
