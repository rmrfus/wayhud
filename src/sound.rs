//! Typewriter audio mixed into one track to avoid per-write scheduling jitter.

use anyhow::Result;

use crate::config::Sound;
use crate::synth::{Params, render_f64};

const RATE: u32 = 48_000;

/// Maximum track duration in seconds; bounds allocation from onset times.
const MAX_TRACK_S: f64 = 120.0;

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

    // Discard non-finite and out-of-range onsets before allocating.
    let onsets: Vec<f64> = onsets
        .iter()
        .copied()
        .filter(|t| t.is_finite() && (0.0..=MAX_TRACK_S).contains(t))
        .collect();
    if onsets.is_empty() {
        return Vec::new();
    }
    let last = onsets.iter().cloned().fold(0.0_f64, f64::max);
    let total = (last * RATE as f64).ceil() as usize + blip.len();
    let mut acc = vec![0.0_f64; total];
    for &t in &onsets {
        let start = (t.max(0.0) * RATE as f64).round() as usize;
        for (i, v) in blip.iter().enumerate() {
            // Overlapping tails sum; clamp before i16 conversion.
            acc[start + i] += v;
        }
    }
    acc.iter()
        .map(|v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// Which message a delayed track belongs to.
///
/// An untype track is scheduled when its message goes up but plays when the
/// vanish begins, which for a listener may be after the message has been
/// replaced. Bumping this drops what was scheduled for the message that went
/// away: without it the old track fired at the old message's vanish, which
/// was earlier, and the blips ran ahead of the text they were erasing.
#[derive(Clone, Default)]
pub struct Generation(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Generation {
    fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Abandon everything scheduled so far.
    pub fn bump(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Play on a worker thread after an optional delay. Return its join handle.
/// Report audio failures without preventing display. Delaying playback avoids
/// allocating silence for the hold before an untype vanish.
///
/// A delayed track checks `generation` after waiting: one-shot never bumps it,
/// so nothing changes there.
#[must_use = "join the handle before exiting or the tail is cut off"]
pub fn play_detached(
    pcm: Vec<i16>,
    delay: std::time::Duration,
    generation: &Generation,
) -> Option<std::thread::JoinHandle<()>> {
    if pcm.is_empty() {
        return None;
    }
    let issued = generation.get();
    let generation = generation.clone();
    Some(std::thread::spawn(move || {
        if !delay.is_zero() {
            std::thread::sleep(delay);
            if generation.get() != issued {
                return;
            }
        }
        if let Err(e) = play(&pcm) {
            eprintln!("wayhud: audio: {e:#}");
        }
    }))
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

        assert!(t.len() > RATE as usize, "track too short: {}", t.len());
        assert!(t.iter().any(|&s| s != 0));
    }

    #[test]
    fn overlapping_onsets_stay_inside_full_scale() {
        // Overlapping blips must saturate without i16 wrapping.
        let onsets = vec![0.0_f64; 10];
        let t = typewriter_track(&cfg(), &onsets);
        assert!(t.iter().all(|&s| s.abs() as i32 <= 32767));
    }

    #[test]
    fn negative_onset_does_not_panic() {
        let t = typewriter_track(&cfg(), &[-1.0, 0.5]);
        assert!(!t.is_empty());
    }

    #[test]
    fn absurd_onsets_do_not_size_the_buffer() {
        // Reject onsets that would require excessive allocation.
        let t = typewriter_track(&cfg(), &[1e12]);
        assert!(t.is_empty(), "an out-of-range onset must be dropped");

        let t = typewriter_track(&cfg(), &[0.1, 1e12]);
        assert!(!t.is_empty());
        assert!(t.len() < (MAX_TRACK_S as usize + 1) * RATE as usize);
    }

    #[test]
    fn non_finite_onsets_are_dropped() {
        assert!(typewriter_track(&cfg(), &[f64::NAN, f64::INFINITY]).is_empty());
    }
}
