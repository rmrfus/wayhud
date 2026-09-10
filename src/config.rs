//! TOML presets and resolved styles. Values inherit from `[style.default]`,
//! then from built-in defaults.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Maximum total lifetime: reveal + hold + vanish. Overlays cannot be dismissed
/// early.
pub const MAX_LIFETIME_MS: u64 = 3_600_000;

/// Character limit checked before allocating layouts, steps and audio onsets.
pub const MAX_TEXT_CHARS: usize = 100_000;

/// Maximum outline width and glow radius in logical pixels. These increase
/// padding, reducing the wrapping width and enlarging the glow mask.
pub const MAX_EDGE_PX: f64 = 128.0;

/// Widest text block `width` may pin, in logical pixels. Larger than any
/// display this runs on, and bounded because the block sizes the surface and
/// the device-resolution glow mask allocated from it.
pub const MAX_BLOCK_PX: i32 = 16_384;

/// Most lines a block may reserve. Well past what any display shows, and
/// bounded for the same reason as [`MAX_BLOCK_PX`]: the reservation sizes the
/// surface and the masks allocated from it.
pub const MAX_LINES: usize = 256;

/// Widest scanline period, in device pixels. Unlike [`MAX_EDGE_PX`] this does
/// not feed the padding; it is bounded so the mask cannot be built from a
/// value that leaves a single gap across the whole message.
pub const MAX_SCANLINE_PERIOD_PX: f64 = 128.0;

/// Physical horizontal placement, independent of writing direction.
/// `Center` leaves both layer-shell edges unanchored.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Vertical placement; separate from [`HAlign`] to reject invalid axis values.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VAlign {
    Top,
    Center,
    Bottom,
}

/// Alignment of lines *within* the text block (pango's own alignment).
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineAlign {
    Left,
    Center,
    Right,
}

/// How the text appears.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reveal {
    /// All at once.
    Instant,
    /// Terminal-style, character by character.
    Typewriter {
        /// Characters per second.
        #[serde(default = "d_cps")]
        cps: f64,
        /// Draw a block cursor at the write head.
        #[serde(default = "d_cursor")]
        cursor: bool,
        /// Vary each keystroke gap by up to this fraction. Range: 0–1.
        #[serde(default)]
        jitter: f64,
        /// Keep the current line at the bottom of the block and scroll earlier
        /// lines up. When false, fill from the top. The surface size is fixed
        /// in both modes.
        #[serde(default)]
        scroll: bool,
    },
}

/// Which way a directional effect travels.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Down,
    Up,
}

/// How the text goes away.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Vanish {
    /// Disappear on the frame the hold expires.
    Instant,
    /// Alpha to zero.
    Fade {
        #[serde(default = "d_vanish_ms")]
        ms: u64,
    },
    /// CRT power-off: squash vertically to a bright line, flash, gone.
    Collapse {
        #[serde(default = "d_vanish_ms")]
        ms: u64,
    },
    /// A soft edge sweeps through the text and erases it as it passes.
    Wash {
        #[serde(default = "d_vanish_ms")]
        ms: u64,
        #[serde(default = "d_dir")]
        dir: Dir,
    },
    /// Erase characters in reverse order, with a caret and blips.
    Untype {
        #[serde(default = "d_vanish_ms")]
        ms: u64,
    },
    /// The text falls apart into blocks, in a fixed pseudo-random order.
    Dissolve {
        #[serde(default = "d_vanish_ms")]
        ms: u64,
    },
}

impl Vanish {
    pub fn ms(&self) -> u64 {
        match self {
            Vanish::Instant => 0,
            Vanish::Fade { ms }
            | Vanish::Collapse { ms }
            | Vanish::Wash { ms, .. }
            | Vanish::Untype { ms }
            | Vanish::Dissolve { ms } => *ms,
        }
    }

    /// Effect name used by TOML and CLI specs.
    pub fn kind(&self) -> &'static str {
        match self {
            Vanish::Instant => "instant",
            Vanish::Fade { .. } => "fade",
            Vanish::Collapse { .. } => "collapse",
            Vanish::Wash { .. } => "wash",
            Vanish::Untype { .. } => "untype",
            Vanish::Dissolve { .. } => "dissolve",
        }
    }

    /// Whether the effect drives the visible character count and audio.
    pub fn is_untype(&self) -> bool {
        matches!(self, Vanish::Untype { .. })
    }
}

/// Halo behind the glyphs and outline. `radius = 0` disables inherited glow.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Glow {
    pub color: String,
    /// Blur radius in logical pixels; increases padding.
    pub radius: f64,
    /// Peak opacity of the halo where it leaves the glyph.
    pub alpha: f64,
}

impl Default for Glow {
    fn default() -> Self {
        Glow {
            color: "#b8bb26".to_string(),
            radius: 12.0,
            alpha: 0.55,
        }
    }
}

/// Horizontal gaps cut through the glyphs, their outline and their halo, to
/// the rhythm of a raster scan. `strength = 0` disables an inherited set.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Scanlines {
    /// Distance between gap centres, in DEVICE pixels: the pattern belongs to
    /// the screen, so it does not scale with the font.
    pub period: f64,
    /// How far a gap darkens what is under it, 0 (invisible) to 1 (opaque).
    pub strength: f64,
    /// Fraction of each period the gap covers.
    pub duty: f64,
}

impl Default for Scanlines {
    fn default() -> Self {
        Scanlines {
            period: 4.0,
            strength: 0.35,
            duty: 0.5,
        }
    }
}

/// Typewriter audio parameters, named to match `blyamk`.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Sound {
    pub enabled: bool,
    pub freq: f64,
    pub decay_ms: f64,
    pub gain: f64,
    /// Blip once every N revealed characters (1 = every character).
    pub every: usize,
}

impl Default for Sound {
    fn default() -> Self {
        Sound {
            enabled: true,
            freq: 2100.0,
            decay_ms: 38.0,
            gain: 0.22,
            every: 1,
        }
    }
}

/// Everything that describes one on-screen message except the text itself.
#[cfg_attr(test, derive(serde::Serialize))]
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Style {
    /// Pango font description. Named families must match fontconfig;
    /// unavailable families fall back to the system font.
    pub font: String,
    pub color: String,
    /// Resolved outline colour; `None` disables the stroke.
    pub outline: Option<String>,
    /// Stroke width in logical pixels; defaults to font size / 14.
    pub outline_width: Option<f64>,
    pub halign: HAlign,
    pub valign: VAlign,
    /// Gap from the anchored edge, in logical px. Ignored on a centred axis.
    pub margin: i32,
    /// Width of the text block in logical pixels, excluding padding. `None`
    /// wraps to the monitor and sizes the surface from the measured text.
    pub width: Option<i32>,
    /// Height of the block in lines. `None` sizes the surface from the
    /// message; a reservation keeps it still while the message grows.
    pub lines: Option<usize>,
    pub line_align: LineAlign,
    /// Resolved glow settings; `None` disables glow.
    pub glow: Option<Glow>,
    /// Resolved scanline settings; `None` disables them.
    pub scanlines: Option<Scanlines>,
    /// Hold time after the reveal finishes.
    pub timeout_ms: u64,
    pub reveal: Reveal,
    pub vanish: Vanish,
    pub sound: Sound,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            font: "Monospace 72".to_string(),
            color: "#b8bb26".to_string(),         // gruvbox bright green
            outline: Some("#1d2021".to_string()), // gruvbox bg0_hard
            outline_width: None,
            halign: HAlign::Center,
            valign: VAlign::Center,
            margin: 64,
            width: None,
            lines: None,
            line_align: LineAlign::Left,
            glow: None,
            scanlines: None,
            timeout_ms: 5000,
            reveal: Reveal::Typewriter {
                cps: d_cps(),
                cursor: true,
                jitter: 0.0,
                scroll: false,
            },
            vanish: Vanish::Collapse { ms: d_vanish_ms() },
            sound: Sound::default(),
        }
    }
}

impl Style {
    /// Validate ranges after resolving the selected preset and CLI overrides.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.timeout_ms <= MAX_LIFETIME_MS,
            "timeout_ms is {} but the maximum is {MAX_LIFETIME_MS}",
            self.timeout_ms
        );
        if let Some(w) = self.outline_width {
            anyhow::ensure!(
                (0.0..=MAX_EDGE_PX).contains(&w),
                "outline_width must be between 0 and {MAX_EDGE_PX}, got {w}"
            );
        }
        if let Reveal::Typewriter { jitter, cps, .. } = self.reveal {
            anyhow::ensure!(
                (0.0..=1.0).contains(&jitter),
                "reveal.jitter must be between 0 and 1, got {jitter}"
            );
            // Reject NaN before comparisons; it would invalidate timeline
            // arithmetic.
            anyhow::ensure!(cps.is_finite(), "reveal.cps must be a finite number");
            // Use `Reveal::Instant` to disable typing; zero cps would retain
            // caret behaviour.
            anyhow::ensure!(cps > 0.0, "reveal.cps must be positive, got {cps}");
        }
        anyhow::ensure!(
            self.vanish.ms() <= MAX_LIFETIME_MS,
            "vanish ms is {} but the maximum is {MAX_LIFETIME_MS}",
            self.vanish.ms()
        );
        if let Some(glow) = &self.glow {
            anyhow::ensure!(
                (0.0..=MAX_EDGE_PX).contains(&glow.radius),
                "glow.radius must be between 0 and {MAX_EDGE_PX}, got {}",
                glow.radius
            );
            anyhow::ensure!(
                (0.0..=1.0).contains(&glow.alpha),
                "glow.alpha must be between 0 and 1, got {}",
                glow.alpha
            );
        }
        if let Some(w) = self.width {
            anyhow::ensure!(
                (1..=MAX_BLOCK_PX).contains(&w),
                "width must be between 1 and {MAX_BLOCK_PX}, got {w}"
            );
        }
        if let Some(n) = self.lines {
            anyhow::ensure!(
                (1..=MAX_LINES).contains(&n),
                "lines must be between 1 and {MAX_LINES}, got {n}"
            );
        }
        if let Some(sl) = &self.scanlines {
            // Below two device pixels a gap and its gap-free half share one
            // pixel, which averages to a flat wash rather than a raster.
            anyhow::ensure!(
                (2.0..=MAX_SCANLINE_PERIOD_PX).contains(&sl.period),
                "scanlines.period must be between 2 and {MAX_SCANLINE_PERIOD_PX}, got {}",
                sl.period
            );
            anyhow::ensure!(
                (0.0..=1.0).contains(&sl.strength),
                "scanlines.strength must be between 0 and 1, got {}",
                sl.strength
            );
            // A duty of 1 would erase the text; 0 leaves nothing to see.
            anyhow::ensure!(
                (0.0..1.0).contains(&sl.duty),
                "scanlines.duty must be at least 0 and below 1, got {}",
                sl.duty
            );
        }
        anyhow::ensure!(
            self.sound.every >= 1,
            "sound.every must be at least 1, got {}",
            self.sound.every
        );
        // `decay_ms` sizes the synth buffer. Ranges match blyamk.
        let sound = &self.sound;
        anyhow::ensure!(
            (100.0..=8000.0).contains(&sound.freq),
            "sound.freq must be between 100 and 8000 Hz, got {}",
            sound.freq
        );
        anyhow::ensure!(
            (10.0..=3000.0).contains(&sound.decay_ms),
            "sound.decay_ms must be between 10 and 3000, got {}",
            sound.decay_ms
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&sound.gain),
            "sound.gain must be between 0 and 1, got {}",
            sound.gain
        );
        Ok(())
    }
}

/// The name whose block every other preset inherits from.
const BASE: &str = "default";

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Merge raw tables before deserialisation to distinguish omitted fields
    /// from fields explicitly set to their built-in defaults.
    pub style: HashMap<String, toml::Table>,
}

/// Recursively overlay `over` onto `base`. Replace tables with different
/// `kind` values to avoid retaining fields from another enum variant.
fn merge_into(base: &mut toml::Table, over: &toml::Table) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) if compatible(b, o) => {
                merge_into(b, o)
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn compatible(base: &toml::Table, over: &toml::Table) -> bool {
    match (base.get("kind"), over.get("kind")) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

impl Config {
    /// Read the config. A missing file yields an empty config; malformed files
    /// fail.
    pub fn load(path: Option<PathBuf>) -> Result<Config> {
        let Some(path) = path.or_else(default_path) else {
            return Ok(Config::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // Type-check every merged preset, including inherited enum kinds.
        // Range checks apply only to the selected preset.
        for name in config.style.keys() {
            config
                .merged(name)
                .try_into::<Style>()
                .with_context(|| format!("in [style.{name}] of {}", path.display()))?;
        }
        Ok(config)
    }

    /// Overlay a preset onto the base before filling field defaults.
    fn merged(&self, name: &str) -> toml::Table {
        let mut merged = self.style.get(BASE).cloned().unwrap_or_default();
        if name != BASE
            && let Some(preset) = self.style.get(name)
        {
            merge_into(&mut merged, preset);
        }
        merged
    }

    /// Resolve a named preset over `[style.default]` and built-in defaults.
    /// Unknown names return an error.
    pub fn style(&self, name: &str) -> Result<Style> {
        if name != BASE && !self.style.contains_key(name) {
            anyhow::bail!("no [style.{name}] in the config");
        }
        let style: Style = self
            .merged(name)
            .try_into()
            .with_context(|| format!("in [style.{name}]"))?;
        style
            .validate()
            .with_context(|| format!("in [style.{name}]"))?;
        Ok(style)
    }
}

fn default_path() -> Option<PathBuf> {
    config_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Resolve from the supplied XDG and HOME values. Use XDG only if absolute;
/// otherwise use HOME/.config.
fn config_path(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("wayhud").join("config.toml"))
}

/// Default typing speed, also used when CLI overrides enable typing.
pub const DEFAULT_CPS: f64 = 28.0;

fn d_cps() -> f64 {
    DEFAULT_CPS
}
fn d_cursor() -> bool {
    true
}
/// Default vanish duration when the preset has none.
pub const DEFAULT_VANISH_MS: u64 = 420;

fn d_vanish_ms() -> u64 {
    DEFAULT_VANISH_MS
}
fn d_dir() -> Dir {
    Dir::Down
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_yields_compiled_defaults() {
        let c: Config = toml::from_str("").unwrap();
        let s = c.style("default").unwrap();
        assert_eq!(s.timeout_ms, 5000);
        assert!(matches!(s.reveal, Reveal::Typewriter { .. }));
    }

    #[test]
    fn preset_overrides_only_named_fields() {
        let c: Config = toml::from_str("[style.alert]\ncolor = \"#ff0000\"\n").unwrap();
        let s = c.style("alert").unwrap();
        assert_eq!(s.color, "#ff0000");

        assert_eq!(s.font, Style::default().font);
        assert_eq!(s.timeout_ms, 5000);
    }

    #[test]
    fn presets_inherit_from_style_default() {
        let c: Config = toml::from_str(
            "[style.default]\nfont = \"Sans 40\"\ntimeout_ms = 1234\n\n\
             [style.alert]\ncolor = \"#ff0000\"\n",
        )
        .unwrap();
        let s = c.style("alert").unwrap();
        assert_eq!(s.color, "#ff0000", "own key must win");
        assert_eq!(
            s.font, "Sans 40",
            "unset key must come from [style.default]"
        );
        assert_eq!(s.timeout_ms, 1234);

        assert_eq!(s.margin, Style::default().margin);
    }

    #[test]
    fn a_preset_can_override_a_base_key_back() {
        let c: Config = toml::from_str(
            "[style.default]\ncolor = \"#111111\"\n[style.a]\ncolor = \"#222222\"\n",
        )
        .unwrap();
        assert_eq!(c.style("a").unwrap().color, "#222222");
        assert_eq!(c.style("default").unwrap().color, "#111111");
    }

    #[test]
    fn sub_tables_merge_key_by_key() {
        // Sound fields merge without a variant discriminant.
        let c: Config = toml::from_str(
            "[style.default]\nsound = { enabled = false, freq = 900.0 }\n\
             [style.a]\nsound = { freq = 1500.0 }\n",
        )
        .unwrap();
        let s = c.style("a").unwrap();
        assert_eq!(s.sound.freq, 1500.0);
        assert!(!s.sound.enabled, "enabled must survive from the base");
    }

    #[test]
    fn changing_kind_replaces_the_table_instead_of_mixing_variants() {
        // Changing kind must discard fields specific to the old variant.
        let c: Config = toml::from_str(
            "[style.default]\nvanish = { kind = \"wash\", ms = 700, dir = \"up\" }\n\
             [style.a]\nvanish = { kind = \"collapse\" }\n\
             [style.b]\nvanish = { ms = 250 }\n",
        )
        .unwrap();
        // A different kind resets ms to the built-in default.
        assert_eq!(c.style("a").unwrap().vanish, Vanish::Collapse { ms: 420 });
        // An omitted kind preserves the base variant and its fields.
        assert_eq!(
            c.style("b").unwrap().vanish,
            Vanish::Wash {
                ms: 250,
                dir: Dir::Up
            }
        );
    }

    #[test]
    fn a_misspelled_key_inside_a_reveal_or_vanish_table_is_rejected() {
        // Enum fields need their own `deny_unknown_fields` attribute.
        for table in [
            "reveal = { kind = \"typewriter\", cps = 40, bogus = 1 }",
            "vanish = { kind = \"fade\", ms = 100, bogus = 1 }",
        ] {
            let c: Config = toml::from_str(&format!("[style.a]\n{table}\n")).unwrap();
            assert!(c.style("a").is_err(), "{table} should be rejected");
        }
    }

    /// Write a config to a scratch file and load it the way the binary does.
    fn load_from_text(text: &str, tag: &str) -> Result<Config> {
        let dir = std::env::temp_dir().join(format!("wayhud-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        let result = Config::load(Some(path));
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    #[test]
    fn a_preset_inheriting_its_kind_survives_load() {
        // Validate after merging so presets can inherit `kind`.
        let cfg = load_from_text(
            "[style.default]\nvanish = { kind = \"wash\", ms = 700, dir = \"up\" }\n\
             [style.faster]\nvanish = { ms = 250 }\n",
            "inherit-kind",
        )
        .expect("config with an inherited kind must load");
        assert_eq!(
            cfg.style("faster").unwrap().vanish,
            Vanish::Wash {
                ms: 250,
                dir: Dir::Up
            }
        );
        assert_eq!(
            cfg.style("default").unwrap().vanish,
            Vanish::Wash {
                ms: 700,
                dir: Dir::Up
            }
        );
    }

    #[test]
    fn a_reveal_inheriting_its_kind_survives_load_too() {
        let cfg = load_from_text(
            "[style.default]\nreveal = { kind = \"typewriter\", cps = 12 }\n\
             [style.fast]\nreveal = { cps = 40 }\n",
            "inherit-reveal",
        )
        .expect("config with an inherited reveal kind must load");
        assert!(matches!(
            cfg.style("fast").unwrap().reveal,
            Reveal::Typewriter { cps, .. } if cps == 40.0
        ));
    }

    #[test]
    fn a_typo_in_any_preset_is_caught_at_load_not_at_use() {
        let err = load_from_text("[style.unused]\ncolour = \"#fff\"\n", "typo").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("style.unused"), "unhelpful error: {msg}");
    }

    #[test]
    fn unknown_preset_is_an_error() {
        let c: Config = toml::from_str("[style.alert]\n").unwrap();
        assert!(c.style("alret").is_err());
    }

    #[test]
    fn typo_in_a_field_name_is_rejected() {
        // Raw tables reject unknown fields during resolution and file loading.
        let c: Config = toml::from_str("[style.a]\ncolour = \"#fff\"\n").unwrap();
        assert!(c.style("a").is_err());
    }

    #[test]
    fn jitter_defaults_to_off_and_is_range_checked() {
        let c: Config = toml::from_str("[style.a]\nreveal = { kind = \"typewriter\" }\n").unwrap();
        assert!(matches!(
            c.style("a").unwrap().reveal,
            Reveal::Typewriter { jitter, .. } if jitter == 0.0
        ));
        let c: Config =
            toml::from_str("[style.a]\nreveal = { kind = \"typewriter\", jitter = 1.5 }\n")
                .unwrap();
        assert!(c.style("a").is_err());
    }

    #[test]
    fn scroll_defaults_to_off_and_survives_a_preset_merge() {
        let c: Config = toml::from_str("[style.a]\nreveal = { kind = \"typewriter\" }\n").unwrap();
        assert!(matches!(
            c.style("a").unwrap().reveal,
            Reveal::Typewriter { scroll: false, .. }
        ));
        // Changing scroll must preserve the inherited speed.
        let c: Config = toml::from_str(
            "[style.default]\nreveal = { kind = \"typewriter\", cps = 12 }\n\
             [style.a]\nreveal = { scroll = true }\n",
        )
        .unwrap();
        assert!(matches!(
            c.style("a").unwrap().reveal,
            Reveal::Typewriter { cps, scroll: true, .. } if cps == 12.0
        ));
    }

    #[test]
    fn tagged_enums_round_trip() {
        let c: Config = toml::from_str(
            "[style.a]\nreveal = { kind = \"instant\" }\nvanish = { kind = \"fade\", ms = 100 }\n",
        )
        .unwrap();
        let s = c.style("a").unwrap();
        assert!(matches!(s.reveal, Reveal::Instant));
        assert!(matches!(s.vanish, Vanish::Fade { ms: 100 }));
    }

    #[test]
    fn sound_knobs_are_range_checked() {
        // `decay_ms` controls allocation size.
        for bad in [
            "sound = { decay_ms = 1e9 }",
            "sound = { freq = 0.0 }",
            "sound = { gain = 40.0 }",
        ] {
            let c: Config = toml::from_str(&format!("[style.a]\n{bad}\n")).unwrap();
            assert!(c.style("a").is_err(), "{bad} should be rejected");
        }
        let c: Config =
            toml::from_str("[style.a]\nsound = { freq = 1200.0, decay_ms = 60.0 }\n").unwrap();
        assert!(c.style("a").is_ok());
    }

    #[test]
    fn a_non_finite_cps_is_rejected() {
        let c: Config =
            toml::from_str("[style.a]\nreveal = { kind = \"typewriter\", cps = nan }\n").unwrap();
        assert!(c.style("a").is_err());
    }

    #[test]
    fn a_non_positive_cps_is_rejected_not_silently_instant() {
        // Disabling typing requires `kind = "instant"`.
        for cps in ["-5", "0"] {
            let c: Config = toml::from_str(&format!(
                "[style.a]\nreveal = {{ kind = \"typewriter\", cps = {cps} }}\n"
            ))
            .unwrap();
            assert!(c.style("a").is_err(), "cps = {cps} should be rejected");
        }
    }

    #[test]
    fn an_absurd_vanish_duration_is_rejected() {
        let c: Config =
            toml::from_str("[style.a]\nvanish = { kind = \"fade\", ms = 99999999999999 }\n")
                .unwrap();
        assert!(c.style("a").is_err());
    }

    #[test]
    fn config_cannot_smuggle_an_absurd_timeout_past_the_cli_check() {
        // The lifetime limit also applies to config values.
        let c: Config = toml::from_str("[style.a]\ntimeout_ms = 31536000000\n").unwrap();
        assert!(c.style("a").is_err());
        let c: Config = toml::from_str("[style.a]\ntimeout_ms = 3600000\n").unwrap();
        assert!(c.style("a").is_ok());
    }

    #[test]
    fn scanlines_merge_field_by_field_like_the_other_tables() {
        let c: Config = toml::from_str(
            "[style.default]\nscanlines = { period = 6.0, strength = 0.5 }\n\
             [style.a]\nscanlines = { strength = 0.2 }\n",
        )
        .unwrap();
        let sl = c.style("a").unwrap().scanlines.expect("scanlines");
        assert_eq!(sl.strength, 0.2);
        assert_eq!(sl.period, 6.0, "period must survive from the base");
        assert_eq!(sl.duty, Scanlines::default().duty);
    }

    #[test]
    fn a_preset_takes_back_an_inherited_raster_with_zero_strength() {
        // TOML has no null, the same reason `outline` needs the literal
        // "none" and a glow needs radius = 0.
        let c: Config = toml::from_str(
            "[style.default]\nscanlines = { period = 4.0 }\n\
             [style.a]\nscanlines = { strength = 0.0 }\n",
        )
        .unwrap();
        // The file keeps the table; dropping it is the resolved Hud's job, in
        // `a_zero_strength_raster_is_no_raster_at_all`.
        let sl = c.style("a").unwrap().scanlines.expect("scanlines");
        assert_eq!(sl.strength, 0.0);
        assert_eq!(sl.period, 4.0, "period still merges from the base");
    }

    #[test]
    fn scanline_ranges_are_checked() {
        for spec in [
            "scanlines = { period = 1.0 }",
            "scanlines = { period = 999.0 }",
            "scanlines = { strength = 2.0 }",
            "scanlines = { duty = 1.0 }",
        ] {
            let c: Config = toml::from_str(&format!("[style.a]\n{spec}\n")).unwrap();
            assert!(c.style("a").is_err(), "{spec} passed validation");
        }
    }

    #[test]
    fn config_range_checks_cover_width_and_sound() {
        let c: Config = toml::from_str("[style.a]\noutline_width = -1.0\n").unwrap();
        assert!(c.style("a").is_err());
        let c: Config = toml::from_str("[style.a]\nsound = { every = 0 }\n").unwrap();
        assert!(c.style("a").is_err());
    }

    #[test]
    fn a_broken_preset_does_not_poison_the_one_being_used() {
        let c: Config =
            toml::from_str("[style.bad]\ntimeout_ms = 99999999999\n[style.good]\n").unwrap();
        assert!(c.style("good").is_ok());
        assert!(c.style("bad").is_err());
    }

    #[test]
    fn an_unusable_xdg_config_home_falls_back_to_home() {
        // Empty and relative XDG paths must fall back to HOME.
        let home = || Some(OsString::from("/home/u"));
        let want_home = PathBuf::from("/home/u/.config/wayhud/config.toml");

        assert_eq!(
            config_path(Some(OsString::from("/xdg")), home()),
            Some(PathBuf::from("/xdg/wayhud/config.toml")),
            "an absolute XDG_CONFIG_HOME must win"
        );
        for unusable in ["", "relative/path", "."] {
            assert_eq!(
                config_path(Some(OsString::from(unusable)), home()),
                Some(want_home.clone()),
                "XDG_CONFIG_HOME={unusable:?} must fall back to $HOME"
            );
        }
        assert_eq!(config_path(None, home()), Some(want_home));
        assert_eq!(config_path(None, None), None, "neither set: no path at all");
    }

    #[test]
    fn shipped_example_config_parses() {
        // Keep the shipped example valid under strict parsing.
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).expect("config.example.toml must parse");
        for name in ["default", "alert", "quiet", "spy", "wipe", "boot"] {
            assert!(cfg.style(name).is_ok(), "example lost [style.{name}]");
        }
    }

    #[test]
    fn outline_width_is_optional_and_unset_by_default() {
        let c: Config = toml::from_str("[style.a]\n").unwrap();
        assert_eq!(c.style("a").unwrap().outline_width, None);
        let c: Config = toml::from_str("[style.a]\noutline_width = 2.5\n").unwrap();
        assert_eq!(c.style("a").unwrap().outline_width, Some(2.5));
    }

    #[test]
    fn each_axis_takes_only_its_own_names() {
        // Reject values from the wrong alignment axis.
        let c: Config =
            toml::from_str("[style.a]\nhalign = \"left\"\nvalign = \"bottom\"\n").unwrap();
        let s = c.style("a").unwrap();
        assert_eq!(s.halign, HAlign::Left);
        assert_eq!(s.valign, VAlign::Bottom);

        for bad in [
            "halign = \"top\"",
            "halign = \"start\"",
            "valign = \"left\"",
            "valign = \"end\"",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("[style.a]\n{bad}\n"))
                    .ok()
                    .is_none_or(|c| c.style("a").is_err()),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn wash_carries_a_direction() {
        let c: Config =
            toml::from_str("[style.a]\nvanish = { kind = \"wash\", ms = 300, dir = \"up\" }\n")
                .unwrap();
        assert_eq!(
            c.style("a").unwrap().vanish,
            Vanish::Wash {
                ms: 300,
                dir: Dir::Up
            }
        );
    }

    #[test]
    fn wash_direction_defaults_to_down() {
        let c: Config = toml::from_str("[style.a]\nvanish = { kind = \"wash\" }\n").unwrap();
        assert_eq!(
            c.style("a").unwrap().vanish,
            Vanish::Wash {
                ms: 420,
                dir: Dir::Down
            }
        );
    }

    #[test]
    fn every_vanish_reports_its_duration() {
        // Every timed variant must contribute its duration to the timeline.
        for (toml_kind, want) in [
            ("instant", 0),
            ("fade", 420),
            ("collapse", 420),
            ("wash", 420),
            ("untype", 420),
            ("dissolve", 420),
        ] {
            let c: Config = toml::from_str(&format!(
                "[style.a]\nvanish = {{ kind = \"{toml_kind}\" }}\n"
            ))
            .unwrap();
            assert_eq!(c.style("a").unwrap().vanish.ms(), want, "{toml_kind}");
        }
    }
}
