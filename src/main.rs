//! wayhud — a layer-shell text overlay for sway.
//!
//! One shot by design: the process shows a message, waits it out, exits. Two
//! concurrent invocations are two processes and two layer surfaces, which the
//! compositor stacks — that is the documented behaviour, not an oversight.

mod config;
mod hud;
mod outputs;
mod sound;
mod synth;
mod timeline;

use std::cell::{Cell, RefCell};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use gtk::glib;

use config::{Config, Dir, HAlign, Reveal, Style, VAlign, Vanish, MAX_LIFETIME_MS};
use hud::Hud;
use outputs::OutputSpec;

#[derive(Parser, Debug)]
#[command(
    name = "wayhud",
    // Straight from Cargo.toml, so the flag cannot drift from the package.
    version,
    about = "Show a heads-up message over everything on sway",
    long_about = None
)]
struct Cli {
    /// Message to show. Omit it, or pass "-", to read stdin.
    text: Option<String>,

    /// Where to show it: current, all, or a comma-separated connector list.
    #[arg(short, long, default_value = "current")]
    output: String,

    /// Hold time in seconds, measured from the END of the reveal.
    #[arg(short, long)]
    timeout: Option<f64>,

    /// Style preset from the config file.
    #[arg(short, long, default_value = "default")]
    style: String,

    /// Pango font description, e.g. "Monospace 72" or "FiraCode Nerd Font 72".
    #[arg(long)]
    font: Option<String>,

    /// Fill colour: any CSS colour GTK understands.
    #[arg(long)]
    color: Option<String>,

    /// Outline colour, or "none" to draw the glyphs flat.
    #[arg(long)]
    outline: Option<String>,

    /// Placement: center, top, bottom, left, right, top-left, bottom-right, …
    #[arg(long)]
    position: Option<String>,

    /// Typewriter speed in characters/second; 0 reveals instantly.
    #[arg(long)]
    typewriter: Option<f64>,

    /// Randomise each keystroke gap by up to +/- this fraction (0..1), so the
    /// typing sounds and looks less like a metronome. 0 disables it.
    #[arg(long)]
    jitter: Option<f64>,

    /// How the text goes away: instant, fade, collapse, wash-down, wash-up,
    /// untype, dissolve. Append ":MS" to set the duration, e.g. "wash-up:700".
    #[arg(long)]
    vanish: Option<String>,

    /// Stay quiet even if the style asks for blips.
    #[arg(long)]
    no_sound: bool,

    /// Take the argument literally: don't expand \n, \t or \\.
    #[arg(long)]
    raw: bool,

    /// Config file (default: $XDG_CONFIG_HOME/wayhud/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wayhud: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let text = read_text(&cli)?;
    let spec = OutputSpec::parse(&cli.output);

    let cfg = Config::load(cli.config.clone())?;
    let mut style = cfg.style(&cli.style)?;
    apply_overrides(&mut style, &cli)?;

    // Jitter is seeded from the clock so repeated messages do not stutter in
    // the same places; tests pass their own seed.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5bd1_e995);
    let hud = Rc::new(Hud::new(style, text, seed)?);
    // reveal + hold + vanish, so a tiny --typewriter strands the overlay no
    // more than a huge --timeout does.
    let total = hud.timeline.total_ms();
    anyhow::ensure!(
        total.is_finite() && total <= MAX_LIFETIME_MS as f64,
        "the message would stay up for {:.0} s; the maximum is {} s \
         (check --typewriter, --timeout and --vanish)",
        total / 1000.0,
        MAX_LIFETIME_MS / 1000
    );

    // Render the whole blip track before the GUI exists: it depends only on
    // the text and the typing speed, and doing it here keeps the first frame
    // from stalling on synthesis.
    let cfg = &hud.style.sound;
    let reveal_pcm = sound::typewriter_track(cfg, &hud.timeline.onsets(cfg.every));
    // Untype clicks its way back out; every other vanish is silent. It is a
    // separate track played after a delay, rather than one track starting at
    // t0: mixing it in would allocate silence for the whole hold, so
    // `--timeout 3600` would cost a gigabyte of zeroes.
    let vanish_pcm = sound::typewriter_track(cfg, &hud.timeline.vanish_onsets(cfg.every));
    let vanish_delay = Duration::from_secs_f64(hud.timeline.vanish_start().max(0.0));
    let tracks = RefCell::new(Some((reveal_pcm, vanish_pcm)));
    // Held so the process can wait for playback instead of killing it on the
    // way out: the last untype blip starts on the very frame the window
    // closes, and PulseAudio drops whatever has not been played.
    let playing: Rc<RefCell<Vec<std::thread::JoinHandle<()>>>> = Rc::new(RefCell::new(Vec::new()));
    // take() makes this fire exactly once no matter how many windows call it.
    let on_first_frame: Rc<dyn Fn()> = Rc::new({
        let playing = playing.clone();
        move || {
            if let Some((reveal, vanish)) = tracks.borrow_mut().take() {
                let mut handles = playing.borrow_mut();
                handles.extend(sound::play_detached(reveal, Duration::ZERO));
                handles.extend(sound::play_detached(vanish, vanish_delay));
            }
        }
    });

    gtk::init().context("initialising GTK")?;
    load_css();

    let display = gtk::gdk::Display::default().context("no display")?;
    let monitors = outputs::resolve(&display, &spec)?;

    // Exit once the last overlay is gone. Counting windows rather than
    // trusting a single timer keeps the loop honest if one output is slower.
    let main_loop = glib::MainLoop::new(None, false);
    let alive = Rc::new(Cell::new(monitors.len()));
    for monitor in monitors {
        hud::present(&monitor, hud.clone(), on_first_frame.clone(), {
            let alive = alive.clone();
            let main_loop = main_loop.clone();
            move || {
                alive.set(alive.get().saturating_sub(1));
                if alive.get() == 0 {
                    main_loop.quit();
                }
            }
        })?;
    }
    main_loop.run();
    for handle in playing.borrow_mut().drain(..) {
        let _ = handle.join();
    }
    Ok(ExitCode::SUCCESS)
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    // The overlay must not paint the theme's window background over the
    // screen; only the glyphs are ours to draw.
    provider.load_from_string("window.wayhud { background: transparent; }");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Text comes from argv, or from stdin when argv is empty or "-".
fn read_text(cli: &Cli) -> Result<String> {
    // Only argv gets unescaped. Text piped in already carries real newlines,
    // and mangling a backslash out of a log line would be rude. Filtering the
    // Option here rather than testing a flag and unwrapping keeps the "argv
    // holds a message" case a single binding that cannot be None.
    if let Some(raw) = cli.text.as_deref().filter(|t| *t != "-") {
        return Ok(if cli.raw {
            raw.to_string()
        } else {
            unescape(raw)
        });
    }
    if std::io::stdin().is_terminal() {
        anyhow::bail!("no text given (pass it as an argument or pipe it in)");
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading stdin")?;
    // A trailing newline from `echo` would otherwise render as a blank line
    // and shove the text off-centre.
    Ok(buf.trim_end_matches('\n').to_string())
}

/// Expand the escapes the shell won't. sway runs `exec` through `sh`, which
/// has no `$'...'`, so a keybinding has no other way to say "second line".
/// An unknown escape is left alone rather than swallowed.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn apply_overrides(style: &mut Style, cli: &Cli) -> Result<()> {
    if let Some(f) = &cli.font {
        style.font = f.clone();
    }
    if let Some(c) = &cli.color {
        style.color = c.clone();
    }
    if let Some(o) = &cli.outline {
        style.outline = if o == "none" { None } else { Some(o.clone()) };
    }
    if let Some(t) = cli.timeout {
        // An upper bound as well as a lower one: the value comes from argv,
        // and there is no way to dismiss a HUD early, so a fat-fingered
        // exponent would pin it to the screen for years.
        let max_s = MAX_LIFETIME_MS as f64 / 1000.0;
        anyhow::ensure!(
            (0.0..=max_s).contains(&t),
            "--timeout must be between 0 and {max_s} seconds"
        );
        style.timeout_ms = (t * 1000.0) as u64;
    }
    if let Some(cps) = cli.typewriter {
        anyhow::ensure!(cps >= 0.0, "--typewriter must not be negative");
        style.reveal = if cps == 0.0 {
            Reveal::Instant
        } else {
            // Carry the preset's other typewriter settings across; only the
            // speed was asked about.
            let (cursor, jitter) = match style.reveal {
                Reveal::Typewriter { cursor, jitter, .. } => (cursor, jitter),
                Reveal::Instant => (true, 0.0),
            };
            Reveal::Typewriter {
                cps,
                cursor,
                jitter,
            }
        };
    }
    if let Some(j) = cli.jitter {
        anyhow::ensure!((0.0..=1.0).contains(&j), "--jitter must be between 0 and 1");
        match &mut style.reveal {
            Reveal::Typewriter { jitter, .. } => *jitter = j,
            // Nothing to stagger, but silently ignoring a flag is worse than
            // saying why it cannot apply.
            Reveal::Instant => {
                anyhow::bail!("--jitter needs a typewriter reveal; pass --typewriter too")
            }
        }
    }
    if let Some(v) = &cli.vanish {
        // Carry the configured duration over when the flag doesn't state one,
        // so switching effect on the CLI doesn't silently reset the timing.
        style.vanish = parse_vanish(v, style.vanish.ms())?;
    }
    if cli.no_sound {
        style.sound.enabled = false;
    }
    if let Some(p) = &cli.position {
        let (h, v) = parse_position(p)?;
        style.halign = h;
        style.valign = v;
    }
    Ok(())
}

/// `wash-up`, `fade:250`, `collapse`, …
fn parse_vanish(spec: &str, fallback_ms: u64) -> Result<Vanish> {
    let (kind, ms) = match spec.split_once(':') {
        Some((k, m)) => (
            k,
            m.parse::<u64>()
                .with_context(|| format!("bad duration {m:?} in --vanish"))?,
        ),
        // An instant preset has no duration to keep, so fall back to the
        // compiled default rather than to a 1 ms flicker.
        None if fallback_ms == 0 => (spec, config::DEFAULT_VANISH_MS),
        None => (spec, fallback_ms),
    };
    anyhow::ensure!(
        ms <= MAX_LIFETIME_MS,
        "--vanish duration must be at most {MAX_LIFETIME_MS} ms"
    );
    Ok(match kind {
        "instant" | "none" => Vanish::Instant,
        "fade" => Vanish::Fade { ms },
        "collapse" | "crt" => Vanish::Collapse { ms },
        "wash" | "wash-down" => Vanish::Wash { ms, dir: Dir::Down },
        "wash-up" => Vanish::Wash { ms, dir: Dir::Up },
        "untype" => Vanish::Untype { ms },
        "dissolve" => Vanish::Dissolve { ms },
        other => anyhow::bail!(
            "unknown --vanish {other:?} (want instant, fade, collapse, \
             wash-down, wash-up, untype or dissolve)"
        ),
    })
}

/// `top-left`, `bottom`, `center`, … in either order.
fn parse_position(s: &str) -> Result<(HAlign, VAlign)> {
    let mut h = HAlign::Center;
    let mut v = VAlign::Center;
    for part in s.split('-') {
        match part {
            "center" | "centre" => {}
            "left" => h = HAlign::Left,
            "right" => h = HAlign::Right,
            "top" => v = VAlign::Top,
            "bottom" => v = VAlign::Bottom,
            other => anyhow::bail!(
                "bad --position component {other:?} \
                 (want center/top/bottom/left/right)"
            ),
        }
    }
    Ok((h, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_parse_in_any_order() {
        assert_eq!(
            parse_position("center").unwrap(),
            (HAlign::Center, VAlign::Center)
        );
        assert_eq!(
            parse_position("top-left").unwrap(),
            (HAlign::Left, VAlign::Top)
        );
        assert_eq!(
            parse_position("left-top").unwrap(),
            (HAlign::Left, VAlign::Top)
        );
        assert_eq!(
            parse_position("bottom").unwrap(),
            (HAlign::Center, VAlign::Bottom)
        );
        assert!(parse_position("diagonal").is_err());
    }

    #[test]
    fn typewriter_override_keeps_the_cursor_choice() {
        let mut s = Style {
            reveal: Reveal::Typewriter {
                cps: 10.0,
                cursor: false,
                jitter: 0.0,
            },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--typewriter", "40"]);
        apply_overrides(&mut s, &cli).unwrap();
        match s.reveal {
            Reveal::Typewriter { cps, cursor, .. } => {
                assert_eq!(cps, 40.0);
                assert!(!cursor, "an unrelated flag must not resurrect the cursor");
            }
            _ => panic!("expected typewriter"),
        }
    }

    #[test]
    fn vanish_flag_keeps_the_configured_duration() {
        let mut s = Style {
            vanish: Vanish::Fade { ms: 900 },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--vanish", "wash-up"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert_eq!(
            s.vanish,
            Vanish::Wash {
                ms: 900,
                dir: Dir::Up
            }
        );
    }

    #[test]
    fn vanish_flag_can_state_its_own_duration() {
        let mut s = Style::default();
        let cli = Cli::parse_from(["wayhud", "x", "--vanish", "dissolve:250"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert_eq!(s.vanish, Vanish::Dissolve { ms: 250 });
    }

    #[test]
    fn unknown_vanish_is_rejected_not_ignored() {
        assert!(parse_vanish("sparkle", 400).is_err());
        assert!(parse_vanish("fade:abc", 400).is_err());
    }

    #[test]
    fn instant_vanish_survives_the_fallback_clamp() {
        // fallback_ms.max(1) must not turn an explicit "instant" into a 1 ms
        // animation with a duration nobody asked for.
        assert_eq!(parse_vanish("instant", 0).unwrap(), Vanish::Instant);
    }

    #[test]
    fn jitter_flag_needs_a_typewriter() {
        let mut s = Style {
            reveal: Reveal::Instant,
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--jitter", "0.3"]);
        assert!(apply_overrides(&mut s, &cli).is_err());
    }

    #[test]
    fn jitter_flag_applies_and_is_range_checked() {
        let mut s = Style::default();
        let cli = Cli::parse_from(["wayhud", "x", "--jitter", "0.4"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(matches!(s.reveal, Reveal::Typewriter { jitter, .. } if jitter == 0.4));

        let mut s = Style::default();
        let cli = Cli::parse_from(["wayhud", "x", "--jitter", "2"]);
        assert!(apply_overrides(&mut s, &cli).is_err());
    }

    #[test]
    fn typewriter_override_keeps_the_jitter() {
        let mut s = Style {
            reveal: Reveal::Typewriter {
                cps: 10.0,
                cursor: true,
                jitter: 0.5,
            },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--typewriter", "40"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(matches!(s.reveal, Reveal::Typewriter { jitter, .. } if jitter == 0.5));
    }

    #[test]
    fn zero_typewriter_means_instant() {
        let mut s = Style::default();
        let cli = Cli::parse_from(["wayhud", "x", "--typewriter", "0"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(matches!(s.reveal, Reveal::Instant));
    }

    #[test]
    fn outline_none_disables_the_stroke() {
        let mut s = Style::default();
        let cli = Cli::parse_from(["wayhud", "x", "--outline", "none"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(s.outline.is_none());
    }

    #[test]
    fn vanish_falls_back_to_the_compiled_duration_over_an_instant_preset() {
        // fallback_ms.max(1) used to turn "no duration to inherit" into a 1 ms
        // animation nobody could see.
        let mut s = Style {
            vanish: Vanish::Instant,
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--vanish", "fade"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert_eq!(
            s.vanish,
            Vanish::Fade {
                ms: config::DEFAULT_VANISH_MS
            }
        );
    }

    #[test]
    fn an_absurd_vanish_duration_is_rejected_on_the_cli() {
        assert!(parse_vanish("fade:99999999999999", 400).is_err());
    }

    #[test]
    fn absurd_timeouts_are_rejected_at_the_edge() {
        let mut s = Style::default();
        // 1e9 seconds used to reach the mixer and try to allocate the silence.
        let cli = Cli::parse_from(["wayhud", "x", "--timeout", "1e9"]);
        assert!(apply_overrides(&mut s, &cli).is_err());
        let cli = Cli::parse_from(["wayhud", "x", "--timeout", "3600"]);
        assert!(apply_overrides(&mut s, &cli).is_ok());
    }

    #[test]
    fn escapes_expand_but_unknown_ones_survive() {
        assert_eq!(unescape("a\\nb"), "a\nb");
        assert_eq!(unescape("a\\tb"), "a\tb");
        assert_eq!(unescape("a\\\\nb"), "a\\nb");
        // A Windows-ish path must not lose its backslash to a silent drop.
        assert_eq!(unescape("C:\\dir"), "C:\\dir");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn raw_mode_keeps_the_text_verbatim() {
        let cli = Cli::parse_from(["wayhud", "a\\nb", "--raw"]);
        assert_eq!(read_text(&cli).unwrap(), "a\\nb");
    }

    #[test]
    fn negative_timeout_is_rejected() {
        let mut s = Style::default();
        // Must use `=`: clap reads a bare `-3` as a flag, not a value.
        let cli = Cli::parse_from(["wayhud", "x", "--timeout=-3"]);
        assert!(apply_overrides(&mut s, &cli).is_err());
    }
}
