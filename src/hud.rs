//! One layer-shell window per output, drawn by hand with pango + cairo.
//!
//! Why not a GtkLabel and CSS: GTK4's CSS has no text-stroke, and an outline
//! faked with eight text-shadows falls apart at 72pt. Going through a cairo
//! path gives a real stroke, and the typewriter clip and the caret come for
//! free in the same draw call.

use std::cell::Cell;
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, LayerShell};

use crate::config::{Align, Dir, LineAlign, Reveal, Style, Vanish};
use crate::timeline::{Phase, Timeline};

/// A message with everything already parsed and validated, so nothing can fail
/// once we're inside a draw callback.
pub struct Hud {
    pub style: Style,
    pub text: String,
    pub timeline: Timeline,
    fill: gdk::RGBA,
    outline: Option<gdk::RGBA>,
    font: pango::FontDescription,
    outline_width: f64,
}

impl Hud {
    /// `seed` drives the typewriter jitter; pass a fixed one to make a run
    /// reproducible.
    pub fn new(style: Style, text: String, seed: u64) -> Result<Hud> {
        let fill = gdk::RGBA::parse(&style.color)
            .with_context(|| format!("bad color {:?}", style.color))?;
        let outline = match &style.outline {
            Some(c) => {
                Some(gdk::RGBA::parse(c).with_context(|| format!("bad outline color {c:?}"))?)
            }
            None => None,
        };
        let font = pango::FontDescription::from_string(&style.font);
        // from_string never fails — it just yields an empty family that
        // silently renders in the default font, which looks like a bug.
        anyhow::ensure!(
            font.family().is_some(),
            "font {:?} has no family; expected something like \
             \"LythMono Nerd Font 72\"",
            style.font
        );
        let timeline = Timeline::new(&text, &style.reveal, style.timeout_ms, &style.vanish, seed);
        let outline_width = outline_width(&font, style.outline_width);
        Ok(Hud {
            fill,
            outline,
            outline_width,
            font,
            timeline,
            style,
            text,
        })
    }

    /// Padding around the text box: the stroke straddles the glyph outline, and
    /// the caret sits past the last character.
    fn pad(&self) -> f64 {
        self.outline_width.max(0.0).ceil() + 8.0
    }

    /// `max_width` is the widest the text block may get, in logical pixels.
    /// Without it a long line silently runs off both edges of the output:
    /// the layer surface is sized from the text, and the compositor clips
    /// whatever doesn't fit on screen.
    fn layout_for(&self, widget: &impl IsA<gtk::Widget>, max_width: i32) -> pango::Layout {
        let layout = widget.as_ref().create_pango_layout(Some(&self.text));
        layout.set_font_description(Some(&self.font));
        layout.set_alignment(match self.style.line_align {
            LineAlign::Left => pango::Alignment::Left,
            LineAlign::Center => pango::Alignment::Center,
            LineAlign::Right => pango::Alignment::Right,
        });
        fit_width(&layout, max_width);
        layout
    }
}

/// Resolve the stroke width in logical pixels.
///
/// cairo centres a stroke on the glyph outline and the fill then covers the
/// inner half, so what shows is always `width / 2` of halo. Held constant that
/// is 21% of the stem at 72pt and 62% at 24pt — a tasteful outline on one and
/// a fake bold on the other. Scaling with the size keeps the ratio fixed;
/// the divisor is set so 72pt lands on the 5.0 that was picked by eye.
fn outline_width(font: &pango::FontDescription, configured: Option<f64>) -> f64 {
    if let Some(w) = configured {
        return w.max(0.0);
    }
    let size = font.size();
    if size <= 0 {
        // No size in the description at all; pango will pick its own default,
        // so there is nothing to scale against.
        return 1.0;
    }
    // An absolute size is already in device units rather than points.
    let points = if font.is_size_absolute() {
        size as f64 / pango::SCALE as f64 * 72.0 / 96.0
    } else {
        size as f64 / pango::SCALE as f64
    };
    (points / 14.0).max(0.5)
}

/// Wrap the layout to `max_width`, then shrink its width to what the text
/// actually occupies.
///
/// The second step is not cosmetic. Pango positions lines for a non-left
/// alignment inside the layout's width, and that width is the wrapping budget
/// — most of the screen — while the window is sized to the text. Leave it and
/// a centred line is drawn hundreds of pixels outside the surface, i.e.
/// nothing appears at all. Re-setting the width to the measured text keeps the
/// line breaks (every line already fits) and makes alignment relative to the
/// block, which is what it has to mean here.
fn fit_width(layout: &pango::Layout, max_width: i32) {
    if max_width <= 0 {
        return;
    }
    layout.set_width(max_width * pango::SCALE);
    // WordChar, not Word: a single unbroken token longer than the screen
    // (a path, a hash) has to break somewhere.
    layout.set_wrap(pango::WrapMode::WordChar);
    let (text_width, _) = layout.pixel_size();
    if text_width > 0 {
        layout.set_width(text_width * pango::SCALE);
    }
}

/// Frame state shared between the tick callback and the draw callback.
struct Frame {
    t0: Cell<Option<i64>>,
    phase: Cell<Phase>,
    t_ms: Cell<f64>,
    blink: Cell<bool>,
}

/// Build and show the overlay on one monitor.
///
/// `on_first_frame` fires exactly once across all windows, on whichever one
/// draws first — that is where the audio starts, so sound and animation share
/// a t0 instead of drifting apart by however long window setup took.
/// `on_closed` fires when this window is done, so the caller can stop the main
/// loop once the last one goes.
pub fn present(
    monitor: &gdk::Monitor,
    hud: Rc<Hud>,
    on_first_frame: Rc<dyn Fn()>,
    on_closed: impl Fn() + 'static,
) -> Result<()> {
    // A plain GtkWindow, not a GtkApplicationWindow: GtkApplication would
    // register on the session bus and go looking for the Inhibit portal, which
    // no backend provides under sway. We need none of what it offers.
    let window = gtk::Window::new();
    window.add_css_class("wayhud");

    window.init_layer_shell();
    window.set_monitor(Some(monitor));
    // The namespace is what shows up on the wire; sway rules key off it, so
    // it is effectively public API and must not drift.
    window.set_namespace(Some("wayhud"));
    window.set_layer(gtk_layer_shell::Layer::Overlay);
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);
    apply_anchors(&window, &hud.style);

    let area = gtk::DrawingArea::new();
    window.set_child(Some(&area));

    // Size the surface from the FULL text once, up front. Sizing it to the
    // partially typed string instead would make the window grow under the
    // typewriter and drag the text across the screen as it goes.
    let pad = hud.pad();
    let max_width = text_budget(monitor, &hud.style, pad);
    // Shaped once and reused: the text, font and width never change, and a
    // vanish redraws every frame — re-shaping there would burn a full pango
    // layout pass 60 times a second on each output.
    let layout = Rc::new(hud.layout_for(&area, max_width));
    let (tw, th) = layout.pixel_size();
    area.set_content_width(tw + (pad * 2.0) as i32);
    area.set_content_height(th + (pad * 2.0) as i32);

    let frame = Rc::new(Frame {
        t0: Cell::new(None),
        phase: Cell::new(Phase::Reveal { chars: 0 }),
        t_ms: Cell::new(0.0),
        blink: Cell::new(false),
    });

    area.set_draw_func({
        let hud = hud.clone();
        let frame = frame.clone();
        move |_area, cr, _w, _h| {
            draw(cr, &hud, &layout, frame.phase.get(), frame.blink.get());
        }
    });

    area.add_tick_callback({
        let hud = hud.clone();
        let frame = frame.clone();
        let window = window.clone();
        let first = Cell::new(true);
        move |area, clock| {
            let now = clock.frame_time();
            let t0 = match frame.t0.get() {
                Some(t) => t,
                None => {
                    frame.t0.set(Some(now));
                    now
                }
            };
            if first.replace(false) {
                on_first_frame();
            }
            let t_ms = (now - t0) as f64 / 1000.0;
            let phase = hud.timeline.phase_at(t_ms);
            let blink = show_caret(&hud.style.reveal, &hud.style.vanish, phase, t_ms);
            let changed = phase != frame.phase.get() || blink != frame.blink.get();
            frame.phase.set(phase);
            frame.t_ms.set(t_ms);
            frame.blink.set(blink);
            if changed {
                area.queue_draw();
            }
            if phase == Phase::Done {
                window.close();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        }
    });

    window.connect_close_request(move |_| {
        on_closed();
        glib::Propagation::Proceed
    });

    window.present();

    // Click-through. Without an empty input region the overlay eats pointer
    // events for whatever sits under it — which, at Layer::Overlay, is
    // everything. Only available once the surface exists, hence after present.
    //
    // Refuse to stay up if it can't be set: an overlay nobody can click
    // through is worse than a message nobody sees, and a silent skip here
    // would leave the pointer trapped with no hint as to why.
    let surface = window
        .surface()
        .context("window has no surface after present; cannot make it click-through")?;
    surface.set_input_region(Some(&gtk::cairo::Region::create()));
    Ok(())
}

fn apply_anchors(window: &gtk::Window, style: &Style) {
    // Anchoring neither edge of an axis is what makes the compositor centre
    // the surface on it, so Center deliberately sets nothing.
    match style.halign {
        Align::Start => {
            window.set_anchor(Edge::Left, true);
            window.set_margin(Edge::Left, style.margin);
        }
        Align::End => {
            window.set_anchor(Edge::Right, true);
            window.set_margin(Edge::Right, style.margin);
        }
        Align::Center => {}
    }
    match style.valign {
        Align::Start => {
            window.set_anchor(Edge::Top, true);
            window.set_margin(Edge::Top, style.margin);
        }
        Align::End => {
            window.set_anchor(Edge::Bottom, true);
            window.set_margin(Edge::Bottom, style.margin);
        }
        Align::Center => {}
    }
}

/// How wide the text may be on this monitor, in logical pixels.
fn text_budget(monitor: &gdk::Monitor, style: &Style, pad: f64) -> i32 {
    let geom = monitor.geometry();
    // Margins only bite on an anchored axis; a centred one keeps the full width.
    let margins = if style.halign == Align::Center {
        0
    } else {
        style.margin
    };
    (geom.width() - margins - (pad * 2.0) as i32).max(1)
}

fn draw(cr: &gtk::cairo::Context, hud: &Hud, layout: &pango::Layout, phase: Phase, caret_on: bool) {
    let total = hud.timeline.chars();
    let (mut visible, vanish_p) = match phase {
        Phase::Reveal { chars } => (chars, 0.0),
        Phase::Hold => (total, 0.0),
        Phase::Vanish { p } => (total, p),
        Phase::Done => return,
    };

    // Bind the cached layout to this cairo context (font options, resolution)
    // without re-shaping it.
    pangocairo::functions::update_layout(cr, layout);
    let (_, th) = layout.pixel_size();
    let th = th as f64;
    let pad = hud.pad();

    // Untype isn't a paint effect: it runs the reveal backwards, so it edits
    // the visible-character count and everything downstream just follows.
    if vanish_p > 0.0 && hud.style.vanish.is_untype() {
        visible = ((1.0 - vanish_p) * total as f64).ceil() as usize;
    }

    let _ = cr.save();
    cr.translate(pad, pad);

    // Effects that change geometry or colour, applied before painting.
    let mut alpha = 1.0_f64;
    let mut whiten = 0.0_f64;
    if vanish_p > 0.0 {
        match hud.style.vanish {
            Vanish::Fade { .. } => alpha = 1.0 - vanish_p,
            Vanish::Collapse { .. } => {
                // CRT power-off: squash toward the middle line, bloom slightly
                // wider, wash to white, then blink out over the last 15%.
                let sy = (1.0 - vanish_p).powf(1.8).max(0.002);
                let sx = 1.0 + 0.06 * vanish_p;
                let mid = th / 2.0;
                cr.translate(0.0, mid);
                cr.scale(sx, sy);
                cr.translate(0.0, -mid);
                whiten = vanish_p * 0.8;
                alpha = if vanish_p < 0.85 {
                    1.0
                } else {
                    (1.0 - vanish_p) / 0.15
                };
            }
            Vanish::Instant | Vanish::Untype { .. } => {}
            // Handled after painting, as a mask.
            Vanish::Wash { .. } | Vanish::Dissolve { .. } => {}
        }
    }

    // Mask effects need the finished glyphs as a source, so they paint into a
    // group first. Doing it the other way round would mask the stroke and the
    // fill separately and leave the outline behind.
    let masked = vanish_p > 0.0
        && matches!(
            hud.style.vanish,
            Vanish::Wash { .. } | Vanish::Dissolve { .. }
        );
    if masked {
        cr.push_group();
    }

    paint_text(cr, layout, hud, visible, total, alpha, whiten, pad);
    // Blink state is decided once per tick; recomputing it here would be a
    // second source of truth for the same thing.
    if caret_on {
        let caret = caret_rect(layout, &hud.text, visible);
        set_color(cr, hud.fill, alpha, whiten);
        cr.rectangle(caret.x, caret.y, caret.w, caret.h);
        let _ = cr.fill();
    }

    if masked {
        let _ = cr.pop_group_to_source();
        match hud.style.vanish {
            Vanish::Wash { dir, .. } => {
                let _ = cr.mask(wash_gradient(th, vanish_p, dir));
            }
            Vanish::Dissolve { .. } => {
                let (tw, _) = layout.pixel_size();
                if let Some(surface) = dissolve_mask(tw as f64, th, pad, vanish_p) {
                    let _ = cr.mask_surface(&surface, -pad, -pad);
                }
            }
            // Unreachable given how `masked` is computed, but a panic inside a
            // draw callback aborts the process on top of the screen; leaving
            // the group unmasked just shows the text.
            _ => {}
        }
    }

    let _ = cr.restore();
}

/// Stroke + fill the glyphs, clipped to whatever has been typed so far.
#[allow(clippy::too_many_arguments)]
fn paint_text(
    cr: &gtk::cairo::Context,
    layout: &pango::Layout,
    hud: &Hud,
    visible: usize,
    total: usize,
    alpha: f64,
    whiten: f64,
    pad: f64,
) {
    if visible < total {
        let caret = caret_rect(layout, &hud.text, visible);
        let (w, _) = layout.pixel_size();
        // Everything above the caret's line, plus the typed part of that line.
        cr.rectangle(-pad, -pad, w as f64 + pad * 2.0, caret.y + pad);
        cr.rectangle(-pad, caret.y, caret.x + pad, caret.h);
        cr.clip();
    }

    cr.move_to(0.0, 0.0);
    pangocairo::functions::layout_path(cr, layout);
    if let Some(o) = hud.outline {
        set_color(cr, o, alpha, whiten);
        cr.set_line_width(hud.outline_width);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        let _ = cr.stroke_preserve();
    }
    set_color(cr, hud.fill, alpha, whiten);
    let _ = cr.fill();

    cr.reset_clip();
}

/// A soft edge sweeping through the text. The gradient is padded on both
/// sides so the sweep starts fully off the text and ends fully past it.
fn wash_gradient(th: f64, p: f64, dir: Dir) -> gtk::cairo::LinearGradient {
    let soft = (th * 0.35).max(8.0);
    let span = th + soft * 2.0;
    match dir {
        Dir::Down => {
            let front = -soft + p * span;
            let g = gtk::cairo::LinearGradient::new(0.0, front, 0.0, front + soft);
            g.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.0);
            g.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 1.0);
            g
        }
        Dir::Up => {
            let front = th + soft - p * span;
            let g = gtk::cairo::LinearGradient::new(0.0, front - soft, 0.0, front);
            g.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 1.0);
            g.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
            g
        }
    }
}

/// An A8 mask of surviving blocks. Each block has a fixed pseudo-random
/// lifetime, so the decay pattern is stable frame to frame instead of
/// re-randomising into static.
fn dissolve_mask(tw: f64, th: f64, pad: f64, p: f64) -> Option<gtk::cairo::ImageSurface> {
    let w = (tw + pad * 2.0).ceil() as i32;
    let h = (th + pad * 2.0).ceil() as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    let block = (th / 9.0).clamp(6.0, 40.0);
    let surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).ok()?;
    {
        let mcr = gtk::cairo::Context::new(&surface).ok()?;
        // Colour is ignored in A8; only the alpha channel survives.
        mcr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        let cols = (w as f64 / block).ceil() as i32;
        let rows = (h as f64 / block).ceil() as i32;
        for by in 0..rows {
            for bx in 0..cols {
                if block_life(bx, by) >= p {
                    mcr.rectangle(bx as f64 * block, by as f64 * block, block, block);
                }
            }
        }
        let _ = mcr.fill();
    }
    Some(surface)
}

/// Deterministic 0.0..1.0 per block position.
fn block_life(bx: i32, by: i32) -> f64 {
    let mut h = (bx as u32).wrapping_mul(73_856_093) ^ (by as u32).wrapping_mul(19_349_663);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h % 10_000) as f64 / 10_000.0
}

/// Caret box in widget pixels.
struct Caret {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn caret_rect(layout: &pango::Layout, text: &str, visible_chars: usize) -> Caret {
    let byte = text
        .char_indices()
        .nth(visible_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let r = layout.index_to_pos(byte as i32);
    let sc = pango::SCALE as f64;
    // A right-to-left run reports a negative width with x at the right edge;
    // normalising here keeps the clip rectangle from collapsing.
    let x = (r.x() as f64 + r.width().min(0) as f64) / sc;
    let h = r.height() as f64 / sc;
    // index_to_pos gives the advance of the character the caret sits ON, which
    // is exactly the terminal block width. Past the end of the text there is no
    // such character, so fall back to half the line height.
    let w = match (r.width().abs() as f64) / sc {
        w if w > 0.0 => w,
        _ => h * 0.5,
    };
    Caret {
        x,
        y: r.y() as f64 / sc,
        w,
        h,
    }
}

fn show_caret(reveal: &Reveal, vanish: &Vanish, phase: Phase, t_ms: f64) -> bool {
    // An explicit `cursor = false` wins everywhere.
    if matches!(reveal, Reveal::Typewriter { cursor: false, .. }) {
        return false;
    }
    let typing = matches!(reveal, Reveal::Typewriter { .. });
    match phase {
        Phase::Reveal { .. } => typing,
        // Keep blinking while the text is up: a terminal that stops blinking
        // reads as a hung terminal.
        Phase::Hold => typing && ((t_ms / 530.0) as u64).is_multiple_of(2),
        // Untype IS the caret eating the text, so it gets one even when the
        // text arrived instantly — otherwise characters vanish untouched.
        Phase::Vanish { .. } => vanish.is_untype(),
        Phase::Done => false,
    }
}

fn set_color(cr: &gtk::cairo::Context, c: gdk::RGBA, alpha: f64, whiten: f64) {
    let mix = |v: f32| (v as f64) * (1.0 - whiten) + whiten;
    cr.set_source_rgba(
        mix(c.red()),
        mix(c.green()),
        mix(c.blue()),
        c.alpha() as f64 * alpha,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const TW: Reveal = Reveal::Typewriter {
        cps: 20.0,
        cursor: true,
        jitter: 0.0,
    };
    const NO_CURSOR: Reveal = Reveal::Typewriter {
        cps: 20.0,
        cursor: false,
        jitter: 0.0,
    };
    const UNTYPE: Vanish = Vanish::Untype { ms: 300 };
    const FADE: Vanish = Vanish::Fade { ms: 300 };

    #[test]
    fn outline_scales_with_the_font_unless_pinned() {
        let w = |spec: &str| outline_width(&pango::FontDescription::from_string(spec), None);
        // 72pt keeps the hand-picked 5.0; the rest follow the same ratio.
        assert!((w("LythMono Nerd Font 72") - 72.0 / 14.0).abs() < 1e-9);
        assert!(w("LythMono Nerd Font 24") < w("LythMono Nerd Font 48"));
        // A configured value stays absolute, however odd.
        let pinned = outline_width(
            &pango::FontDescription::from_string("LythMono Nerd Font 24"),
            Some(9.0),
        );
        assert_eq!(pinned, 9.0);
    }

    #[test]
    fn outline_width_survives_a_font_with_no_size() {
        // from_string("Sans") leaves size at 0; scaling against that would
        // give a zero-width stroke that silently draws nothing.
        let w = outline_width(&pango::FontDescription::from_string("Sans"), None);
        assert!(w > 0.0, "got {w}");
    }

    #[test]
    fn negative_configured_width_is_clamped_not_passed_to_cairo() {
        let w = outline_width(&pango::FontDescription::from_string("Sans 20"), Some(-3.0));
        assert_eq!(w, 0.0);
    }

    /// A layout built straight from pangocairo, with no GTK widget and no
    /// gtk::init — enough to exercise the geometry.
    fn bare_layout(text: &str, font: &str) -> pango::Layout {
        let ctx = pangocairo::FontMap::default().create_context();
        let layout = pango::Layout::new(&ctx);
        layout.set_font_description(Some(&pango::FontDescription::from_string(font)));
        layout.set_text(text);
        layout
    }

    #[test]
    fn alignment_does_not_push_the_text_out_of_the_window() {
        // The bug: alignment placed lines inside the wrapping budget (most of
        // the screen) while the window was sized to the text, so anything but
        // Left was drawn entirely outside the surface — a blank screen.
        for align in [
            pango::Alignment::Left,
            pango::Alignment::Center,
            pango::Alignment::Right,
        ] {
            let layout = bare_layout("REPRO", "Sans 36");
            layout.set_alignment(align);
            fit_width(&layout, 1354);
            let (text_width, _) = layout.pixel_size();
            let first_x = layout.index_to_pos(0).x() / pango::SCALE;
            assert!(
                first_x < text_width,
                "{align:?}: first glyph at {first_x} is outside a {text_width}px window"
            );
        }
    }

    #[test]
    fn a_line_too_long_for_the_budget_still_wraps() {
        // Shrinking the width must not undo the wrapping it was set for.
        let long = "wraps ".repeat(80);
        let layout = bare_layout(&long, "Sans 36");
        layout.set_alignment(pango::Alignment::Center);
        fit_width(&layout, 600);
        assert!(layout.line_count() > 1, "text did not wrap");
        let (text_width, _) = layout.pixel_size();
        assert!(
            text_width <= 600,
            "wrapped width {text_width} exceeds the budget"
        );
    }

    #[test]
    fn untype_gets_a_caret_even_after_an_instant_reveal() {
        let v = Phase::Vanish { p: 0.5 };
        assert!(show_caret(&Reveal::Instant, &UNTYPE, v, 0.0));
        // ...but an instant reveal has nothing to type, so no caret before it.
        assert!(!show_caret(&Reveal::Instant, &UNTYPE, Phase::Hold, 0.0));
    }

    #[test]
    fn other_vanishes_drop_the_caret() {
        assert!(!show_caret(&TW, &FADE, Phase::Vanish { p: 0.5 }, 0.0));
        assert!(!show_caret(&TW, &UNTYPE, Phase::Done, 0.0));
    }

    #[test]
    fn cursor_false_disables_it_everywhere() {
        for phase in [
            Phase::Reveal { chars: 1 },
            Phase::Hold,
            Phase::Vanish { p: 0.5 },
        ] {
            assert!(!show_caret(&NO_CURSOR, &UNTYPE, phase, 0.0));
        }
    }

    #[test]
    fn hold_blinks_rather_than_staying_lit() {
        assert!(show_caret(&TW, &FADE, Phase::Hold, 0.0));
        assert!(!show_caret(&TW, &FADE, Phase::Hold, 600.0));
        assert!(show_caret(&TW, &FADE, Phase::Hold, 1100.0));
    }

    #[test]
    fn dissolve_blocks_are_stable_and_in_range() {
        // Same block must always report the same lifetime, or the decay
        // re-randomises every frame and reads as static instead of a dissolve.
        for (x, y) in [(0, 0), (3, 7), (41, 2)] {
            let a = block_life(x, y);
            assert_eq!(a, block_life(x, y));
            assert!((0.0..1.0).contains(&a), "{a} out of range");
        }
        // Neighbours must not decay together, or it looks like a wipe.
        assert_ne!(block_life(5, 5), block_life(6, 5));
    }
}
