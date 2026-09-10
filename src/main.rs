//! Layer-shell text overlay for sway. One invocation shows one message and
//! exits, with independent surfaces per invocation; `--listen` keeps the same
//! surfaces for whatever arrives on a socket.

mod config;
mod hud;
mod ipc;
mod outputs;
mod sound;
mod spec;
mod synth;
mod timeline;

use std::cell::{Cell, RefCell};
use std::io::{IsTerminal, Read};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use gtk::gio::prelude::SocketExtManual;
use gtk::glib;

use config::{
    Config, Dir, Glow, HAlign, LineAlign, MAX_LIFETIME_MS, MAX_TEXT_CHARS, Reveal, Scanlines,
    Sound, Style, VAlign, Vanish,
};
use hud::{Hud, Session};
use outputs::OutputSpec;
use spec::Spec;
use timeline::Phase;

#[derive(Parser, Debug)]
#[command(
    name = "wayhud",

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

    /// Hold time in seconds, measured from the end of the reveal.
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

    /// Glow colour, or "none". Takes radius=PX and alpha=0..1. E.g.
    /// "#b8bb26,radius=12,alpha=0.7".
    #[arg(long)]
    glow: Option<String>,

    /// Scanline period in device pixels, or "none". Takes strength=0..1 and
    /// duty=0..<1. E.g. "4,strength=0.35".
    #[arg(long)]
    scanlines: Option<String>,

    /// Placement: center, top, bottom-right, … Takes halign= and valign=.
    #[arg(long)]
    position: Option<String>,

    /// Gap from the anchored edge, in logical pixels. A centred axis ignores
    /// it.
    #[arg(long)]
    margin: Option<i32>,

    /// Width of the text block in logical pixels. Wraps to it and sizes the
    /// surface from it, instead of from the measured text.
    #[arg(long)]
    width: Option<i32>,

    /// Height of the block in lines. Reserves the room up front, so a message
    /// growing inside it does not resize the surface.
    #[arg(long)]
    lines: Option<usize>,

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

    /// Typewriter sound: "on", "off", or freq=, decay_ms=, gain= and every=.
    /// E.g. "freq=1800,gain=0.3".
    #[arg(long)]
    sound: Option<String>,

    /// Take the argument literally: don't expand \n, \t or \\.
    #[arg(long)]
    raw: bool,

    /// Config file (default: $XDG_CONFIG_HOME/wayhud/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Send the message to a running listener instead of showing it here.
    #[arg(long)]
    send: bool,

    /// Stay up and show what arrives on the socket. Takes no message.
    #[arg(long, conflicts_with = "send")]
    listen: bool,

    /// Listener socket (default: $XDG_RUNTIME_DIR/wayhud.sock).
    #[arg(long)]
    socket: Option<PathBuf>,
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

/// The listener socket: the flag, or the default under the runtime directory.
fn socket_path(cli: &Cli) -> Result<PathBuf> {
    match &cli.socket {
        Some(p) => Ok(p.clone()),
        None => ipc::default_path(),
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    // A client carries text and nothing else: the style belongs to the
    // listener, so it does not read the config at all.
    if cli.send {
        let text = read_text(&cli)?;
        ipc::send(&socket_path(&cli)?, &text)?;
        return Ok(ExitCode::SUCCESS);
    }

    let spec = OutputSpec::parse(&cli.output);
    let cfg = Config::load(cli.config.clone())?;
    let mut style = cfg.style(&cli.style)?;
    apply_overrides(&mut style, &cli)?;
    // Validate the resolved style after applying CLI overrides.
    style
        .validate()
        .context("in the style built from the command line")?;

    if cli.listen {
        anyhow::ensure!(
            cli.text.is_none(),
            "--listen takes no message; it shows what arrives on the socket"
        );
        return listen(style, spec, socket_path(&cli)?);
    }

    let text = read_text(&cli)?;
    let hud = Rc::new(Hud::new(style, text, seed())?);
    // Limit the total reveal, hold and vanish duration.
    let total = hud.timeline.total_ms();
    anyhow::ensure!(
        total.is_finite() && total <= MAX_LIFETIME_MS as f64,
        "the message would stay up for {:.0} s; the maximum is {} s \
         (check --reveal, --timeout and --vanish)",
        total / 1000.0,
        MAX_LIFETIME_MS / 1000
    );

    // Mix before GUI setup to avoid delaying the first frame.
    let cfg = &hud.style.sound;
    let reveal_pcm = sound::typewriter_track(cfg, &hud.timeline.onsets(cfg.every));
    // Delay the separate untype track to avoid allocating silence during the
    // hold.
    let vanish_pcm = sound::typewriter_track(cfg, &hud.timeline.vanish_onsets(cfg.every));
    let vanish_delay = Duration::from_secs_f64(hud.timeline.vanish_start().max(0.0));
    let tracks = RefCell::new(Some((reveal_pcm, vanish_pcm)));
    // Wait for playback on exit so the final untype blip can finish.
    let playing: Rc<RefCell<Vec<std::thread::JoinHandle<()>>>> = Rc::new(RefCell::new(Vec::new()));
    // `take()` starts playback once across all windows.
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

    // Exit after the last overlay closes.
    let main_loop = glib::MainLoop::new(None, false);
    let alive = Rc::new(Cell::new(monitors.len()));
    let session = hud::Session::once(hud.clone());
    for monitor in monitors {
        hud::present(&monitor, session.clone(), on_first_frame.clone(), {
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
    // Keep the window background transparent.
    provider.load_from_string("window.wayhud { background: transparent; }");
    // The display retains its own reference to the CSS provider.
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Read at most four bytes per allowed character plus one overflow byte.
/// This bounds memory before UTF-8 decoding and character counting.
fn read_capped(r: impl Read) -> Result<String> {
    const BUDGET: u64 = MAX_TEXT_CHARS as u64 * 4 + 1;
    let mut bytes = Vec::new();
    r.take(BUDGET)
        .read_to_end(&mut bytes)
        .context("reading stdin")?;
    // Check overflow before decoding: a truncated tail may split a UTF-8
    // character.
    anyhow::ensure!(
        (bytes.len() as u64) < BUDGET,
        "message is longer than the maximum of {MAX_TEXT_CHARS} characters"
    );
    String::from_utf8(bytes).context("input is not valid UTF-8")
}

/// Lines a listener keeps when the style does not reserve any. The block has
/// to be bounded or a burst grows the surface off the screen; `lines` is the
/// reservation the surface is sized from, so where it is set it is also
/// exactly how much room there is.
const DEFAULT_LISTEN_LINES: usize = 10;

/// The message a listener currently has up, and when it went up, so an
/// arriving one can tell whether to grow the block or start a new one.
struct Live {
    hud: Rc<Hud>,
    started: std::time::Instant,
}

/// Split a block into lines without allocating a line for an empty block.
fn block_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').map(str::to_string).collect()
    }
}

/// Stay up and show what arrives on the socket.
///
/// The rule is that a message joins the one on screen rather than replacing
/// it: while the block is revealing or being held, an arrival is appended as a
/// line and only that line is typed, and the hold starts again from it. A
/// vanish is a commit point -- what arrives during one waits for it to finish
/// and then starts a block of its own, so a burst cannot keep the overlay up
/// forever.
fn listen(mut style: Style, spec: OutputSpec, path: PathBuf) -> Result<ExitCode> {
    // Bound the block: without it a burst grows the surface off the screen.
    // `lines` is the reservation the surface is sized from, so it is also
    // exactly how many lines there is room for.
    // Reserved before the first message, because the layer surface is
    // negotiated once and cannot be grown into afterwards.
    let cap = *style.lines.get_or_insert(DEFAULT_LISTEN_LINES);

    let socket = ipc::bind(&path)?;
    gtk::init().context("initialising GTK")?;
    load_css();
    let display = gtk::gdk::Display::default().context("no display")?;
    let monitors = outputs::resolve(&display, &spec)?;

    // Nothing on screen yet; the windows are built hidden.
    let session = Session::listening(Rc::new(Hud::new(style.clone(), String::new(), seed())?));

    let tracks: Rc<RefCell<Option<Tracks>>> = Rc::new(RefCell::new(None));
    let playing: Rc<RefCell<Vec<std::thread::JoinHandle<()>>>> = Rc::new(RefCell::new(Vec::new()));
    let on_first_frame: Rc<dyn Fn()> = Rc::new({
        let tracks = tracks.clone();
        let playing = playing.clone();
        move || {
            if let Some(t) = tracks.borrow_mut().take() {
                let mut handles = playing.borrow_mut();
                handles.extend(sound::play_detached(t.reveal, Duration::ZERO));
                handles.extend(sound::play_detached(t.vanish, t.delay));
                // A listener runs for days; finished threads must not pile up.
                handles.retain(|h| !h.is_finished());
            }
        }
    });

    for monitor in &monitors {
        hud::present(monitor, session.clone(), on_first_frame.clone(), || {})?;
    }

    let live: Rc<RefCell<Option<Live>>> = Rc::new(RefCell::new(None));
    let pending: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    // Put a block up, typing only what follows `shown`.
    let put: Rc<dyn Fn(String, usize)> = Rc::new({
        let session = session.clone();
        let tracks = tracks.clone();
        let live = live.clone();
        move |text: String, shown: usize| {
            let hud = Hud::new(style.clone(), text, seed()).map(|mut h| {
                h.timeline = timeline::Timeline::resuming(
                    &h.text,
                    &h.style.reveal,
                    h.style.timeout_ms,
                    &h.style.vanish,
                    seed(),
                    shown,
                );
                h
            });
            let hud = match hud {
                Ok(h) => Rc::new(h),
                Err(e) => {
                    eprintln!("wayhud: dropped a message: {e:#}");
                    return;
                }
            };
            *tracks.borrow_mut() = Some(mix(&hud));
            *live.borrow_mut() = Some(Live {
                hud: hud.clone(),
                started: std::time::Instant::now(),
            });
            session.show(hud);
        }
    });

    let mut buf = vec![0u8; ipc::MAX_DATAGRAM];
    // gio only watches the descriptor: reading and decoding stay in `ipc`, so
    // there is one path for it rather than one that runs and one that is
    // merely tested. The duplicate is what gio takes ownership of.
    let watch = gtk::gio::Socket::from_fd(OwnedFd::from(
        socket.try_clone().context("duplicating the socket")?,
    ))
    .context("watching the socket")?;
    let source = watch.create_source(
        glib::IOCondition::IN,
        None::<&gtk::gio::Cancellable>,
        None,
        glib::Priority::DEFAULT,
        move |_, _| {
            // Drain: several notifications can land between two wakeups.
            while let Some(text) = ipc::recv(&socket, &mut buf) {
                let text = text.trim_end_matches('\n').to_string();
                if text.is_empty() {
                    continue;
                }
                arrived(&live, &pending, &put, cap, text);
            }
            glib::ControlFlow::Continue
        },
    );
    source.attach(Some(&glib::MainContext::default()));

    glib::MainLoop::new(None, false).run();
    Ok(ExitCode::SUCCESS)
}

/// Decide what one arriving message does to what is on screen.
fn arrived(
    live: &Rc<RefCell<Option<Live>>>,
    pending: &Rc<RefCell<Vec<String>>>,
    put: &Rc<dyn Fn(String, usize)>,
    cap: usize,
    text: String,
) {
    let phase = live.borrow().as_ref().map(|l| {
        let ms = l.started.elapsed().as_secs_f64() * 1000.0;
        (
            l.hud.text.clone(),
            l.hud.timeline.phase_at(ms),
            l.hud.timeline.total_ms() - ms,
        )
    });
    match phase {
        // Growing: the block gains a line and the hold starts again from it.
        Some((current, Phase::Reveal { .. } | Phase::Hold, _)) => {
            let mut lines = block_lines(&current);
            lines.push(text);
            let over = lines.len().saturating_sub(cap);
            lines.drain(..over);
            let kept = lines[..lines.len() - 1].join("\n");
            // The separator counts as shown too, or the new line would be
            // typed starting with the newline before it.
            let shown = if kept.is_empty() {
                0
            } else {
                kept.chars().count() + 1
            };
            put(lines.join("\n"), shown);
        }
        // A vanish is a commit point: let it finish, then start afresh.
        Some((_, Phase::Vanish { .. }, remaining)) => {
            pending.borrow_mut().push(text);
            let wait = Duration::from_secs_f64((remaining.max(0.0) / 1000.0) + 0.01);
            let pending = pending.clone();
            let put = put.clone();
            glib::timeout_add_local_once(wait, move || {
                let held: Vec<String> = pending.borrow_mut().drain(..).collect();
                if !held.is_empty() {
                    put(held.join("\n"), 0);
                }
            });
        }
        // Nothing up, or the last block is finished.
        _ => put(text, 0),
    }
}

/// The blip tracks for one message, and how long the untype track waits.
struct Tracks {
    reveal: Vec<i16>,
    vanish: Vec<i16>,
    delay: Duration,
}

/// Mix a message's blips up front, so the clicks stay locked to the
/// characters instead of inheriting the sound server's write scheduling.
fn mix(hud: &Hud) -> Tracks {
    let cfg = &hud.style.sound;
    Tracks {
        reveal: sound::typewriter_track(cfg, &hud.timeline.onsets(cfg.every)),
        // The untype track is delayed rather than padded, so a long hold does
        // not become allocated silence.
        vanish: sound::typewriter_track(cfg, &hud.timeline.vanish_onsets(cfg.every)),
        delay: Duration::from_secs_f64(hud.timeline.vanish_start().max(0.0)),
    }
}

/// A fresh jitter seed, so two runs do not type identically.
fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5bd1_e995)
}

/// Text comes from argv, or from stdin when argv is empty or "-".
fn read_text(cli: &Cli) -> Result<String> {
    // Expand escapes only in argv; preserve piped text.
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
    // Trim trailing newlines on both input paths to avoid an empty Pango line
    // that changes the surface height and final caret position.
    let text = text.trim_end_matches('\n');
    // Check the character limit before allocating layout and timing data.
    let count = text.chars().count();
    anyhow::ensure!(
        count <= MAX_TEXT_CHARS,
        "message is {count} characters; the maximum is {MAX_TEXT_CHARS}"
    );
    Ok(text.to_string())
}

/// Expand escapes for sway bindings executed through `sh`.
/// Preserve unknown escapes.
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

/// Apply CLI fields over the preset, preserving omitted values.
/// `run` validates the resulting style.
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
    if let Some(sl) = &cli.scanlines {
        style.scanlines = parse_scanlines(sl, style.scanlines.as_ref())?;
    }
    if let Some(t) = cli.timeout {
        // Validate seconds before conversion to u64, which would saturate
        // negatives to zero.
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
    if let Some(w) = cli.width {
        style.width = Some(w);
    }
    if let Some(n) = cli.lines {
        style.lines = Some(n);
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

/// Parse placement. A bare name sets both axes; named fields set one each.
/// The bare value is the period: the field a raster is usually described by.
fn parse_scanlines(spec: &str, current: Option<&Scanlines>) -> Result<Option<Scanlines>> {
    let mut s = Spec::parse("--scanlines", spec)?;
    let period = s.headline("period")?;
    let strength = s.take_parsed::<f64>("strength")?;
    let duty = s.take_parsed::<f64>("duty")?;
    s.finish()?;
    if period == Some("none") {
        anyhow::ensure!(
            strength.is_none() && duty.is_none(),
            "--scanlines none takes no other fields; there is no raster to describe"
        );
        return Ok(None);
    }
    let period = period
        .map(|v| {
            v.parse::<f64>()
                .map_err(|e| anyhow::anyhow!("--scanlines: bad value {v:?} for period ({e})"))
        })
        .transpose()?;
    // Use the built-in raster when switching one on through a field-only spec.
    let base = current.cloned().unwrap_or_default();
    Ok(Some(Scanlines {
        period: period.unwrap_or(base.period),
        strength: strength.unwrap_or(base.strength),
        duty: duty.unwrap_or(base.duty),
    }))
}

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
            // Suggest the current spelling for the pre-1.0 alias.
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

/// Parse outline colour and width, preserving omitted preset fields.
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
        // Reject width with `none`; preserve the configured width when
        // disabling alone.
        anyhow::ensure!(
            width.is_none(),
            "--outline none takes no width; there is no stroke to size"
        );
        return Ok((None, current_width));
    }
    // Preserve the preset colour when only width is specified.
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
    // Use glow defaults when enabling a halo through a field-only spec.
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
            let (base_cps, base_cursor, base_jitter, base_scroll) = match *current {
                Reveal::Typewriter {
                    cps,
                    cursor,
                    jitter,
                    scroll,
                } => (cps, cursor, jitter, scroll),
                // Typewriter fields require a typewriter preset or an explicit
                // kind.
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
    // Reject duration on instant before parsing its value.
    if kind == "instant" {
        anyhow::ensure!(
            ms.is_none() && dir.is_none(),
            "--vanish instant takes no other fields; it happens on one frame"
        );
        return Ok(Vanish::Instant);
    }
    // Use the built-in duration when switching from instant.
    let ms = ms.unwrap_or(match current.ms() {
        0 => config::DEFAULT_VANISH_MS,
        keep => keep,
    });
    if kind != "wash" {
        // Direction is valid only for wash; suggest an explicit kind.
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
            // Inherit direction only from wash.
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
            // Bare values use on/off; the named enabled field uses true/false.
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
        // A bare position sets both axes.
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

        assert_eq!(pos("valign=top"), (HAlign::Center, VAlign::Top));
        // Named fields can override the bare position.
        assert_eq!(pos("bottom,halign=right"), (HAlign::Right, VAlign::Bottom));
    }

    #[test]
    fn a_dropped_spelling_is_answered_with_the_one_that_works() {
        // Suggest center for the obsolete centre alias.
        let err = format!("{:#}", with("--position", "centre").unwrap_err());
        assert!(err.contains(r#"spelled "center""#), "{err}");
    }

    #[test]
    fn a_direction_off_a_wash_is_handed_the_spec_that_works() {
        // Reject dir without wash and suggest the required kind.
        let err = format!("{:#}", with("--vanish", "fade,dir=up").unwrap_err());
        assert!(err.contains("wash,dir=up"), "{err}");
        // Preserve the requested direction in the suggestion.
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

        assert_eq!(s.outline_width, None);

        let s = with("--outline", "#123456,width=3.5").unwrap();
        assert_eq!(s.outline.as_deref(), Some("#123456"));
        assert_eq!(s.outline_width, Some(3.5));

        let s = with("--outline", "width=2").unwrap();
        assert_eq!(s.outline.as_deref(), Style::default().outline.as_deref());
        assert_eq!(s.outline_width, Some(2.0));
    }

    #[test]
    fn outline_none_switches_the_stroke_off_and_takes_no_width() {
        let s = with("--outline", "none").unwrap();
        assert!(s.outline.is_none());

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
        // A radius-only spec enables glow with its default colour.
        let s = with("--glow", "radius=6").unwrap();
        let g = s.glow.expect("glow");
        assert_eq!(g.radius, 6.0);
        assert_eq!(g.color, Glow::default().color);
    }

    #[test]
    fn scanlines_reach_every_one_of_their_three_fields() {
        let s = with("--scanlines", "6,strength=0.5,duty=0.25").unwrap();
        let sl = s.scanlines.expect("scanlines");
        assert_eq!(sl.period, 6.0);
        assert_eq!(sl.strength, 0.5);
        assert_eq!(sl.duty, 0.25);
    }

    #[test]
    fn scanline_fields_not_mentioned_come_from_the_preset() {
        let mut s = Style {
            scanlines: Some(Scanlines {
                period: 10.0,
                strength: 0.9,
                duty: 0.3,
            }),
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--scanlines", "strength=0.2"]);
        apply_overrides(&mut s, &cli).unwrap();
        let sl = s.scanlines.expect("scanlines");
        assert_eq!(sl.period, 10.0);
        assert_eq!(sl.duty, 0.3);
        assert_eq!(sl.strength, 0.2);
    }

    #[test]
    fn scanlines_none_switches_off_an_inherited_raster() {
        let mut s = Style {
            scanlines: Some(Scanlines::default()),
            ..Style::default()
        };
        let cli = Cli::parse_from(["wayhud", "x", "--scanlines", "none"]);
        apply_overrides(&mut s, &cli).unwrap();
        assert!(s.scanlines.is_none());
        assert!(with("--scanlines", "none,strength=0.4").is_err());
    }

    #[test]
    fn a_scanline_field_over_a_preset_with_none_turns_them_on() {
        let s = with("--scanlines", "strength=0.5").unwrap();
        let sl = s.scanlines.expect("scanlines");
        assert_eq!(sl.strength, 0.5);
        assert_eq!(sl.period, Scanlines::default().period);
    }

    #[test]
    fn a_scanline_period_that_is_not_a_number_is_refused_as_one() {
        // The only flag whose bare value is a number rather than a word, so a
        // typo must come back about the period and not about an unknown kind.
        let e = with("--scanlines", "wide").unwrap_err().to_string();
        assert!(e.contains("period"), "{e}");
        // And a misspelt field still lists the ones that exist.
        let e = with("--scanlines", "4,thickness=0.5")
            .unwrap_err()
            .to_string();
        assert!(e.contains("strength"), "{e}");
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
        // Scroll must be overridable in both directions.
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
        // Require an explicit kind when enabling typing from instant.
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
        // Report that instant takes no duration before parsing ms.
        let err = format!("{:#}", with("--vanish", "instant,ms=99999999").unwrap_err());
        assert!(err.contains("takes no other fields"), "{err}");

        // Switching from instant uses the built-in effect duration.
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
        // Reject obsolete aliases.
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
        // Reject unknown fields for every spec flag.
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
        // Validate ranges after applying CLI overrides.
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
            ("--width", "0"),
            ("--width", "99999"),
            ("--lines", "0"),
            ("--lines", "9999"),
            ("--scanlines", "1"),
            ("--scanlines", "4,strength=1.5"),
            ("--scanlines", "4,duty=1"),
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
        // Enumerated from the style rather than from a list kept beside it.
        // A hand-written list is only as complete as whoever last added a
        // field remembered to be, and it has twice not been: `scanlines` and
        // `lines` both reached `Style` while this test stayed green.
        //
        // Serialising is what makes it notice. A field no flag below sets is
        // still `None` here, and TOML has no null to write it as, so the
        // conversion fails and names the field.
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
            "--scanlines",
            "6,strength=0.5,duty=0.25",
            "--width",
            "640",
            "--lines",
            "9",
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

        let table = toml::Table::try_from(&s)
            .unwrap_or_else(|e| panic!("a style field has no flag on the command line above: {e}"));
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "color",
                "font",
                "glow",
                "halign",
                "line_align",
                "lines",
                "margin",
                "outline",
                "outline_width",
                "reveal",
                "scanlines",
                "sound",
                "timeout_ms",
                "valign",
                "vanish",
                "width",
            ],
            "a style field was added or renamed"
        );

        assert_eq!(s.font, "Sans 40");
        assert_eq!(s.color, "#010101");
        assert_eq!(s.outline.as_deref(), Some("#020202"));
        assert_eq!(s.outline_width, Some(3.0));
        assert_eq!(s.halign, HAlign::Right);
        assert_eq!(s.valign, VAlign::Top);
        assert_eq!(s.margin, 7);
        assert_eq!(s.width, Some(640));
        assert_eq!(s.lines, Some(9));
        assert_eq!(s.line_align, LineAlign::Center);
        let g = s.glow.expect("glow");
        assert_eq!((g.color.as_str(), g.radius, g.alpha), ("#030303", 9.0, 0.4));
        let sl = s.scanlines.expect("scanlines");
        assert_eq!((sl.period, sl.strength, sl.duty), (6.0, 0.5, 0.25));
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
        // Large timeouts must fail before audio allocation.
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
        // Preserve backslashes in unknown escapes.
        assert_eq!(unescape("C:\\dir"), "C:\\dir");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn a_trailing_newline_is_trimmed_however_the_text_arrived() {
        // Trim trailing newlines consistently for argv and stdin.
        let cli = Cli::parse_from(["wayhud", "one\ntwo\n"]);
        assert_eq!(read_text(&cli).unwrap(), "one\ntwo");

        let cli = Cli::parse_from(["wayhud", "one\n\n\n"]);
        assert_eq!(read_text(&cli).unwrap(), "one", "all of them, not just one");

        // Preserve internal newlines.
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
        // Bound text before downstream allocations.
        let long = "x".repeat(config::MAX_TEXT_CHARS + 1);
        let cli = Cli::parse_from(["wayhud", &long]);
        let err = read_text(&cli).unwrap_err();
        assert!(
            format!("{err:#}").contains("maximum"),
            "wrong error: {err:#}"
        );

        let cli = Cli::parse_from(["wayhud", &"x".repeat(config::MAX_TEXT_CHARS)]);
        assert!(read_text(&cli).is_ok());
    }

    #[test]
    fn an_endless_stream_stops_at_the_budget_instead_of_being_read_whole() {
        // Check bytes consumed as well as the error: character counting alone
        // would allow an unbounded read before rejection.
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
        // Use a finite source so a regression fails without hanging.
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
