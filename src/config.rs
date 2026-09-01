//! Config file + the resolved style a single HUD invocation renders with.
//!
//! Layout on disk is a flat map of named presets:
//!
//! ```toml
//! [style.default]
//! font = "LythMono Nerd Font 72"
//! [style.alert]
//! color = "#ff3355"
//! ```
//!
//! Presets do NOT inherit from each other — every field falls back to the
//! compiled-in default instead. Inheritance is a second mechanism to hold in
//! your head at 3am for no gain: a preset is fully described by its own block.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Where a block of text sits along one axis. Maps onto layer-shell anchors:
/// `Center` means "anchor neither edge", which the compositor centres for us.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Start,
    Center,
    End,
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
        #[serde(default = "d_true")]
        cursor: bool,
    },
}

/// How the text goes away.
#[derive(Deserialize, Clone, Debug)]
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
    /// Pango font description. Note the family is `LythMono Nerd Font`, not
    /// `Lyth Mono` — fontconfig silently falls back to DejaVu on the latter.
    pub font: String,
    pub color: String,
    /// `None` (absent in TOML) means no outline pass at all.
    pub outline: Option<String>,
    pub outline_width: f64,
    pub halign: Align,
    pub valign: Align,
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
            font: "LythMono Nerd Font 72".to_string(),
            color: "#b8bb26".to_string(),         // gruvbox bright green
            outline: Some("#1d2021".to_string()), // gruvbox bg0_hard
            outline_width: 5.0,
            halign: Align::Center,
            valign: Align::Center,
            margin: 64,
            line_align: LineAlign::Left,
            timeout_ms: 5000,
            reveal: Reveal::Typewriter {
                cps: d_cps(),
                cursor: true,
            },
            vanish: Vanish::Collapse { ms: d_vanish_ms() },
            sound: Sound::default(),
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub style: HashMap<String, Style>,
}

impl Config {
    /// Read the config, or hand back an empty one if the file isn't there.
    /// A malformed file IS an error — silently falling back to defaults on a
    /// typo means debugging a HUD that ignores half its own settings.
    pub fn load(path: Option<PathBuf>) -> Result<Config> {
        let path = match path.or_else(default_path) {
            Some(p) => p,
            None => return Ok(Config::default()),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Look up a preset. An unknown name is an error, not a silent default:
    /// `--style alret` should say so rather than render the wrong thing.
    pub fn style(&self, name: &str) -> Result<Style> {
        match self.style.get(name) {
            Some(s) => Ok(s.clone()),
            None if name == "default" => Ok(Style::default()),
            None => anyhow::bail!("no [style.{name}] in the config"),
        }
    }
}

fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("wayhud").join("config.toml"))
}

fn d_cps() -> f64 {
    28.0
}
fn d_true() -> bool {
    true
}
fn d_vanish_ms() -> u64 {
    420
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
    fn unknown_preset_is_an_error() {
        let c: Config = toml::from_str("[style.alert]\n").unwrap();
        assert!(c.style("alret").is_err());
    }

    #[test]
    fn typo_in_a_field_name_is_rejected() {
        // deny_unknown_fields: a silently ignored key is worse than a crash.
        assert!(toml::from_str::<Config>("[style.a]\ncolour = \"#fff\"\n").is_err());
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
}
