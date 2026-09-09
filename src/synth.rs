//! Blip synthesis from `blyamk`, rendered as float samples for track mixing.

use std::f64::consts::PI;

/// Synthesis parameters, validated by `Style::validate` before allocation.
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

/// Cluster ratios and weights measured from a bell sample in blyamk.
/// Ratios are relative to the base frequency; the octave is twice the base.
const CLUSTER: [(f64, f64); 4] = [
    (0.00, 0.70), // base, ratio 1.0   (1000 Hz) — octave fundamental
    (1.00, 0.63), // 1 - 1*d           (925 Hz)
    (2.00, 0.86), // 1 - 2*d           (850 Hz)
    (2.33, 1.00), // 1 - 2.33*d        (825 Hz) — the dominant partial
];

/// Resolve partials and discard inaudible or aliasing frequencies.
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

    // The caller must accept an empty cluster as silence.
    out.retain(|part| part.freq > 0.0 && part.freq < nyquist && part.weight > 0.0);
    out
}

/// Envelope timing derived from attack and decay.
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

        // decay_ms = time to -60 dB => exp(-decay/tau) = 1/1000 => tau =
        // decay/ln(1000)
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

        // Fade to zero at the final sample; handle a one-sample fade
        // separately.
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

/// Render float samples with peak amplitude `gain` before i16 quantisation.
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

    // Normalise to `gain`; skip division for silence.
    let scale = if peak > 0.0 { p.gain / peak } else { 0.0 };
    for v in &mut raw {
        *v = (*v * scale).clamp(-1.0, 1.0);
    }
    raw
}
