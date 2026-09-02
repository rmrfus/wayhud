//! Blip synthesis, lifted from `blyamk` (same author, same knobs).
//!
//! Everything here is a pure function of `Params`: no I/O, no globals. wayhud
//! only needs one short tick, but it needs it rendered as floats so the
//! typewriter track can mix many of them at sub-sample-accurate offsets.

use std::f64::consts::PI;

/// Synthesis parameters. Ranges are checked by `Style::validate` before this
/// is built — `decay_ms` in particular sizes the sample buffer.
#[derive(Debug, Clone)]
pub struct Params {
    pub freq: f64,       // base frequency, Hz
    pub attack_ms: f64,  // raised-cosine attack to peak, ms
    pub decay_ms: f64,   // ring-out to -60 dB, ms
    pub brightness: f64, // octave overtone weight, 0..1
    pub detune: f64,     // cluster spread, 0..0.4
    pub gain: f64,       // output peak amplitude, 0..1
    pub rate: u32,       // sample rate, Hz
}

/// One resolved sine partial: absolute frequency + relative weight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Partial {
    pub freq: f64,
    pub weight: f64,
}

/// Cluster + octave calibration measured from a real bell sample in blyamk.
/// The cluster ratios are relative to the base freq; weights are the measured
/// full-signal amplitudes. Note the loudest partial is the *bottom* of the cluster (the
/// bell "hum"), not the base — the base is only the reference the octave
/// overtone doubles.
const CLUSTER: [(f64, f64); 4] = [
    (0.00, 0.70), // base, ratio 1.0   (1000 Hz) — octave fundamental
    (1.00, 0.63), // 1 - 1*d           (925 Hz)
    (2.00, 0.86), // 1 - 2*d           (850 Hz)
    (2.33, 1.00), // 1 - 2.33*d        (825 Hz) — the dominant partial
];

/// Expand the knobs into the actual partials, dropping anything that would
/// alias or contribute nothing. The weight-sum / peak normalization downstream
/// runs over exactly the partials kept here.
pub fn partials(p: &Params) -> Vec<Partial> {
    let nyquist = p.rate as f64 / 2.0;
    let mut out = Vec::with_capacity(CLUSTER.len() + 1);

    for (mul, weight) in CLUSTER {
        // mul is the detune multiplier k in ratio (1 - k*detune).
        let ratio = 1.0 - mul * p.detune;
        out.push(Partial {
            freq: p.freq * ratio,
            weight,
        });
    }
    // Octave overtone; brightness is its weight (0 => effectively absent).
    out.push(Partial {
        freq: p.freq * 2.0,
        weight: p.brightness,
    });

    // Safety: keep only audible, non-aliasing, contributing partials. Extreme
    // detune/freq/rate combos (e.g. -f 8000 --rate 8000) can cull all of them;
    // the caller must tolerate an empty cluster (renders as silence).
    out.retain(|part| part.freq > 0.0 && part.freq < nyquist && part.weight > 0.0);
    out
}

/// Envelope + timing, all derived from attack/decay. `fade` is defined off the
/// body first so there is no self-reference.
struct Envelope {
    attack: f64, // s
    tau: f64,    // s, exp decay time-constant (internal)
    total_n: usize,
    fade_n: usize,
}

impl Envelope {
    fn new(p: &Params) -> Self {
        let rate = p.rate as f64;
        let attack = p.attack_ms / 1000.0;
        let decay = p.decay_ms / 1000.0;
        let body = attack + decay;
        let fade = (0.005_f64).min(body / 4.0); // 5 ms, or body/4 for tiny blips
        let total = body + fade;

        // decay knob = ms to -60 dB => exp(-decay/tau) = 1/1000 => tau = decay/ln(1000)
        let tau = decay / 1000.0_f64.ln();

        let total_n = (total * rate).round() as usize;
        // At least 1 fade sample, never longer than the whole buffer.
        let fade_n = ((fade * rate).round() as usize).clamp(1, total_n.max(1));

        Envelope {
            attack,
            tau,
            total_n,
            fade_n,
        }
    }

    /// env(t) = attack_env * decay_env * fade_out, evaluated at sample `i`.
    fn at(&self, i: usize, rate: f64) -> f64 {
        let t = i as f64 / rate;

        let attack_env = if self.attack <= 0.0 {
            1.0 // avoid 0/0 at t=0 when attack disabled
        } else {
            0.5 * (1.0 - (PI * (t / self.attack).clamp(0.0, 1.0)).cos())
        };

        let decay_env = if t >= self.attack {
            (-(t - self.attack) / self.tau).exp()
        } else {
            1.0
        };

        // Linear fade over the last `fade_n` samples, hitting exactly 0 at the
        // final index (i == total_n - 1). Denominator uses fade_n-1 so the ramp
        // is a clean 1 -> 0 with no residual step; guarded against fade_n == 1.
        let n = self.total_n;
        let fade_out = if i + self.fade_n >= n {
            let denom = (self.fade_n - 1).max(1) as f64;
            (((n - 1 - i) as f64) / denom).clamp(0.0, 1.0)
        } else {
            1.0
        };

        attack_env * decay_env * fade_out
    }
}

/// Render to normalized float samples in [-1, 1], peak-normalized to `gain`.
/// Split out from `render` so tests can assert finiteness / peak before the
/// i16 quantization hides NaNs (NaN as i16 silently becomes 0).
pub fn render_f64(p: &Params) -> Vec<f64> {
    let parts = partials(p);
    let env = Envelope::new(p);
    let rate = p.rate as f64;
    let n = env.total_n;

    // First pass: raw = env * sum of weighted sines. Track the true peak.
    let mut raw = vec![0.0_f64; n];
    let mut peak = 0.0_f64;
    for (i, slot) in raw.iter_mut().enumerate() {
        let t = i as f64 / rate;
        let e = env.at(i, rate);
        let mut s = 0.0;
        for part in &parts {
            s += part.weight * (2.0 * PI * part.freq * t).sin();
        }
        let v = e * s;
        *slot = v;
        let a = v.abs();
        if a > peak {
            peak = a;
        }
    }

    // Second pass: normalize by the measured peak so `gain` IS the peak
    // amplitude. Guard the all-culled/silent case (peak == 0) against 0/0.
    let scale = if peak > 0.0 { p.gain / peak } else { 0.0 };
    for v in &mut raw {
        *v = (*v * scale).clamp(-1.0, 1.0);
    }
    raw
}
