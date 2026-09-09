//! wayhud — a layer-shell text overlay for sway.
//!
//! One shot by design: the process shows a message, waits it out, exits. Two
//! concurrent invocations are two processes and two layer surfaces, which the
//! compositor stacks — that is the documented behaviour, not an oversight.

mod config;
mod hud;
mod outputs;
mod sound;
mod spec;
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

use config::{
    Config, Dir, Glow, HAlign, LineAlign, MAX_LIFETIME_MS, MAX_TEXT_CHARS, Reveal, Sound, Style,
    VAlign, Vanish,
};
use hud::Hud;
use outputs::OutputSpec;
use spec::Spec;

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

    /// Outline: a colour, or "none". Takes width=PX.
    /// E.g. "#1d2021" or "#1d2021,width=5".
    #[arg(long)]
    outline: Option<String>,

    /// Halo behind the glyphs, painted under the outline rather than instead
    /// of it: a colour, or "none". Takes radius=PX and alpha=0..1.
    /// E.g. "#b8bb26,radius=12,alpha=0.7".
    #[arg(long)]
    glow: Option<String>,

    /// Placement: center, top, bottom-right, … Takes halign= and valign=.
    #[arg(long)]
    position: Option<String>,

    /// Gap from the anchored edge, in logical pixels. A centred axis ignores it.
    #[arg(long)]
    margin: Option<i32>,

    /// Alignment of lines within the block: left, center, right.
    #[arg(long)]
    line_align: Option<String>,

    /// How the text appears: "instant", or "typewriter". Takes cps=,
    /// cursor=, jitter= and scroll=. E.g. "typewriter,cps=50,scroll=true".
    #[arg(long)]
    reveal: Option<String>,

    /// How the text goes away: instant, fade, collapse, wash, untype,
    /// dissolve. Takes ms= and, for wash, dir=up|down.
    /// E.g. "wash,dir=up,ms=700".
    #[arg(long)]
    vanish: Option<String>,

    /// The typewriter blip: "on", "off", or the knobs freq=, decay_ms=,
    /// gain= and every=. E.g. "freq=1800,gain=0.3".
    #[arg(long)]
    sound: Option<String>,

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
    // The same ranges the config file is held to. Without this the flags would
    // each have to restate them, which is how a bound ends up enforced on one
    // path and not the other.
    style
        .validate()
        .context("in the style built from the command line")?;

    // Jitter is seeded from the clock so repeated messages do not stutter in
    // the same places; tests pass their own seed.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5bd1_e995);
    let hud = Rc::new(Hud::new(style, text, seed)?);
    // reveal + hold + vanish, so a tiny --reveal cps= strands the overlay no
    // more than a huge --timeout does.
    let total = hud.timeline.total_ms();
    anyhow::ensure!(
        total.is_finite() && total <= MAX_LIFETIME_MS as f64,
        "the message would stay up for {:.0} s; the maximum is {} s \
         (check --reveal, --timeout and --vanish)",
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
    // The display takes its own reference to the provider, which is why this
    // local can die at the end of the function and the rule still applies for
    // the life of the process. Both are refcounted GObjects, so the order the
    // two are dropped in carries no meaning — worth knowing, because the
    // edition 2024 migration lint flags exactly that order changing here.
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Read a message from `r`, stopping before an endless stream fills memory.
///
/// `Read::take` bounds bytes while the cap counts characters, so the budget is
/// the cap's worst case in UTF-8 plus one: MAX_TEXT_CHARS characters occupy at
/// most four times as many bytes, so one byte past that proves the message is
/// over the limit whatever it encodes. Stopping there is the whole point — a
/// character count can only speak once the stream is already resident, which
/// is too late for a pipe that never ends.
fn read_capped(r: impl Read) -> Result<String> {
    const BUDGET: u64 = MAX_TEXT_CHARS as u64 * 4 + 1;
    let mut bytes = Vec::new();
    r.take(BUDGET)
        .read_to_end(&mut bytes)
        .context("reading stdin")?;
    // Filling the budget means the input did not end inside it. This has to
    // answer before the conversion below, not after: a tail cut at an
    // arbitrary byte need not be valid UTF-8, and "not valid UTF-8" is the
    // wrong thing to tell someone whose only mistake was piping too much.
    anyhow::ensure!(
        (bytes.len() as u64) < BUDGET,
        "message is longer than the maximum of {MAX_TEXT_CHARS} characters"
    );
    String::from_utf8(bytes).context("input is not valid UTF-8")
}

/// Text comes from argv, or from stdin when argv is empty or "-".
fn read_text(cli: &Cli) -> Result<String> {
    // Only argv gets unescaped. Text piped in already carries real newlines,
    // and mangling a backslash out of a log line would be rude. Filtering the
    // Option here rather than testing a flag and unwrapping keeps the "argv
    // holds a message" case a single binding that cannot be None.
    let text = if let Some(raw) = cli.text.as_deref().filter(|t| *t != "-") {
        if cli.raw {
            raw.to_string()
        } else {
            unescape(raw)
        }
    } else {
        if std::io::stdin().is_terminal() {
            anyhow::bail!("no text given (pass it as an argument or pipe it in)");
        }
        read_capped(std::io::stdin())?
    };
    // Whichever way the text arrived. Pango turns a trailing newline into an
    // empty final line and counts it in the layout height, so the surface ends
    // up a line taller than anything visible: the compositor centres that
    // phantom along with the text and the whole message sits half a line off.
    // The caret then finishes the reveal parked at x=0 on that empty line,
    // adrift from the message it belongs to.
    //
    // Trimming only the piped side, which is what this did, made
    // `wayhud "a\n"` and `printf 'a\n' | wayhud` render differently for no
    // reason a user could see.
    let text = text.trim_end_matches('\n');
    // Bound the text itself, not just its lifetime: the check in run() fires
    // after the step table and the layout are already allocated.
    let count = text.chars().count();
    anyhow::ensure!(
        count <= MAX_TEXT_CHARS,
        "message is {count} characters; the maximum is {MAX_TEXT_CHARS}"
    );
    Ok(text.to_string())
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

/// Lay the command line over a resolved preset.
///
/// Every flag reads the style it is overriding, so a spec states only what it
/// changes: `--glow 'radius=20'` keeps the preset's colour and alpha. Ranges
/// are not checked here — `run` validates the finished style with the same
/// code the config file goes through, so a bound cannot hold in one place and
/// not the other.
fn apply_overrides(style: &mut Style, cli: &Cli) -> Result<()> {
    if let Some(f) = &cli.font {
        style.font = f.clone();
    }
    if let Some(c) = &cli.color {
        style.color = c.clone();
    }
    if let Some(o) = &cli.outline {
        let (colour, width) = parse_outline(o, style.outline.as_deref(), style.outline_width)?;
        style.outline = colour;
        style.outline_width = width;
    }
    if let Some(g) = &cli.glow {
        style.glow = parse_glow(g, style.glow.as_ref())?;
    }
    if let Some(t) = cli.timeout {
        // Checked here rather than left to `validate`: seconds are this flag's
        // own unit, and a negative one saturates to 0 on the way to
        // `timeout_ms` instead of arriving as something to complain about.
        let max_s = MAX_LIFETIME_MS as f64 / 1000.0;
        anyhow::ensure!(
            (0.0..=max_s).contains(&t),
            "--timeout must be between 0 and {max_s} seconds"
        );
        style.timeout_ms = (t * 1000.0) as u64;
    }
    if let Some(p) = &cli.position {
        let (h, v) = parse_position(p, style)?;
        style.halign = h;
        style.valign = v;
    }
    if let Some(m) = cli.margin {
        style.margin = m;
    }
    if let Some(a) = &cli.line_align {
        style.line_align = parse_line_align(a)?;
    }
    if let Some(r) = &cli.reveal {
        style.reveal = parse_reveal(r, &style.reveal)?;
    }
    if let Some(v) = &cli.vanish {
        style.vanish = parse_vanish(v, &style.vanish)?;
    }
    if let Some(snd) = &cli.sound {
        style.sound = parse_sound(snd, &style.sound)?;
    }
    Ok(())
}

/// `bottom-left`, `center`, or `halign=left,valign=bottom`.
///
/// A bare word still names BOTH axes: `bottom` is bottom-centre, not "bottom
/// and whatever the preset said". Keeping the other axis would make the same
/// flag mean different things depending on which preset it was used with.
fn parse_position(spec: &str, style: &Style) -> Result<(HAlign, VAlign)> {
    let mut s = Spec::parse("--position", spec)?;
    let (mut halign, mut valign) = match s.bare() {
        Some(word) => parse_position_word(word)?,
        None => (style.halign, style.valign),
    };
    if let Some(h) = s.take("halign") {
        halign = parse_halign(h)?;
    }
    if let Some(v) = s.take("valign") {
        valign = parse_valign(v)?;
    }
    s.finish()?;
    Ok((halign, valign))
}

/// `top-left`, `bottom`, `center`, … in either order.
fn parse_position_word(word: &str) -> Result<(HAlign, VAlign)> {
    let mut h = HAlign::Center;
    let mut v = VAlign::Center;
    for part in word.split('-') {
        match part {
            "center" => {}
            // Taken before 1.0, when the flag had a vocabulary of its own; the
            // config file never took it. Refusing it keeps the two spellings
            // from diverging again, but the answer has to be the word to type
            // rather than a list of five in which one differs by a letter.
            "centre" => anyhow::bail!(
                "--position: spelled \"center\" here, the way the config file \
                 spells it"
            ),
            "left" => h = HAlign::Left,
            "right" => h = HAlign::Right,
            "top" => v = VAlign::Top,
            "bottom" => v = VAlign::Bottom,
            other => anyhow::bail!(
                "--position: bad component {other:?} \
                 (want center, top, bottom, left or right)"
            ),
        }
    }
    Ok((h, v))
}

fn parse_halign(v: &str) -> Result<HAlign> {
    Ok(match v {
        "left" => HAlign::Left,
        "center" => HAlign::Center,
        "right" => HAlign::Right,
        other => anyhow::bail!("bad halign {other:?} (want left, center or right)"),
    })
}

fn parse_valign(v: &str) -> Result<VAlign> {
    Ok(match v {
        "top" => VAlign::Top,
        "center" => VAlign::Center,
        "bottom" => VAlign::Bottom,
        other => anyhow::bail!("bad valign {other:?} (want top, center or bottom)"),
    })
}

fn parse_line_align(v: &str) -> Result<LineAlign> {
    Ok(match v {
        "left" => LineAlign::Left,
        "center" => LineAlign::Center,
        "right" => LineAlign::Right,
        other => anyhow::bail!("bad --line-align {other:?} (want left, center or right)"),
    })
}

fn parse_dir(v: &str) -> Result<Dir> {
    Ok(match v {
        "up" => Dir::Up,
        "down" => Dir::Down,
        other => anyhow::bail!("--vanish: bad dir {other:?} (want up or down)"),
    })
}

/// `#1d2021`, `#1d2021,width=5`, `none`.
///
/// Returns `(colour, width)`. A field the spec does not mention keeps the
/// preset's value, which is what lets a colour be tried without restating the
/// geometry — the contract every one of these flags shares.
fn parse_outline(
    spec: &str,
    current_colour: Option<&str>,
    current_width: Option<f64>,
) -> Result<(Option<String>, Option<f64>)> {
    let mut s = Spec::parse("--outline", spec)?;
    let colour = s.headline("color")?;
    let width = s.take_parsed::<f64>("width")?;
    s.finish()?;
    if colour == Some("none") {
        // There is no stroke to size, so a width here is a typo rather than a
        // setting. Bare "none" still carries the configured width through
        // untouched, for a preset that switches the outline back on.
        anyhow::ensure!(
            width.is_none(),
            "--outline none takes no width; there is no stroke to size"
        );
        return Ok((None, current_width));
    }
    // The preset's colour has to be passed in, because `None` here would
    // otherwise mean both "no outline" and "nothing was said about it" —
    // and `--outline 'width=2'` would switch the stroke off while claiming
    // to resize it.
    Ok((
        colour.map_or_else(
            || current_colour.map(str::to_string),
            |c| Some(c.to_string()),
        ),
        width.or(current_width),
    ))
}

/// `#b8bb26`, `#b8bb26,radius=12,alpha=0.7`, `none`.
fn parse_glow(spec: &str, current: Option<&Glow>) -> Result<Option<Glow>> {
    let mut s = Spec::parse("--glow", spec)?;
    let colour = s.headline("color")?;
    let radius = s.take_parsed::<f64>("radius")?;
    let alpha = s.take_parsed::<f64>("alpha")?;
    s.finish()?;
    if colour == Some("none") {
        anyhow::ensure!(
            radius.is_none() && alpha.is_none(),
            "--glow none takes no other fields; there is no halo to describe"
        );
        return Ok(None);
    }
    // Falling back to the compiled default rather than refusing means
    // `--glow 'radius=20'` over a preset with no halo turns one on at the
    // default colour, instead of naming a colour to change a radius.
    let base = current.cloned().unwrap_or_default();
    Ok(Some(Glow {
        color: colour.map_or(base.color, str::to_string),
        radius: radius.unwrap_or(base.radius),
        alpha: alpha.unwrap_or(base.alpha),
    }))
}

/// `instant`, `typewriter,cps=50`, `cps=50,scroll=true`.
fn parse_reveal(spec: &str, current: &Reveal) -> Result<Reveal> {
    let mut s = Spec::parse("--reveal", spec)?;
    let kind = s.headline("kind")?;
    let cps = s.take_parsed::<f64>("cps")?;
    let cursor = s.take_bool("cursor")?;
    let jitter = s.take_parsed::<f64>("jitter")?;
    let scroll = s.take_bool("scroll")?;
    s.finish()?;

    let typed = cps.is_some() || cursor.is_some() || jitter.is_some() || scroll.is_some();
    match kind {
        Some("instant") => {
            anyhow::ensure!(
                !typed,
                "--reveal instant takes no other fields; cps, cursor, jitter \
                 and scroll all belong to the typewriter"
            );
            Ok(Reveal::Instant)
        }
        Some("typewriter") | None => {
            // What the spec leaves out comes from the preset, so
            // `--reveal 'cps=50'` changes the speed and nothing else.
            let (base_cps, base_cursor, base_jitter, base_scroll) = match *current {
                Reveal::Typewriter {
                    cps,
                    cursor,
                    jitter,
                    scroll,
                } => (cps, cursor, jitter, scroll),
                // Nothing to inherit. Saying so beats silently typing at a
                // speed nobody chose.
                Reveal::Instant => {
                    anyhow::ensure!(
                        kind.is_some(),
                        "--reveal: the preset reveals instantly, so there is \
                         nothing to adjust; say typewriter to switch it on"
                    );
                    (config::DEFAULT_CPS, true, 0.0, false)
                }
            };
            Ok(Reveal::Typewriter {
                cps: cps.unwrap_or(base_cps),
                cursor: cursor.unwrap_or(base_cursor),
                jitter: jitter.unwrap_or(base_jitter),
                scroll: scroll.unwrap_or(base_scroll),
            })
        }
        Some(other) => {
            anyhow::bail!("--reveal: unknown kind {other:?} (want instant or typewriter)")
        }
    }
}

/// `fade`, `wash,dir=up,ms=700`, `kind=collapse,ms=250`.
fn parse_vanish(spec: &str, current: &Vanish) -> Result<Vanish> {
    let mut s = Spec::parse("--vanish", spec)?;
    let kind = s.headline("kind")?;
    let ms = s.take_parsed::<u64>("ms")?;
    let dir_word = s.take("dir");
    let dir = dir_word.map(parse_dir).transpose()?;
    s.finish()?;

    let kind = kind.unwrap_or_else(|| current.kind());
    // Instant has no timing at all, so this runs before the duration is
    // resolved: a stated ms here is a typo, and the answer must be that the
    // effect takes no duration rather than that the duration is wrong.
    if kind == "instant" {
        anyhow::ensure!(
            ms.is_none() && dir.is_none(),
            "--vanish instant takes no other fields; it happens on one frame"
        );
        return Ok(Vanish::Instant);
    }
    // An instant preset has no duration worth keeping, so fall back to the
    // compiled default rather than to a 0 ms flicker.
    let ms = ms.unwrap_or(match current.ms() {
        0 => config::DEFAULT_VANISH_MS,
        keep => keep,
    });
    if kind != "wash" {
        // Refused rather than taken to mean wash: a spec that silently
        // switches the effect because of one field is how a keybinding ends up
        // doing something nobody wrote down. The kind is one word away, so
        // hand it over instead of only saying no.
        anyhow::ensure!(
            dir.is_none(),
            "--vanish: dir only applies to wash, not to {kind}; \
             say --vanish 'wash,dir={}' to switch the effect too",
            dir_word.unwrap_or("up")
        );
    }
    Ok(match kind {
        "fade" => Vanish::Fade { ms },
        "collapse" => Vanish::Collapse { ms },
        "untype" => Vanish::Untype { ms },
        "dissolve" => Vanish::Dissolve { ms },
        "wash" => Vanish::Wash {
            ms,
            // Carried over only from another wash; every other kind has no
            // direction to inherit.
            dir: dir.unwrap_or(match *current {
                Vanish::Wash { dir, .. } => dir,
                _ => Dir::Down,
            }),
        },
        other => anyhow::bail!(
            "--vanish: unknown kind {other:?} (want instant, fade, collapse, \
             wash, untype or dissolve)"
        ),
    })
}

/// `on`, `off`, `freq=1800,gain=0.3`.
fn parse_sound(spec: &str, current: &Sound) -> Result<Sound> {
    let mut s = Spec::parse("--sound", spec)?;
    let enabled = s.headline("enabled")?;
    let freq = s.take_parsed::<f64>("freq")?;
    let decay_ms = s.take_parsed::<f64>("decay_ms")?;
    let gain = s.take_parsed::<f64>("gain")?;
    let every = s.take_parsed::<usize>("every")?;
    s.finish()?;
    Ok(Sound {
        enabled: match enabled {
            None => current.enabled,
            // "on"/"off" bare, "true"/"false" by name: the first is what one
            // types, the second is what the config file says.
            Some("on" | "true") => true,
            Some("off" | "false") => false,
            Some(other) => anyhow::bail!("--sound: {other:?} is not on or off"),
        },
        freq: freq.unwrap_or(current.freq),
        decay_ms: decay_ms.unwrap_or(current.decay_ms),
        gain: gain.unwrap_or(current.gain),
        every: every.unwrap_or(current.every),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_word_still_names_both_axes() {
        let pos = |spec| {
            let mut s = Style::default();
            let cli = Cli::parse_from(["wayhud", "x", "--position", spec]);
            apply_overrides(&mut s, &cli).unwrap();
            (s.halign, s.valign)
        };
        assert_eq!(pos("center"), (HAlign::Center, VAlign::Center));
        assert_eq!(pos("top-left"), (HAlign::Left, VAlign::Top));
        assert_eq!(pos("left-top"), (HAlign::Left, VAlign::Top));
        // Not "bottom, keeping whatever the preset said about the other axis":
        // the same flag would then mean different things per preset.
        assert_eq!(pos("bottom"), (HAlign::Center, VAlign::Bottom));
    }

    #[test]
    fn a_position_can_also_be_written_field_by_field() {
        let pos = |spec| {
            let mut s = Style::default();
            let cli = Cli::parse_from(["wayhud", "x", "--position", spec]);
            apply_overrides(&mut s, &cli).unwrap();
            (s.halign, s.valign)
        };
        assert_eq!(
            pos("halign=left,valign=bottom"),
            (HAlign::Left, VAlign::Bottom)
        );
        // One axis alone leaves the other at the preset's value.
        assert_eq!(pos("valign=top"), (HAlign::Center, VAlign::Top));
        // And a word can still be corrected by a named field after it.
        assert_eq!(pos("bottom,halign=right"), (HAlign::Right, VAlign::Bottom));
    }

    #[test]
    fn a_dropped_spelling_is_answered_with_the_one_that_works() {
        // The CLI took "centre" before 1.0 and the config file never did.
        // Keeping it would be two vocabularies again; refusing it with a list
        // of five words, one of which differs by a letter, would be unkind.
        let err = format!("{:#}", with("--position", "centre").unwrap_err());
        assert!(err.contains(r#"spelled "center""#), "{err}");
    }

    #[test]
    fn a_direction_off_a_wash_is_handed_the_spec_that_works() {
        // Strict on purpose: a spec must not switch the effect because of one
        // field. But the kind is a word away, so the refusal carries it.
        let err = format!("{:#}", with("--vanish", "fade,dir=up").unwrap_err());
        assert!(err.contains("wash,dir=up"), "{err}");
        // The suggestion echoes the direction that was asked for, rather than
        // a fixed one that would send someone the wrong way.
        let err = format!("{:#}", with("--vanish", "fade,dir=down").unwrap_err());
        assert!(err.contains("wash,dir=down"), "{err}");
    }

    #[test]
    fn a_bad_position_is_rejected_not_ignored() {
        for spec in ["diagonal", "halign=top", "valign=left", "halign=sideways"] {
            let mut s = Style::default();
            let cli = Cli::parse_from(["wayhud", "x", "--position", spec]);
            assert!(
                apply_overrides(&mut s, &cli).is_err(),
                "{spec:?} was accepted"
            );
        }
    }

    /// Apply one spec to a fresh default style.
    fn with(flag: &str, spec: &str) -> Result<Style> {
        let mut s = Style::default();
        let cli = Cli::parse_from(["wayhud", "x", flag, spec]);
        apply_overrides(&mut s, &cli)?;
        Ok(s)
    }

    #[test]
    fn outline_takes_a_colour_a_width_or_both() {
        let s = with("--outline", "#123456").unwrap();
        assert_eq!(s.outline.as_deref(), Some("#123456"));
        // Nothing said about the width, so it stays unset and keeps scaling
        // with the font size.
        assert_eq!(s.outline_width, None);

        let s = with("--outline", "#123456,width=3.5").unwrap();
        assert_eq!(s.outline.as_deref(), Some("#123456"));
        assert_eq!(s.outline_width, Some(3.5));

        // Width alone, colour inherited.
        let s = with("--outline", "width=2").unwrap();
        assert_eq!(s.outline.as_deref(), Style::default().outline.as_deref());
        assert_eq!(s.outline_width, Some(2.0));
    }

    #[test]
    fn outline_none_switches_the_stroke_off_and_takes_no_width() {
        let s = with("--outline", "none").unwrap();
        assert!(s.outline.is_none());
        // There is no stroke to size, so this is a typo rather than a setting.
        assert!(with("--outline", "none,width=5").is_err());
    }

    #[test]
    fn an_outline_width_carries_across_a_colour_change() {
        let mut s = Style {
            outline_width: Some(7.0),
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--outline", "#abcdef"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert_eq!(
            s.outline_width,
            Some(7.0),
            "geometry must survive a recolour"
        );
    }

    #[test]
    fn glow_reaches_every_one_of_its_three_fields() {
        // alpha is the field the old colon form had no room for, which is what
        // started this.
        let s = with("--glow", "#b8bb26,radius=9,alpha=0.4").unwrap();
        let g = s.glow.expect("glow");
        assert_eq!(g.color, "#b8bb26");
        assert_eq!(g.radius, 9.0);
        assert_eq!(g.alpha, 0.4);
    }

    #[test]
    fn glow_fields_not_mentioned_come_from_the_preset() {
        let mut s = Style {
            glow: Some(Glow {
                color: "#ff0000".into(),
                radius: 20.0,
                alpha: 0.9,
            }),
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--glow", "alpha=0.2"]);
        apply_overrides(&mut s, &cli).unwrap();
        let g = s.glow.expect("glow");
        assert_eq!(g.color, "#ff0000");
        assert_eq!(g.radius, 20.0);
        assert_eq!(g.alpha, 0.2);
    }

    #[test]
    fn glow_none_switches_off_an_inherited_halo() {
        let mut s = Style {
            glow: Some(Glow::default()),
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--glow", "none"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(s.glow.is_none());
        assert!(with("--glow", "none,radius=4").is_err());
    }

    #[test]
    fn a_glow_field_over_a_preset_with_no_halo_turns_one_on() {
        // Refusing here would mean naming a colour just to set a radius.
        let s = with("--glow", "radius=6").unwrap();
        let g = s.glow.expect("glow");
        assert_eq!(g.radius, 6.0);
        assert_eq!(g.color, Glow::default().color);
    }

    #[test]
    fn reveal_reaches_every_typewriter_field() {
        let s = with(
            "--reveal",
            "typewriter,cps=50,cursor=false,jitter=0.3,scroll=true",
        )
        .unwrap();
        match s.reveal {
            Reveal::Typewriter {
                cps,
                cursor,
                jitter,
                scroll,
            } => {
                assert_eq!(cps, 50.0);
                assert!(!cursor, "cursor had no flag at all before this");
                assert_eq!(jitter, 0.3);
                assert!(scroll);
            }
            Reveal::Instant => panic!("expected a typewriter"),
        }
    }

    #[test]
    fn reveal_fields_not_mentioned_come_from_the_preset() {
        let mut s = Style {
            reveal: Reveal::Typewriter {
                cps: 12.0,
                cursor: false,
                jitter: 0.4,
                scroll: true,
            },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--reveal", "cps=44"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(
            matches!(
                s.reveal,
                Reveal::Typewriter { cps, cursor: false, jitter, scroll: true }
                    if cps == 44.0 && jitter == 0.4
            ),
            "only the speed was asked about: {:?}",
            s.reveal
        );
    }

    #[test]
    fn scroll_is_a_field_now_so_it_switches_both_ways() {
        // The old --scroll could only turn terminal mode on, which left a
        // preset that asked for it impossible to override.
        let mut s = Style {
            reveal: Reveal::Typewriter {
                cps: 28.0,
                cursor: true,
                jitter: 0.0,
                scroll: true,
            },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--reveal", "scroll=false"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(matches!(s.reveal, Reveal::Typewriter { scroll: false, .. }));
    }

    #[test]
    fn reveal_instant_takes_no_typewriter_fields() {
        assert!(matches!(
            with("--reveal", "instant").unwrap().reveal,
            Reveal::Instant
        ));
        for spec in [
            "instant,cps=20",
            "instant,scroll=true",
            "kind=instant,jitter=0.2",
        ] {
            assert!(with("--reveal", spec).is_err(), "{spec:?} was accepted");
        }
    }

    #[test]
    fn a_typewriter_field_over_an_instant_preset_says_so() {
        // Nothing to inherit, so typing at a speed nobody chose would be a
        // guess. Naming the kind switches it on deliberately.
        let mut s = Style {
            reveal: Reveal::Instant,
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--reveal", "cps=30"]);
        let err = format!("{:#}", apply_overrides(&mut s, &cli).unwrap_err());
        assert!(err.contains("typewriter"), "{err}");

        let mut s = Style {
            reveal: Reveal::Instant,
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--reveal", "typewriter,cps=30"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(matches!(s.reveal, Reveal::Typewriter { cps, .. } if cps == 30.0));
    }

    #[test]
    fn vanish_keeps_the_configured_duration_unless_told_otherwise() {
        let mut s = Style {
            vanish: Vanish::Fade { ms: 900 },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--vanish", "wash,dir=up"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert_eq!(
            s.vanish,
            Vanish::Wash {
                ms: 900,
                dir: Dir::Up
            }
        );

        assert_eq!(
            with("--vanish", "fade,ms=250").unwrap().vanish,
            Vanish::Fade { ms: 250 }
        );
    }

    #[test]
    fn a_vanish_duration_alone_keeps_the_effect() {
        // Only reachable because kind() gives the preset's variant a name the
        // spec can leave unsaid.
        let mut s = Style {
            vanish: Vanish::Dissolve { ms: 400 },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--vanish", "ms=800"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert_eq!(s.vanish, Vanish::Dissolve { ms: 800 });
    }

    #[test]
    fn wash_direction_is_a_field_rather_than_part_of_the_name() {
        assert_eq!(
            with("--vanish", "wash,dir=up,ms=700").unwrap().vanish,
            Vanish::Wash {
                ms: 700,
                dir: Dir::Up
            }
        );
        // A direction on anything else is a typo, not a setting.
        assert!(with("--vanish", "fade,dir=up").is_err());
        assert!(with("--vanish", "wash,dir=sideways").is_err());
    }

    #[test]
    fn a_wash_keeps_its_direction_when_only_the_duration_changes() {
        let mut s = Style {
            vanish: Vanish::Wash {
                ms: 300,
                dir: Dir::Up,
            },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--vanish", "ms=600"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert_eq!(
            s.vanish,
            Vanish::Wash {
                ms: 600,
                dir: Dir::Up
            }
        );
    }

    #[test]
    fn instant_vanish_takes_no_duration_and_survives_the_fallback() {
        assert_eq!(with("--vanish", "instant").unwrap().vanish, Vanish::Instant);
        // The check runs before the duration is resolved, so the complaint is
        // that instant has no duration rather than that this one is wrong.
        let err = format!("{:#}", with("--vanish", "instant,ms=99999999").unwrap_err());
        assert!(err.contains("takes no other fields"), "{err}");

        // An instant preset has no duration to keep, so a real effect falls
        // back to the compiled default rather than to a 0 ms flicker.
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
    fn an_unknown_vanish_kind_is_rejected_not_ignored() {
        assert!(with("--vanish", "sparkle").is_err());
        // The old spellings are gone: one vocabulary with the config file.
        for old in ["wash-up", "wash-down", "crt", "none"] {
            assert!(with("--vanish", old).is_err(), "{old:?} should be gone");
        }
    }

    #[test]
    fn sound_reaches_the_four_knobs_that_had_no_flag() {
        let s = with("--sound", "freq=1800,decay_ms=60,gain=0.3,every=4").unwrap();
        assert_eq!(s.sound.freq, 1800.0);
        assert_eq!(s.sound.decay_ms, 60.0);
        assert_eq!(s.sound.gain, 0.3);
        assert_eq!(s.sound.every, 4);
        // Untouched, so still on.
        assert!(s.sound.enabled);
    }

    #[test]
    fn sound_switches_both_ways_and_keeps_the_knobs() {
        let s = with("--sound", "off").unwrap();
        assert!(!s.sound.enabled);
        assert_eq!(
            s.sound.freq,
            Sound::default().freq,
            "off must not reset the tone"
        );

        let mut s = Style {
            sound: Sound {
                enabled: false,
                ..Sound::default()
            },
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--sound", "on"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(
            s.sound.enabled,
            "a preset that silenced itself must be overridable"
        );

        assert!(with("--sound", "maybe").is_err());
    }

    #[test]
    fn margin_and_line_align_have_flags_at_last() {
        assert_eq!(with("--margin", "7").unwrap().margin, 7);
        assert_eq!(
            with("--line-align", "center").unwrap().line_align,
            LineAlign::Center
        );
        assert!(with("--line-align", "middle").is_err());
    }

    #[test]
    fn an_unknown_field_is_refused_by_every_grouped_flag() {
        // deny_unknown_fields, but for the command line: a flag that ignores
        // half its spec is how a keybinding ends up lying about what it does.
        for (flag, spec) in [
            ("--outline", "#111,thickness=3"),
            ("--glow", "#111,size=3"),
            ("--position", "halign=left,align=top"),
            ("--reveal", "typewriter,speed=50"),
            ("--vanish", "fade,duration=200"),
            ("--sound", "on,volume=0.5"),
        ] {
            let err = format!("{:#}", with(flag, spec).unwrap_err());
            assert!(err.contains("unknown field"), "{flag} {spec}: {err}");
        }
    }

    #[test]
    fn the_command_line_is_held_to_the_same_ranges_as_the_config() {
        // Every one of these is checked in Style::validate and nowhere else
        // now; run() calls it after the overrides land. Before, each flag
        // restated its own bound and the two lists drifted.
        for (flag, spec) in [
            ("--outline", "#111,width=999"),
            ("--glow", "#111,radius=999"),
            ("--glow", "#111,alpha=4"),
            ("--reveal", "typewriter,cps=-5"),
            ("--reveal", "typewriter,jitter=3"),
            ("--sound", "freq=99999"),
            ("--sound", "decay_ms=1e9"),
            ("--sound", "gain=40"),
            ("--sound", "every=0"),
        ] {
            let style = with(flag, spec).expect("parsing should accept it");
            assert!(
                style.validate().is_err(),
                "{flag} {spec:?} passed validation"
            );
        }
    }

    #[test]
    fn every_style_field_is_reachable_from_the_command_line() {
        // The whole point of the rework: anything the config file can say, the
        // command line can say too, in the same words. A field added to Style
        // without a flag should fail here rather than be noticed a year later.
        let cli = Cli::parse_from([
            "wayhud",
            "x",
            "--font",
            "Sans 40",
            "--color",
            "#010101",
            "--outline",
            "#020202,width=3",
            "--glow",
            "#030303,radius=9,alpha=0.4",
            "--position",
            "top-right",
            "--margin",
            "7",
            "--line-align",
            "center",
            "--timeout",
            "2",
            "--reveal",
            "typewriter,cps=11,cursor=false,jitter=0.25,scroll=true",
            "--vanish",
            "wash,ms=333,dir=up",
            "--sound",
            "off,freq=1234,decay_ms=56,gain=0.7,every=3",
        ]);
        let mut s = Style::default();
        apply_overrides(&mut s, &cli).unwrap();
        s.validate().expect("the spec above must be a legal style");

        assert_eq!(s.font, "Sans 40");
        assert_eq!(s.color, "#010101");
        assert_eq!(s.outline.as_deref(), Some("#020202"));
        assert_eq!(s.outline_width, Some(3.0));
        assert_eq!(s.halign, HAlign::Right);
        assert_eq!(s.valign, VAlign::Top);
        assert_eq!(s.margin, 7);
        assert_eq!(s.line_align, LineAlign::Center);
        let g = s.glow.expect("glow");
        assert_eq!((g.color.as_str(), g.radius, g.alpha), ("#030303", 9.0, 0.4));
        assert_eq!(s.timeout_ms, 2000);
        assert!(
            matches!(
                s.reveal,
                Reveal::Typewriter { cps, cursor: false, jitter, scroll: true }
                    if cps == 11.0 && jitter == 0.25
            ),
            "{:?}",
            s.reveal
        );
        assert_eq!(
            s.vanish,
            Vanish::Wash {
                ms: 333,
                dir: Dir::Up
            }
        );
        assert_eq!(
            (
                s.sound.enabled,
                s.sound.freq,
                s.sound.decay_ms,
                s.sound.gain,
                s.sound.every
            ),
            (false, 1234.0, 56.0, 0.7, 3)
        );
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
    fn a_trailing_newline_is_trimmed_however_the_text_arrived() {
        // Pango makes an empty final line out of it and counts that line in
        // the layout height, so the surface is a line taller than the message:
        // the compositor centres the phantom too and everything sits half a
        // line high, with the caret finishing at x=0 on the empty line.
        // stdin was already trimmed for exactly this reason; argv was not, so
        // the same message rendered differently depending on how it was given.
        let cli = Cli::parse_from(["wayhud", "one\ntwo\n"]);
        assert_eq!(read_text(&cli).unwrap(), "one\ntwo");

        let cli = Cli::parse_from(["wayhud", "one\n\n\n"]);
        assert_eq!(read_text(&cli).unwrap(), "one", "all of them, not just one");

        // A newline in the middle is the whole point of the feature and stays.
        let cli = Cli::parse_from(["wayhud", "one\n\ntwo"]);
        assert_eq!(read_text(&cli).unwrap(), "one\n\ntwo");
    }

    #[test]
    fn raw_mode_keeps_the_text_verbatim() {
        let cli = Cli::parse_from(["wayhud", "a\\nb", "--raw"]);
        assert_eq!(read_text(&cli).unwrap(), "a\\nb");
    }

    #[test]
    fn an_absurdly_long_message_is_rejected_at_the_edge() {
        // Everything downstream scales with the text — steps, onsets, the
        // shaped layout — so an unbounded pipe is an OOM before the first
        // frame, and the lifetime cap only fires after those allocations.
        let long = "x".repeat(config::MAX_TEXT_CHARS + 1);
        let cli = Cli::parse_from(["wayhud", &long]);
        let err = read_text(&cli).unwrap_err();
        assert!(
            format!("{err:#}").contains("maximum"),
            "wrong error: {err:#}"
        );
        // The boundary itself still loads.
        let cli = Cli::parse_from(["wayhud", &"x".repeat(config::MAX_TEXT_CHARS)]);
        assert!(read_text(&cli).is_ok());
    }

    #[test]
    fn an_endless_stream_stops_at_the_budget_instead_of_being_read_whole() {
        // argv is bounded by ARG_MAX; the pipe is the side that can grow
        // without limit, so it is the side worth a test. The byte counter is
        // the assertion that matters — a cap that only counts characters
        // passes the error check below while still having read everything.
        struct Counting {
            asked: usize,
            left: usize,
        }
        impl Read for Counting {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(self.left);
                buf[..n].fill(b'x');
                self.asked += n;
                self.left -= n;
                Ok(n)
            }
        }
        let budget = MAX_TEXT_CHARS * 4 + 1;
        // Finite, so a regression fails the assertion rather than hanging.
        let mut src = Counting {
            asked: 0,
            left: budget * 4,
        };
        let err = read_capped(&mut src).unwrap_err();
        assert!(
            format!("{err:#}").contains("maximum"),
            "wrong error: {err:#}"
        );
        assert_eq!(
            src.asked, budget,
            "read {} bytes against a {budget}-byte budget: the stream is not bounded",
            src.asked
        );
        // A message at the cap still arrives whole.
        let at_cap = "x".repeat(MAX_TEXT_CHARS);
        assert_eq!(read_capped(at_cap.as_bytes()).unwrap(), at_cap);
    }

    #[test]
    fn negative_timeout_is_rejected() {
        let mut s = Style::default();
        // Must use `=`: clap reads a bare `-3` as a flag, not a value.
        let cli = Cli::parse_from(["wayhud", "x", "--timeout=-3"]);
        assert!(apply_overrides(&mut s, &cli).is_err());
    }
}
