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

use std::cell::RefCell;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use anyhow::{Context, Result};
use clap::Parser;
use gtk::gio;
use gtk::prelude::*;

use config::{Align, Config, Reveal, Style};
use hud::Hud;
use outputs::OutputSpec;

#[derive(Parser, Debug)]
#[command(
    name = "wayhud",
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

    /// Pango font description, e.g. "LythMono Nerd Font 72".
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

    let hud = Rc::new(Hud::new(style, text)?);

    // Render the whole blip track before the GUI exists: it depends only on
    // the text and the typing speed, and doing it here keeps the first frame
    // from stalling on synthesis.
    let onsets = hud.timeline.onsets(&hud.text, hud.style.sound.every);
    let pcm = RefCell::new(Some(sound::typewriter_track(&hud.style.sound, &onsets)));
    // take() makes this fire exactly once no matter how many windows call it.
    let on_first_frame: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(pcm) = pcm.borrow_mut().take() {
            sound::play_detached(pcm);
        }
    });

    // NON_UNIQUE: without it GTK's single-instance machinery would hand our
    // arguments to an already-running wayhud over D-Bus instead of opening a
    // second overlay, which is exactly the "show both" behaviour we want.
    let app = gtk::Application::new(
        Some("net.x123.wayhud"),
        gio::ApplicationFlags::NON_UNIQUE | gio::ApplicationFlags::HANDLES_COMMAND_LINE,
    );

    let failure = Rc::new(RefCell::new(None::<anyhow::Error>));

    app.connect_startup(|_| {
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
    });

    // We parse argv ourselves with clap; this handler exists only to stop GIO
    // from complaining about the arguments it was handed.
    app.connect_command_line(|app, _| {
        app.activate();
        gtk::glib::ExitCode::SUCCESS
    });

    app.connect_activate({
        let hud = hud.clone();
        let failure = failure.clone();
        let spec = spec.clone();
        move |app| {
            let display = match gtk::gdk::Display::default() {
                Some(d) => d,
                None => {
                    *failure.borrow_mut() = Some(anyhow::anyhow!("no display"));
                    return;
                }
            };
            match outputs::resolve(&display, &spec) {
                Ok(monitors) => {
                    for monitor in monitors {
                        hud::present(app, &monitor, hud.clone(), on_first_frame.clone());
                    }
                }
                Err(e) => *failure.borrow_mut() = Some(e),
            }
        }
    });

    let code = app.run_with_args::<&str>(&[]);
    if let Some(e) = failure.borrow_mut().take() {
        return Err(e);
    }
    Ok(if code == glib_exit_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn glib_exit_ok() -> gtk::glib::ExitCode {
    gtk::glib::ExitCode::SUCCESS
}

/// Text comes from argv, or from stdin when argv is empty or "-".
fn read_text(cli: &Cli) -> Result<String> {
    let from_stdin = match cli.text.as_deref() {
        None | Some("-") => true,
        Some(_) => false,
    };
    if !from_stdin {
        let raw = cli.text.clone().unwrap();
        // Only argv gets unescaped. Text piped in already carries real
        // newlines, and mangling a backslash out of a log line would be rude.
        return Ok(if cli.raw { raw } else { unescape(&raw) });
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
        anyhow::ensure!(t >= 0.0, "--timeout must not be negative");
        style.timeout_ms = (t * 1000.0) as u64;
    }
    if let Some(cps) = cli.typewriter {
        anyhow::ensure!(cps >= 0.0, "--typewriter must not be negative");
        style.reveal = if cps == 0.0 {
            Reveal::Instant
        } else {
            let cursor = match style.reveal {
                Reveal::Typewriter { cursor, .. } => cursor,
                Reveal::Instant => true,
            };
            Reveal::Typewriter { cps, cursor }
        };
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

/// `top-left`, `bottom`, `center`, … in either order.
fn parse_position(s: &str) -> Result<(Align, Align)> {
    let mut h = Align::Center;
    let mut v = Align::Center;
    for part in s.split('-') {
        match part {
            "center" | "centre" => {}
            "left" => h = Align::Start,
            "right" => h = Align::End,
            "top" => v = Align::Start,
            "bottom" => v = Align::End,
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
            (Align::Center, Align::Center)
        );
        assert_eq!(
            parse_position("top-left").unwrap(),
            (Align::Start, Align::Start)
        );
        assert_eq!(
            parse_position("left-top").unwrap(),
            (Align::Start, Align::Start)
        );
        assert_eq!(
            parse_position("bottom").unwrap(),
            (Align::Center, Align::End)
        );
        assert!(parse_position("diagonal").is_err());
    }

    #[test]
    fn typewriter_override_keeps_the_cursor_choice() {
        let mut s = Style {
            reveal: Reveal::Typewriter {
                cps: 10.0,
                cursor: false,
            },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--typewriter", "40"]);
        apply_overrides(&mut s, &cli).unwrap();
        match s.reveal {
            Reveal::Typewriter { cps, cursor } => {
                assert_eq!(cps, 40.0);
                assert!(!cursor, "an unrelated flag must not resurrect the cursor");
            }
            _ => panic!("expected typewriter"),
        }
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
