//! Reveal, hold and vanish timing in milliseconds, independent of GTK and
//! clocks. The hold starts after the reveal completes.

use crate::config::{Reveal, Vanish};

/// Seeded xorshift64* for reproducible typing jitter.
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
    /// Character reveal times in ascending milliseconds from t0. Empty for
    /// instant reveal. Animation and audio share these times, including jitter.
    steps: Vec<f64>,
    chars: usize,
    reveal_ms: f64,
    hold_ms: f64,
    vanish_ms: f64,
    /// Cached non-whitespace character positions eligible for blips.
    audible: Vec<bool>,
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
        Timeline::resuming(text, reveal, timeout_ms, vanish, seed, 0)
    }

    /// A timeline for a block whose first `shown` characters are already on
    /// screen.
    ///
    /// They are given a step of zero, so they are visible on the first frame
    /// and the typewriter runs only over what follows. A listener appends a
    /// line to a block that is already up; retyping the whole block on every
    /// arrival is the thing this exists to avoid.
    ///
    /// They are also struck from `audible`: a step of zero would otherwise
    /// fire every one of their blips at once at t0.
    pub fn resuming(
        text: &str,
        reveal: &Reveal,
        timeout_ms: u64,
        vanish: &Vanish,
        seed: u64,
        shown: usize,
    ) -> Timeline {
        let chars = text.chars().count();
        let shown = shown.min(chars);
        let steps = match reveal {
            Reveal::Instant => Vec::new(),
            // Defensive fallback for non-positive cps; normal input is
            // validated earlier.
            Reveal::Typewriter { cps, .. } if *cps <= 0.0 => Vec::new(),
            Reveal::Typewriter { cps, jitter, .. } => {
                let base = 1000.0 / cps;
                let jitter = jitter.clamp(0.0, 1.0);
                let mut rng = Rng::new(seed);
                let mut t = 0.0;
                (0..chars)
                    .map(|i| {
                        if i < shown {
                            return 0.0;
                        }
                        // Clamping jitter to 1 keeps gaps non-negative and
                        // steps sorted.
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
            audible: text
                .chars()
                .enumerate()
                .map(|(i, c)| i >= shown && !c.is_whitespace())
                .collect(),
            chars,
            hold_ms: timeout_ms as f64,
            vanish_ms: vanish.duration_ms(chars) as f64,
            untype: vanish.is_untype(),
        }
    }

    pub fn phase_at(&self, t_ms: f64) -> Phase {
        if t_ms < self.reveal_ms {
            // Sorted steps allow a partition-point lookup bounded by the
            // character count.
            return Phase::Reveal {
                chars: self.steps.partition_point(|&s| s <= t_ms),
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

    /// Blip times in seconds from t0, excluding whitespace.
    pub fn onsets(&self, every: usize) -> Vec<f64> {
        if self.steps.is_empty() {
            return Vec::new();
        }
        let every = every.max(1);
        self.blip_indices(every)
            // Use the same onset as the visual reveal.
            .filter_map(|i| self.steps.get(i).map(|ms| ms / 1000.0))
            .collect()
    }

    fn blip_indices(&self, every: usize) -> impl Iterator<Item = usize> + '_ {
        self.audible
            .iter()
            .enumerate()
            .filter(move |(i, audible)| **audible && i.is_multiple_of(every))
            .map(|(i, _)| i)
    }

    /// Untype blip times in seconds from the start of vanish. The caller delays
    /// playback by `vanish_start()` to avoid allocating hold-time silence.
    /// Other effects return no onsets.
    pub fn vanish_onsets(&self, every: usize) -> Vec<f64> {
        if !self.untype || self.vanish_ms <= 0.0 || self.chars == 0 {
            return Vec::new();
        }
        // Erase at a constant rate.
        let step = self.vanish_ms / self.chars as f64 / 1000.0;
        self.blip_indices(every.max(1))
            // Erased from the end: the last character goes first.
            .map(|i| (self.chars - i) as f64 * step)
            .collect()
    }

    /// When the vanish begins, in seconds from t0.
    pub fn vanish_start(&self) -> f64 {
        (self.reveal_ms + self.hold_ms) / 1000.0
    }

    /// Visible character count during untype, matched to `vanish_onsets`
    /// timing.
    pub fn untype_visible(&self, p: f64) -> usize {
        (((1.0 - p) * self.chars as f64).ceil() as usize).min(self.chars)
    }

    /// Everything from t0 to the frame the window closes.
    pub fn total_ms(&self) -> f64 {
        self.reveal_ms + self.hold_ms + self.vanish_ms
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
            scroll: false,
        }
    }

    fn tw_jitter(cps: f64, jitter: f64) -> Reveal {
        Reveal::Typewriter {
            cps,
            cursor: true,
            jitter,
            scroll: false,
        }
    }

    /// Fixed seed: jitter must be reproducible in a test.
    const SEED: u64 = 0x1234_5678_9abc_def0;

    fn timeline(text: &str, reveal: &Reveal, timeout_ms: u64, vanish: &Vanish) -> Timeline {
        Timeline::new(text, reveal, timeout_ms, vanish, SEED)
    }

    #[test]
    fn a_resumed_block_shows_what_was_already_typed_at_once() {
        // Four characters up, two more arriving: the first four are there on
        // the first frame and only the new pair is typed.
        let tl = Timeline::resuming("abcdef", &tw(10.0), 500, &Vanish::Instant, 1, 4);
        assert_eq!(tl.phase_at(0.0), Phase::Reveal { chars: 4 });
        assert_eq!(tl.phase_at(100.0), Phase::Reveal { chars: 5 });
        assert_eq!(tl.phase_at(200.0), Phase::Hold);
        // The reveal is as long as the new part, not the whole block.
        assert!(
            (tl.reveal_ms - 200.0).abs() < 1e-9,
            "reveal_ms {}",
            tl.reveal_ms
        );
    }

    #[test]
    fn a_resumed_block_does_not_blip_for_what_is_already_up() {
        // A step of zero would fire every earlier character's blip at t0.
        let tl = Timeline::resuming("abcdef", &tw(10.0), 0, &Vanish::Instant, 1, 4);
        let onsets = tl.onsets(1);
        assert_eq!(onsets.len(), 2, "{onsets:?}");
        assert!(onsets.iter().all(|&t| t > 0.0), "{onsets:?}");
    }

    #[test]
    fn resuming_past_the_end_is_a_block_with_nothing_to_type() {
        let tl = Timeline::resuming("abc", &tw(10.0), 500, &Vanish::Instant, 1, 99);
        assert_eq!(tl.phase_at(0.0), Phase::Hold);
        assert!(tl.onsets(1).is_empty());
    }

    #[test]
    fn resuming_from_zero_is_the_plain_constructor() {
        let a = Timeline::new("abcdef", &tw(10.0), 500, &Vanish::Instant, 7);
        let b = Timeline::resuming("abcdef", &tw(10.0), 500, &Vanish::Instant, 7, 0);
        assert_eq!(a.steps, b.steps);
        assert_eq!(a.onsets(1), b.onsets(1));
    }

    #[test]
    fn hold_starts_after_the_reveal_not_at_t0() {
        // 10 characters at 10 cps: 1000 ms reveal, then 5000 ms hold.
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
        let all = tl.onsets(1);
        assert_eq!(all.len(), 4);
        assert!((all[0] - 0.1).abs() < 1e-9);
        assert!((all[3] - 0.5).abs() < 1e-9);
        // every=2 keeps indices 0,2,4 -> minus the space at 2 -> 0 and 4.
        assert_eq!(tl.onsets(2).len(), 2);
    }

    #[test]
    fn untype_blips_run_backwards_and_are_vanish_relative() {
        let tl = timeline("abcd", &tw(10.0), 1000, &Vanish::Untype { cps: 10.0 });
        // reveal 400 ms + hold 1000 ms.
        assert!((tl.vanish_start() - 1.4).abs() < 1e-9);
        let on = tl.vanish_onsets(1);
        assert_eq!(on.len(), 4);
        // Char 3 (the last) is erased first, char 0 last.
        assert!(on[3] < on[0], "erase order must be reversed: {on:?}");
        // Onsets stay within the vanish duration regardless of hold time.
        assert!(on.iter().all(|&t| t <= 0.4), "not vanish-relative: {on:?}");
    }

    #[test]
    fn a_huge_hold_does_not_inflate_the_vanish_onsets() {
        // A long hold must not add silence to the mixed track.
        let tl = timeline("ab", &tw(10.0), 3_600_000, &Vanish::Untype { cps: 10.0 });
        assert!(tl.vanish_onsets(1).iter().all(|&t| t <= 0.2));
    }

    #[test]
    fn an_untype_blip_lands_when_its_character_disappears() {
        // Each blip coincides with removal of its character.
        let tl = timeline("abcdef", &tw(10.0), 0, &Vanish::Untype { cps: 10.0 });
        let onsets = tl.vanish_onsets(1);
        for (i, t) in onsets.iter().enumerate() {
            let p = t * 1000.0 / 600.0;
            assert_eq!(
                tl.untype_visible(p),
                i,
                "blip {i} at p={p} leaves {} visible",
                tl.untype_visible(p)
            );
        }
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
            assert!(tl.vanish_onsets(1).is_empty(), "{v:?} should be silent");
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
        // `partition_point` requires sorted times.
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
        // Jittered audio onsets must match character reveal times.
        let tl = timeline("abcd", &tw_jitter(10.0, 0.5), 0, &Vanish::Instant);
        let onsets = tl.onsets(1);
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
        assert!(tl.onsets(1).is_empty());
    }
}
