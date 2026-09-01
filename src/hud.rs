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

use crate::config::{Align, LineAlign, Reveal, Style, Vanish};
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
}

impl Hud {
    pub fn new(style: Style, text: String) -> Result<Hud> {
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
        let timeline = Timeline::new(&text, &style.reveal, style.timeout_ms, &style.vanish);
        Ok(Hud {
            fill,
            outline,
            font,
            timeline,
            style,
            text,
        })
    }

    /// Padding around the text box: the stroke straddles the glyph outline, and
    /// the caret sits past the last character.
    fn pad(&self) -> f64 {
        self.style.outline_width.max(0.0).ceil() + 8.0
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
        if max_width > 0 {
            layout.set_width(max_width * pango::SCALE);
            // WordChar, not Word: a single unbroken token longer than the
            // screen (a path, a hash) has to break somewhere.
            layout.set_wrap(pango::WrapMode::WordChar);
        }
        layout
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
pub fn present(
    app: &gtk::Application,
    monitor: &gdk::Monitor,
    hud: Rc<Hud>,
    on_first_frame: Rc<dyn Fn()>,
) {
    let window = gtk::ApplicationWindow::new(app);
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
    let (tw, th) = hud.layout_for(&area, max_width).pixel_size();
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
        move |area, cr, _w, _h| {
            draw(
                area,
                cr,
                &hud,
                max_width,
                frame.phase.get(),
                frame.t_ms.get(),
            );
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
            let blink = show_caret(&hud.style.reveal, phase, t_ms);
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

    window.present();

    // Click-through. Without an empty input region the overlay eats pointer
    // events for whatever sits under it — which, at Layer::Overlay, is
    // everything. Only available once the surface exists, hence after present.
    if let Some(surface) = window.surface() {
        surface.set_input_region(Some(&gtk::cairo::Region::create()));
    }
}

fn apply_anchors(window: &gtk::ApplicationWindow, style: &Style) {
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

fn draw(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    hud: &Hud,
    max_width: i32,
    phase: Phase,
    t_ms: f64,
) {
    let total = hud.timeline.chars();
    let (visible, vanish_p) = match phase {
        Phase::Reveal { chars } => (chars, 0.0),
        Phase::Hold => (total, 0.0),
        Phase::Vanish { p } => (total, p),
        Phase::Done => return,
    };

    let layout = hud.layout_for(area, max_width);
    let (_, th) = layout.pixel_size();
    let pad = hud.pad();

    let _ = cr.save();
    cr.translate(pad, pad);

    // Vanish transform + alpha.
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
                let mid = th as f64 / 2.0;
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
            Vanish::Instant => {}
        }
    }

    // Caret position doubles as the typewriter clip boundary.
    let caret = caret_rect(&layout, &hud.text, visible);

    if visible < total {
        let Caret {
            x: cx,
            y: cy,
            h: ch,
            ..
        } = caret;
        let (w, _) = layout.pixel_size();
        // Everything above the caret's line, plus the typed part of that line.
        cr.rectangle(-pad, -pad, w as f64 + pad * 2.0, cy + pad);
        cr.rectangle(-pad, cy, cx + pad, ch);
        cr.clip();
    }

    cr.move_to(0.0, 0.0);
    pangocairo::functions::layout_path(cr, &layout);
    if let Some(o) = hud.outline {
        set_color(cr, o, alpha, whiten);
        cr.set_line_width(hud.style.outline_width);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        let _ = cr.stroke_preserve();
    }
    set_color(cr, hud.fill, alpha, whiten);
    let _ = cr.fill();

    cr.reset_clip();

    if show_caret(&hud.style.reveal, phase, t_ms) {
        set_color(cr, hud.fill, alpha, whiten);
        cr.rectangle(caret.x, caret.y, caret.w, caret.h);
        let _ = cr.fill();
    }

    let _ = cr.restore();
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

fn show_caret(reveal: &Reveal, phase: Phase, t_ms: f64) -> bool {
    let Reveal::Typewriter { cursor: true, .. } = reveal else {
        return false;
    };
    match phase {
        Phase::Reveal { .. } => true,
        // Keep blinking while the text is up: a terminal that stops blinking
        // reads as a hung terminal.
        Phase::Hold => ((t_ms / 530.0) as u64).is_multiple_of(2),
        _ => false,
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
