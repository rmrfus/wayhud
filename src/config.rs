//! Config file + the resolved style a single HUD invocation renders with.
//!
//! Layout on disk is a flat map of named presets:
//!
//! ```toml
//! [style.default]
//! font = "Monospace 72"
//! [style.alert]
//! color = "#ff3355"
//! ```
//!
//! `[style.default]` is the base for every other preset: a key the preset does
//! not set is taken from there, and only then from the compiled-in default.
//! Anything else makes a section named "default" a lie, and forces the shared
//! font to be copy-pasted into every preset in the file.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// One hour. Long enough for anything a heads-up message is for, short enough
/// that a typo cannot strand the overlay on screen — there is no way to
/// dismiss one early.
///
/// Bounds the WHOLE lifetime (reveal + hold + vanish), not just the hold:
/// `--typewriter 0.01` or `--vanish fade:99999999999999` strand the overlay
/// exactly as well as a huge `--timeout` does.
pub const MAX_LIFETIME_MS: u64 = 3_600_000;

/// Where a block of text sits horizontally. Maps onto layer-shell anchors:
/// `Center` means "anchor neither edge", which the compositor centres for us.
///
/// Physical names, not the `start`/`end` of CSS and GTK: those are relative to
/// the writing direction, and this is not — `Left` is `Edge::Left` in an RTL
/// locale too. It also keeps one vocabulary across `--position`, `halign` and
/// `line_align` instead of three.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Where a block of text sits vertically. A separate type from [`HAlign`] so
/// `halign = "top"` is a config error rather than something to puzzle out.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VAlign {
    Top,
    Center,
    Bottom,
}

/// Alignment of lines *within* the text block (pango's own alignment).
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineAlign {
    Left,
    Center,
    Right,
}

/// How the text appears.
#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
        /// Randomise each gap by up to +/- this fraction of it. 0 is a
        /// metronome; 0.3 varies every gap by up to 30% either way. Clamped
        /// to 1.0, past which a gap would want to be negative.
        #[serde(default)]
        jitter: f64,
    },
}

/// Which way a directional effect travels.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Down,
    Up,
}

/// How the text goes away.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    /// Backspace it out: the caret walks back and eats the text, blipping.
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

    /// Untype reveals in reverse, so it drives the caret and the blip track
    /// rather than being a pure paint effect like the others.
    pub fn is_untype(&self) -> bool {
        matches!(self, Vanish::Untype { .. })
    }
}

/// Typewriter blip. Knob names match `blyamk`, so a sound dialled in there
/// with `blyamk -v` transfers over verbatim.
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
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Style {
    /// Pango font description. The default names the fontconfig generic
    /// `Monospace`, which resolves on any box that has fonts at all; a real
    /// family has to match fontconfig exactly, or it falls back to something
    /// else without a word of warning.
    pub font: String,
    pub color: String,
    /// `None` (absent in TOML) means no outline pass at all.
    pub outline: Option<String>,
    /// Stroke width in logical pixels. Left unset it scales with the font
    /// size, which is almost always what you want — see `Hud::outline_width`.
    pub outline_width: Option<f64>,
    pub halign: HAlign,
    pub valign: VAlign,
    /// Gap from the anchored edge, in logical px. Ignored on a centred axis.
    pub margin: i32,
    pub line_align: LineAlign,
    /// Hold time AFTER the reveal finishes — not the total lifetime.
    pub timeout_ms: u64,
    pub reveal: Reveal,
    pub vanish: Vanish,
    pub sound: Sound,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            // A generic, not a family: a HUD that renders in the wrong font
            // on every machine but the author's is not a default.
            font: "Monospace 72".to_string(),
            color: "#b8bb26".to_string(),         // gruvbox bright green
            outline: Some("#1d2021".to_string()), // gruvbox bg0_hard
            outline_width: None,
            halign: HAlign::Center,
            valign: VAlign::Center,
            margin: 64,
            line_align: LineAlign::Left,
            timeout_ms: 5000,
            reveal: Reveal::Typewriter {
                cps: d_cps(),
                cursor: true,
                jitter: 0.0,
            },
            vanish: Vanish::Collapse { ms: d_vanish_ms() },
            sound: Sound::default(),
        }
    }
}

impl Style {
    /// Range checks that serde cannot express. Runs on the preset actually
    /// selected, so an unused broken preset elsewhere in the file is not a
    /// reason to refuse to show a message.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.timeout_ms <= MAX_LIFETIME_MS,
            "timeout_ms is {} but the maximum is {MAX_LIFETIME_MS}",
            self.timeout_ms
        );
        if let Some(w) = self.outline_width {
            anyhow::ensure!(w >= 0.0, "outline_width must not be negative, got {w}");
        }
        if let Reveal::Typewriter { jitter, cps, .. } = self.reveal {
            anyhow::ensure!(
                (0.0..=1.0).contains(&jitter),
                "reveal.jitter must be between 0 and 1, got {jitter}"
            );
            // NaN passes every comparison below and would make the whole
            // timeline NaN, closing the window on the first frame.
            anyhow::ensure!(cps.is_finite(), "reveal.cps must be a finite number");
        }
        anyhow::ensure!(
            self.vanish.ms() <= MAX_LIFETIME_MS,
            "vanish ms is {} but the maximum is {MAX_LIFETIME_MS}",
            self.vanish.ms()
        );
        anyhow::ensure!(
            self.sound.every >= 1,
            "sound.every must be at least 1, got {}",
            self.sound.every
        );
        // The synth allocates its buffer from decay_ms, so an unchecked value
        // is an out-of-memory abort before a single sample is mixed: 1e9 asks
        // for 384 GB. Ranges match blyamk's, which is where these knobs and
        // their calibration come from.
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
    /// Kept as raw tables so a preset can be merged onto the base before any
    /// field defaults are filled in; deserialising to `Style` first would make
    /// "unset" and "set to the compiled default" indistinguishable.
    pub style: HashMap<String, toml::Table>,
}

/// Overlay `over` onto `base`, recursing into sub-tables.
///
/// A sub-table is replaced wholesale rather than merged when both sides carry
/// a `kind` and the two differ: `kind` is the discriminant of a tagged enum,
/// so leftovers from the other variant would be rejected as unknown fields.
/// `vanish = { ms = 300 }` over a `collapse` base still merges — no new kind,
/// nothing to contradict.
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
    /// Read the config, or hand back an empty one if the file isn't there.
    /// A malformed file IS an error — silently falling back to defaults on a
    /// typo means debugging a HUD that ignores half its own settings.
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
        // Type-check every preset now, not just the one that ends up selected:
        // a misspelt key in a preset nobody asked for today is still a typo,
        // and reporting it at load time is the whole point of
        // deny_unknown_fields. Range checks stay per-preset — see `style`.
        //
        // Check the MERGED table, not the raw one: a preset that inherits its
        // `kind` from the base has no `kind` of its own, and the tagged enums
        // would be rejected for a field the merge was about to supply.
        for name in config.style.keys() {
            config
                .merged(name)
                .try_into::<Style>()
                .with_context(|| format!("in [style.{name}] of {}", path.display()))?;
        }
        Ok(config)
    }

    /// Look up a preset. An unknown name is an error, not a silent default:
    /// `--style alret` should say so rather than render the wrong thing.
    /// A preset's keys overlaid on the base's, before defaults are filled in.
    fn merged(&self, name: &str) -> toml::Table {
        let mut merged = self.style.get(BASE).cloned().unwrap_or_default();
        if name != BASE
            && let Some(preset) = self.style.get(name)
        {
            merge_into(&mut merged, preset);
        }
        merged
    }

    /// Resolve a preset: its own keys over `[style.default]`'s, over the
    /// compiled-in defaults.
    ///
    /// An unknown name is an error, not a silent fallback: `--style alret`
    /// should say so rather than quietly render something else.
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

/// Where the config lives, given the two variables.
///
/// Split out from `default_path` so the rules can be exercised without
/// mutating the process environment: `set_var` is unsound once a second thread
/// exists, which is why edition 2024 made it an unsafe fn.
///
/// The XDG spec uses `XDG_CONFIG_HOME` only when it holds an ABSOLUTE path,
/// and that is not pedantry. `XDG_CONFIG_HOME=""` taken at face value becomes
/// `PathBuf::from("")`, so the lookup turns into `./wayhud/config.toml` —
/// relative to whatever directory the process happened to start in, with the
/// real config in $HOME silently ignored. An empty path is not absolute, so
/// one predicate covers both the empty and the relative case.
fn config_path(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("wayhud").join("config.toml"))
}

fn d_cps() -> f64 {
    28.0
}
fn d_cursor() -> bool {
    true
}
/// The compiled-in vanish duration, also used as the fallback when a preset
/// has no duration of its own to carry over.
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
        // Everything else must still be the compiled default, not zeroed.
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
        // And a key neither block sets still falls through to the compiled one.
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
        // sound has no discriminant, so a preset tweaking one knob keeps the
        // rest of the base's sound rather than resetting it.
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
        // Merging key-by-key here would leave `dir` on a collapse and `ms`
        // from a fade, and deny_unknown_fields would reject the result.
        let c: Config = toml::from_str(
            "[style.default]\nvanish = { kind = \"wash\", ms = 700, dir = \"up\" }\n\
             [style.a]\nvanish = { kind = \"collapse\" }\n\
             [style.b]\nvanish = { ms = 250 }\n",
        )
        .unwrap();
        // Different kind: the whole table is replaced, ms falls back to the
        // compiled default rather than inheriting the wash's 700.
        assert_eq!(c.style("a").unwrap().vanish, Vanish::Collapse { ms: 420 });
        // Same (absent) kind: merged, so it stays a wash and keeps dir.
        assert_eq!(
            c.style("b").unwrap().vanish,
            Vanish::Wash {
                ms: 250,
                dir: Dir::Up
            }
        );
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
        // The load-time type check runs before the merge unless it is careful:
        // `vanish = { ms = 250 }` has no `kind` of its own, and rejecting it
        // there made the whole file unloadable — including presets that were
        // perfectly fine.
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
        // deny_unknown_fields: a silently ignored key is worse than a crash.
        // Presets are held as raw tables now, so the rejection happens when
        // one is resolved (and, for a file on disk, at load — see below).
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
        // decay_ms sizes the synth buffer; 1e9 aborted the process.
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
    fn an_absurd_vanish_duration_is_rejected() {
        let c: Config =
            toml::from_str("[style.a]\nvanish = { kind = \"fade\", ms = 99999999999999 }\n")
                .unwrap();
        assert!(c.style("a").is_err());
    }

    #[test]
    fn config_cannot_smuggle_an_absurd_timeout_past_the_cli_check() {
        // --timeout is bounded in main; without this the same value simply
        // moves into the file and pins the overlay to the screen for a year.
        let c: Config = toml::from_str("[style.a]\ntimeout_ms = 31536000000\n").unwrap();
        assert!(c.style("a").is_err());
        let c: Config = toml::from_str("[style.a]\ntimeout_ms = 3600000\n").unwrap();
        assert!(c.style("a").is_ok());
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
        // XDG_CONFIG_HOME="" used to resolve to ./wayhud/config.toml, relative
        // to wherever the process started, while the real config in $HOME went
        // unread. The spec uses the variable only when it is an absolute path.
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
        // README calls this file the reference for every key. With
        // deny_unknown_fields a stale example is an invalid config, and
        // without this test the user finds that out, not CI.
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).expect("config.example.toml must parse");
        for name in ["default", "alert", "quiet", "spy", "wipe"] {
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
        // The whole point of splitting Align in two: one shared enum meant
        // `halign` and `valign` accepted the same three words, and you had to
        // remember which edge "start" was on for which axis.
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
        // ms() feeds the timeline; a variant missing from that match arm would
        // silently animate for zero milliseconds.
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
