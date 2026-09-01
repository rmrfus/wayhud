//! The animation state machine. Pure arithmetic on milliseconds — no GTK, no
//! clock — so the phase boundaries can be unit-tested instead of eyeballed.
//!
//! The hold is measured from the END of the reveal, per the CLI contract:
//! `--timeout 5` means five seconds of readable text, whatever the typewriter
//! spent getting there.

use crate::config::{Reveal, Vanish};

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
    chars: usize,
    reveal_ms: f64,
    hold_ms: f64,
    vanish_ms: f64,
    /// Characters per second, kept for sound onsets. 0.0 when instant.
    cps: f64,
    /// Untype erases character by character, so it gets blips of its own.
    untype: bool,
}

impl Timeline {
    pub fn new(text: &str, reveal: &Reveal, timeout_ms: u64, vanish: &Vanish) -> Timeline {
        let chars = text.chars().count();
        let (reveal_ms, cps) = match reveal {
            Reveal::Instant => (0.0, 0.0),
            // A non-positive cps in the config would divide by zero and hang
            // the HUD on screen forever; treat it as "instant" instead.
            Reveal::Typewriter { cps, .. } if *cps <= 0.0 => (0.0, 0.0),
            Reveal::Typewriter { cps, .. } => (chars as f64 / cps * 1000.0, *cps),
        };
        Timeline {
            chars,
            reveal_ms,
            hold_ms: timeout_ms as f64,
            vanish_ms: vanish.ms() as f64,
            cps,
            untype: vanish.is_untype(),
        }
    }

    pub fn phase_at(&self, t_ms: f64) -> Phase {
        if t_ms < self.reveal_ms {
            // floor, so char N only lights up once its full slot has elapsed.
            let n = (t_ms / self.reveal_ms * self.chars as f64).floor() as usize;
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
        if self.cps <= 0.0 || self.chars == 0 {
            return Vec::new();
        }
        let every = every.max(1);
        let step = self.reveal_ms / self.chars as f64 / 1000.0;
        text.chars()
            .enumerate()
            .filter(|(i, c)| !c.is_whitespace() && i.is_multiple_of(every))
            // Char i is fully revealed at the END of its slot.
            .map(|(i, _)| (i + 1) as f64 * step)
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
        Reveal::Typewriter { cps, cursor: true }
    }

    #[test]
    fn hold_starts_after_the_reveal_not_at_t0() {
        // 10 chars at 10 cps = 1000 ms of typing, THEN 5000 ms of hold.
        let tl = Timeline::new("0123456789", &tw(10.0), 5000, &Vanish::Instant);
        assert_eq!(tl.phase_at(999.0), Phase::Reveal { chars: 9 });
        assert_eq!(tl.phase_at(1000.0), Phase::Hold);
        assert_eq!(tl.phase_at(5999.0), Phase::Hold);
        assert_eq!(tl.phase_at(6000.0), Phase::Done);
    }

    #[test]
    fn instant_reveal_skips_straight_to_hold() {
        let tl = Timeline::new("abc", &Reveal::Instant, 100, &Vanish::Instant);
        assert_eq!(tl.phase_at(0.0), Phase::Hold);
        assert_eq!(tl.phase_at(100.0), Phase::Done);
    }

    #[test]
    fn vanish_runs_zero_to_one_then_done() {
        let tl = Timeline::new("ab", &Reveal::Instant, 100, &Vanish::Fade { ms: 200 });
        assert_eq!(tl.phase_at(100.0), Phase::Vanish { p: 0.0 });
        assert_eq!(tl.phase_at(200.0), Phase::Vanish { p: 0.5 });
        assert_eq!(tl.phase_at(300.0), Phase::Done);
    }

    #[test]
    fn zero_cps_does_not_divide_by_zero() {
        // A config typo must not leave the HUD pinned on screen forever.
        let tl = Timeline::new("abc", &tw(0.0), 10, &Vanish::Instant);
        assert_eq!(tl.phase_at(0.0), Phase::Hold);
        assert_eq!(tl.phase_at(10.0), Phase::Done);
    }

    #[test]
    fn empty_text_still_terminates() {
        let tl = Timeline::new("", &tw(10.0), 10, &Vanish::Instant);
        assert_eq!(tl.phase_at(0.0), Phase::Hold);
        assert_eq!(tl.phase_at(11.0), Phase::Done);
    }

    #[test]
    fn onsets_skip_whitespace_and_respect_every() {
        let tl = Timeline::new("ab cd", &tw(10.0), 0, &Vanish::Instant);
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
        let tl = Timeline::new("abcd", &tw(10.0), 1000, &Vanish::Untype { ms: 400 });
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
        let tl = Timeline::new("ab", &tw(10.0), 3_600_000, &Vanish::Untype { ms: 200 });
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
            let tl = Timeline::new("abcd", &tw(10.0), 100, &v);
            assert!(
                tl.vanish_onsets("abcd", 1).is_empty(),
                "{v:?} should be silent"
            );
        }
    }

    #[test]
    fn instant_reveal_has_no_onsets() {
        let tl = Timeline::new("abc", &Reveal::Instant, 0, &Vanish::Instant);
        assert!(tl.onsets("abc", 1).is_empty());
    }
}
