//! The animation state machine. Pure arithmetic on milliseconds — no GTK, no
//! clock — so the phase boundaries can be unit-tested instead of eyeballed.
//!
//! The hold is measured from the END of the reveal, per the CLI contract:
//! `--timeout 5` means five seconds of readable text, whatever the typewriter
//! spent getting there.

use crate::config::{Reveal, Vanish};

/// A tiny xorshift64*. Not for anything that matters — it decides how long a
/// keystroke waits — but seeded explicitly so a run is reproducible in a test.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // Zero is a fixed point of xorshift; anything else is fine.
        Rng(seed | 1)
    }

    /// Uniform in 0.0..1.0.
    fn unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    /// Text is still being typed out; `chars` are visible so far.
    Reveal { chars: usize },
    /// Fully visible, waiting out the timeout.
    Hold,
    /// Going away; `p` runs 0.0 -> 1.0.
    Vanish { p: f64 },
    /// Nothing left to draw; the window can close.
    Done,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    /// When each character becomes visible, in ms from t0, ascending. Empty
    /// for an instant reveal.
    ///
    /// Materialised rather than computed on demand because jitter makes the
    /// gaps unequal: the animation and the blip track have to agree on the
    /// exact same moments, and two formulas would drift apart the day one of
    /// them changed.
    steps: Vec<f64>,
    chars: usize,
    reveal_ms: f64,
    hold_ms: f64,
    vanish_ms: f64,
    /// Untype erases character by character, so it gets blips of its own.
    untype: bool,
}

impl Timeline {
    pub fn new(
        text: &str,
        reveal: &Reveal,
        timeout_ms: u64,
        vanish: &Vanish,
        seed: u64,
    ) -> Timeline {
        let chars = text.chars().count();
        let steps = match reveal {
            Reveal::Instant => Vec::new(),
            // A non-positive cps in the config would divide by zero and hang
            // the HUD on screen forever; treat it as "instant" instead.
            Reveal::Typewriter { cps, .. } if *cps <= 0.0 => Vec::new(),
            Reveal::Typewriter { cps, jitter, .. } => {
                let base = 1000.0 / cps;
                let jitter = jitter.clamp(0.0, 1.0);
                let mut rng = Rng::new(seed);
                let mut t = 0.0;
                (0..chars)
                    .map(|_| {
                        // 1 +/- jitter. Clamped at 1.0 above, so the factor
                        // cannot go negative and the sequence stays ascending
                        // — phase_at counts on that.
                        let factor = 1.0 + jitter * (rng.unit() * 2.0 - 1.0);
                        t += base * factor;
                        t
                    })
                    .collect()
            }
        };
        Timeline {
            reveal_ms: steps.last().copied().unwrap_or(0.0),
            steps,
            chars,
            hold_ms: timeout_ms as f64,
            vanish_ms: vanish.ms() as f64,
            untype: vanish.is_untype(),
        }
    }

    pub fn phase_at(&self, t_ms: f64) -> Phase {
        if t_ms < self.reveal_ms {
            // A character is visible once its own moment has passed. The
            // steps ascend, so this is a partition point.
            let n = self.steps.partition_point(|&s| s <= t_ms);
            return Phase::Reveal {
                chars: n.min(self.chars),
            };
        }
        let t = t_ms - self.reveal_ms;
        if t < self.hold_ms {
            return Phase::Hold;
        }
        let t = t - self.hold_ms;
        if t < self.vanish_ms {
            return Phase::Vanish {
                p: (t / self.vanish_ms).clamp(0.0, 1.0),
            };
        }
        Phase::Done
    }

    /// When each blip should sound, in seconds from t0.
    ///
    /// Whitespace is skipped: a space key on a movie terminal doesn't click,
    /// and blipping on newlines sounds like a stutter.
    pub fn onsets(&self, text: &str, every: usize) -> Vec<f64> {
        if self.steps.is_empty() {
            return Vec::new();
        }
        let every = every.max(1);
        text.chars()
            .enumerate()
            .filter(|(i, c)| !c.is_whitespace() && i.is_multiple_of(every))
            // The same moment the character appears on screen — one source of
            // truth, so jitter cannot desynchronise sound from animation.
            .filter_map(|(i, _)| self.steps.get(i).map(|ms| ms / 1000.0))
            .collect()
    }

    /// Blips for an untype vanish, in seconds from the START OF THE VANISH —
    /// not from t0. The caller delays playback by `vanish_start()` instead, so
    /// a long hold never turns into allocated silence.
    ///
    /// Empty for every other mode: nothing is being struck, so nothing clicks.
    pub fn vanish_onsets(&self, text: &str, every: usize) -> Vec<f64> {
        if !self.untype || self.vanish_ms <= 0.0 || self.chars == 0 {
            return Vec::new();
        }
        // The erase runs at a steady rate: it is a machine undoing the text,
        // not a person typing it.
        let every = every.max(1);
        let step = self.vanish_ms / self.chars as f64 / 1000.0;
        text.chars()
            .enumerate()
            .filter(|(i, c)| !c.is_whitespace() && i.is_multiple_of(every))
            // Erased from the end: the last character goes first.
            .map(|(i, _)| (self.chars - i) as f64 * step)
            .collect()
    }

    /// When the vanish begins, in seconds from t0.
    pub fn vanish_start(&self) -> f64 {
        (self.reveal_ms + self.hold_ms) / 1000.0
    }

    pub fn chars(&self) -> usize {
        self.chars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tw(cps: f64) -> Reveal {
        Reveal::Typewriter {
            cps,
            cursor: true,
            jitter: 0.0,
        }
    }

    fn tw_jitter(cps: f64, jitter: f64) -> Reveal {
        Reveal::Typewriter {
            cps,
            cursor: true,
            jitter,
        }
    }

    /// Fixed seed: jitter must be reproducible in a test.
    const SEED: u64 = 0x1234_5678_9abc_def0;

    fn timeline(text: &str, reveal: &Reveal, timeout_ms: u64, vanish: &Vanish) -> Timeline {
        Timeline::new(text, reveal, timeout_ms, vanish, SEED)
    }

    #[test]
    fn hold_starts_after_the_reveal_not_at_t0() {
        // 10 chars at 10 cps = 1000 ms of typing, THEN 5000 ms of hold.
        let tl = timeline("0123456789", &tw(10.0), 5000, &Vanish::Instant);
        assert_eq!(tl.phase_at(999.0), Phase::Reveal { chars: 9 });
        assert_eq!(tl.phase_at(1000.0), Phase::Hold);
        assert_eq!(tl.phase_at(5999.0), Phase::Hold);
        assert_eq!(tl.phase_at(6000.0), Phase::Done);
    }

    #[test]
    fn instant_reveal_skips_straight_to_hold() {
        let tl = timeline("abc", &Reveal::Instant, 100, &Vanish::Instant);
        assert_eq!(tl.phase_at(0.0), Phase::Hold);
        assert_eq!(tl.phase_at(100.0), Phase::Done);
    }

    #[test]
    fn vanish_runs_zero_to_one_then_done() {
        let tl = timeline("ab", &Reveal::Instant, 100, &Vanish::Fade { ms: 200 });
        assert_eq!(tl.phase_at(100.0), Phase::Vanish { p: 0.0 });
        assert_eq!(tl.phase_at(200.0), Phase::Vanish { p: 0.5 });
        assert_eq!(tl.phase_at(300.0), Phase::Done);
    }

    #[test]
    fn zero_cps_does_not_divide_by_zero() {
        // A config typo must not leave the HUD pinned on screen forever.
        let tl = timeline("abc", &tw(0.0), 10, &Vanish::Instant);
        assert_eq!(tl.phase_at(0.0), Phase::Hold);
        assert_eq!(tl.phase_at(10.0), Phase::Done);
    }

    #[test]
    fn empty_text_still_terminates() {
        let tl = timeline("", &tw(10.0), 10, &Vanish::Instant);
        assert_eq!(tl.phase_at(0.0), Phase::Hold);
        assert_eq!(tl.phase_at(11.0), Phase::Done);
    }

    #[test]
    fn onsets_skip_whitespace_and_respect_every() {
        let tl = timeline("ab cd", &tw(10.0), 0, &Vanish::Instant);
        // step = 100 ms; chars a,b,' ',c,d at indices 0..4, space dropped.
        let all = tl.onsets("ab cd", 1);
        assert_eq!(all.len(), 4);
        assert!((all[0] - 0.1).abs() < 1e-9);
        assert!((all[3] - 0.5).abs() < 1e-9);
        // every=2 keeps indices 0,2,4 -> minus the space at 2 -> 0 and 4.
        assert_eq!(tl.onsets("ab cd", 2).len(), 2);
    }

    #[test]
    fn untype_blips_run_backwards_and_are_vanish_relative() {
        let tl = timeline("abcd", &tw(10.0), 1000, &Vanish::Untype { ms: 400 });
        // reveal 400 ms + hold 1000 ms.
        assert!((tl.vanish_start() - 1.4).abs() < 1e-9);
        let on = tl.vanish_onsets("abcd", 1);
        assert_eq!(on.len(), 4);
        // Char 3 (the last) is erased first, char 0 last.
        assert!(on[3] < on[0], "erase order must be reversed: {on:?}");
        // Relative to the vanish, so bounded by its duration however long the
        // hold was — that is what keeps the mixed track small.
        assert!(on.iter().all(|&t| t <= 0.4), "not vanish-relative: {on:?}");
    }

    #[test]
    fn a_huge_hold_does_not_inflate_the_vanish_onsets() {
        // The bug this guards: onsets measured from t0 made typewriter_track
        // allocate silence for the whole timeout — an hour was a gigabyte.
        let tl = timeline("ab", &tw(10.0), 3_600_000, &Vanish::Untype { ms: 200 });
        assert!(tl.vanish_onsets("ab", 1).iter().all(|&t| t <= 0.2));
    }

    #[test]
    fn only_untype_gets_vanish_blips() {
        for v in [
            Vanish::Fade { ms: 400 },
            Vanish::Collapse { ms: 400 },
            Vanish::Wash {
                ms: 400,
                dir: crate::config::Dir::Up,
            },
            Vanish::Dissolve { ms: 400 },
            Vanish::Instant,
        ] {
            let tl = timeline("abcd", &tw(10.0), 100, &v);
            assert!(
                tl.vanish_onsets("abcd", 1).is_empty(),
                "{v:?} should be silent"
            );
        }
    }

    /// Gaps between consecutive character moments.
    fn gaps(tl: &Timeline) -> Vec<f64> {
        let mut prev = 0.0;
        tl.steps
            .iter()
            .map(|&s| {
                let g = s - prev;
                prev = s;
                g
            })
            .collect()
    }

    #[test]
    fn zero_jitter_is_a_metronome() {
        let tl = timeline("abcdefgh", &tw(10.0), 0, &Vanish::Instant);
        for g in gaps(&tl) {
            assert!((g - 100.0).abs() < 1e-9, "uneven gap {g} with jitter off");
        }
    }

    #[test]
    fn jitter_varies_the_gaps_but_keeps_them_ordered() {
        let tl = timeline("abcdefghij", &tw_jitter(10.0, 0.4), 0, &Vanish::Instant);
        let g = gaps(&tl);
        assert!(
            g.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-6),
            "jitter produced identical gaps: {g:?}"
        );
        // Monotonic moments are what phase_at's partition_point relies on.
        assert!(
            tl.steps.windows(2).all(|w| w[0] <= w[1]),
            "steps not ascending"
        );
        // Every gap stays inside 1 +/- jitter of the nominal 100 ms.
        for gap in &g {
            assert!(
                (60.0..=140.0).contains(gap),
                "gap {gap} outside +/-40% of 100 ms"
            );
        }
    }

    #[test]
    fn jitter_is_reproducible_for_a_seed_and_differs_between_seeds() {
        let mk = |seed| {
            Timeline::new("abcdefgh", &tw_jitter(10.0, 0.5), 0, &Vanish::Instant, seed).steps
        };
        assert_eq!(mk(7), mk(7), "same seed must replay identically");
        assert_ne!(mk(7), mk(8), "different seeds must not coincide");
    }

    #[test]
    fn jitter_does_not_move_the_average_much() {
        // Symmetric jitter: the reveal should still take roughly chars/cps.
        let tl = timeline(&"x".repeat(200), &tw_jitter(50.0, 0.6), 0, &Vanish::Instant);
        let nominal = 200.0 / 50.0 * 1000.0;
        let actual = tl.phase_at(f64::MAX);
        assert_eq!(actual, Phase::Done);
        let total: f64 = tl.steps.last().copied().unwrap();
        assert!(
            (total - nominal).abs() < nominal * 0.15,
            "reveal took {total} ms against a nominal {nominal}"
        );
    }

    #[test]
    fn sound_onsets_are_the_same_moments_as_the_animation() {
        // The reason steps are materialised: with jitter, a second formula
        // for the blip times would drift away from what is on screen.
        let tl = timeline("abcd", &tw_jitter(10.0, 0.5), 0, &Vanish::Instant);
        let onsets = tl.onsets("abcd", 1);
        assert_eq!(onsets.len(), 4);
        for (i, t) in onsets.iter().enumerate() {
            assert!(
                (t * 1000.0 - tl.steps[i]).abs() < 1e-9,
                "blip {i} at {t}s does not match step {}",
                tl.steps[i]
            );
        }
    }

    #[test]
    fn instant_reveal_has_no_onsets() {
        let tl = timeline("abc", &Reveal::Instant, 0, &Vanish::Instant);
        assert!(tl.onsets("abc", 1).is_empty());
    }
}
