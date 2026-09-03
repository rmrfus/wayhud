//! One layer-shell window per output, drawn by hand with pango + cairo.
//!
//! Why not a GtkLabel and CSS: GTK4's CSS has no text-stroke, and an outline
//! faked with eight text-shadows falls apart at 72pt. Going through a cairo
//! path gives a real stroke, and the typewriter clip and the caret come for
//! free in the same draw call.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, LayerShell};

use crate::config::{Dir, Glow, HAlign, LineAlign, Reveal, Style, VAlign, Vanish};
use crate::timeline::{Phase, Timeline};

/// A message with everything already parsed and validated, so nothing can fail
/// once we're inside a draw callback.
pub struct Hud {
    pub style: Style,
    pub text: String,
    pub timeline: Timeline,
    fill: gdk::RGBA,
    outline: Option<gdk::RGBA>,
    /// Parsed colour plus the knobs, resolved once so the draw callback cannot
    /// fail. `None` when the style asks for no glow, or asks for radius 0.
    glow: Option<(gdk::RGBA, Glow)>,
    font: pango::FontDescription,
    outline_width: f64,
    /// Width of the caret drawn PAST the last character, where `index_to_pos`
    /// reports none and `caret_rect` falls back to half a line height.
    ///
    /// Measured from a throwaway layout rather than guessed from the point
    /// size. The guess was `points * 0.7`, on the stated assumption that a
    /// line is at most 1.4x the size — DejaVu Sans is 1.57x at 72pt, so the
    /// reserve was already 5px short of the caret it was reserving for, and
    /// only the 8px of slack in `pad` kept the caret from being clipped at the
    /// surface edge. A font with taller vertical metrics spends that slack.
    caret_fallback: f64,
}

impl Hud {
    /// `seed` drives the typewriter jitter; pass a fixed one to make a run
    /// reproducible.
    pub fn new(style: Style, text: String, seed: u64) -> Result<Hud> {
        let fill = gdk::RGBA::parse(&style.color)
            .with_context(|| format!("bad color {:?}", style.color))?;
        let outline = match style.outline.as_deref() {
            // "none" as well as an absent key: TOML has no null, so a preset
            // inheriting an outline from [style.default] has no other way to
            // take it back off.
            None | Some("none") => None,
            Some(c) => {
                Some(gdk::RGBA::parse(c).with_context(|| format!("bad outline color {c:?}"))?)
            }
        };
        // radius 0 collapses to "no glow" here rather than at every use site:
        // it is how a preset switches off a glow inherited from the base, and
        // a zero-radius blur would otherwise allocate a mask to paint nothing.
        let glow = match &style.glow {
            Some(g) if g.radius > 0.0 => {
                let rgba = gdk::RGBA::parse(&g.color)
                    .with_context(|| format!("bad glow color {:?}", g.color))?;
                Some((rgba, g.clone()))
            }
            _ => None,
        };
        let font = pango::FontDescription::from_string(&style.font);
        // from_string never fails — it just yields an empty family that
        // silently renders in the default font, which looks like a bug.
        anyhow::ensure!(
            font.family().is_some(),
            "font {:?} has no family; expected something like \
             \"Monospace 72\"",
            style.font
        );
        // A layout off the default font map, not the widget's: this runs
        // before gtk::init and only the font's own line height is wanted.
        let caret_fallback = {
            let ctx = pangocairo::FontMap::default().create_context();
            let probe = pango::Layout::new(&ctx);
            probe.set_font_description(Some(&font));
            probe.set_text("M");
            caret_rect(&probe, "M", 1).w
        };
        let timeline = Timeline::new(&text, &style.reveal, style.timeout_ms, &style.vanish, seed);
        let outline_width = outline_width(&font, style.outline_width);
        Ok(Hud {
            fill,
            outline,
            glow,
            outline_width,
            caret_fallback,
            font,
            timeline,
            style,
            text,
        })
    }

    /// Padding around the text box.
    ///
    /// Covers the stroke, which straddles the glyph outline, and the caret,
    /// which is drawn PAST the last character: `index_to_pos` reports zero
    /// width there, so `draw` falls back to half the line height. A fixed 8px
    /// was never enough for that — at 72pt the caret is about 45px wide and
    /// was clipped to a sliver whenever the last line was also the longest.
    fn pad(&self) -> f64 {
        let stroke = self.outline_width.max(0.0).ceil();
        // The halo needs room of its own or the surface edge cuts it into a
        // straight line, which is the one thing a glow must not have.
        let glow = self
            .glow
            .as_ref()
            .map_or(0.0, |(_, g)| blur_passes(g.radius).1.ceil());
        // Exactly the caret `caret_rect` will produce past the last character,
        // measured rather than derived from the point size, so the reserve and
        // the thing reserved for cannot disagree.
        let caret = if matches!(self.style.reveal, Reveal::Typewriter { cursor: true, .. }) {
            self.caret_fallback
        } else {
            0.0
        };
        stroke + glow + caret.ceil() + 8.0
    }

    /// `max_width` is the widest the text block may get, in logical pixels.
    /// Without it a long line silently runs off both edges of the output:
    /// the layer surface is sized from the text, and the compositor clips
    /// whatever doesn't fit on screen.
    fn layout_for(&self, widget: &gtk::DrawingArea, max_width: i32) -> pango::Layout {
        let layout = widget.create_pango_layout(Some(&self.text));
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
    match font_points(font) {
        // No size in the description at all; pango will pick its own default,
        // so there is nothing to scale against.
        p if p <= 0.0 => 1.0,
        p => (p / 14.0).max(0.5),
    }
}

/// Font size in points, 0.0 when the description does not carry one.
fn font_points(font: &pango::FontDescription) -> f64 {
    let size = font.size();
    if size <= 0 {
        return 0.0;
    }
    // An absolute size is already in device units rather than points.
    if font.is_size_absolute() {
        size as f64 / pango::SCALE as f64 * 72.0 / 96.0
    } else {
        size as f64 / pango::SCALE as f64
    }
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

/// A blurred A8 image of the REVEALED glyphs, ready to mask the glow colour.
///
/// The reveal is clipped here, before the blur, and that is the whole point.
/// Clipping the finished halo with a rectangle instead leaves a hard edge
/// wherever the blurred mask is still bright at the boundary — which, at any
/// useful radius, is a straight bright line down the screen at the caret and
/// another under the line being typed. Cutting the ink first lets the halo
/// fade off the last typed glyph the way it fades off every other edge.
///
/// The price is that the mask is no longer built once: it depends on how many
/// characters are showing. The caller rebuilds it when that count changes —
/// per keystroke, not per frame — and only the revealed corner is blurred, so
/// early characters cost a fraction of a full message.
///
/// Rendered at DEVICE resolution: everything else here is in logical pixels,
/// but a mask built at logical size and scaled up is a blur of a blur, soft on
/// exactly the HiDPI outputs this runs on.
fn glow_mask(
    layout: &pango::Layout,
    text: &str,
    visible: usize,
    total: usize,
    radius: f64,
    pad: f64,
    scale: f64,
) -> Option<gtk::cairo::ImageSurface> {
    let (tw, th) = layout.pixel_size();
    let w = (((tw as f64) + pad * 2.0) * scale).ceil() as i32;
    let h = (((th as f64) + pad * 2.0) * scale).ceil() as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    let (pass, reach) = blur_passes(radius);
    let mut surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).ok()?;
    // How much of the surface the blur has to touch. Everything outside is
    // zero and stays zero, so blurring it is pure work for no pixels.
    let (mut live_w, mut live_h) = (w, h);
    {
        let mcr = gtk::cairo::Context::new(&surface).ok()?;
        mcr.scale(scale, scale);
        mcr.translate(pad, pad);
        if visible < total {
            let caret = caret_rect(layout, text, visible);
            // Whole lines above the caret, then the typed part of its line.
            // No slack past the caret: an unrevealed glyph must not reach the
            // mask at all, and no room below the line box either, because the
            // blur will carry the halo past it on its own.
            mcr.rectangle(-pad, -pad, f64::from(tw) + pad * 2.0, caret.y + pad);
            mcr.rectangle(-pad, caret.y, pad + caret.x, caret.h);
            mcr.clip();
            // The live region has to cover everything the clip admits, and the
            // first rectangle admits WHOLE LINES at full width. Narrowing it to
            // the caret's corner left the completed lines' ink unblurred past
            // that point, which shows as the halo stopping dead mid-line.
            live_w = if caret.y > 0.0 {
                w
            } else {
                (((pad + caret.x + reach) * scale).ceil() as i32).clamp(1, w)
            };
            live_h = ((((pad + caret.y + caret.h + reach) * scale).ceil()) as i32).clamp(1, h);
        }
        // Colour is ignored in A8; only the coverage matters.
        mcr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        mcr.move_to(0.0, 0.0);
        pangocairo::functions::layout_path(&mcr, layout);
        let _ = mcr.fill();
    }
    surface.flush();

    let stride = surface.stride() as usize;
    let (uw, uh) = (live_w as usize, live_h as usize);
    let mut buf = vec![0u8; uw * uh];
    {
        let data = surface.data().ok()?;
        for y in 0..uh {
            buf[y * uw..y * uw + uw].copy_from_slice(&data[y * stride..y * stride + uw]);
        }
    }
    blur(
        &mut buf,
        uw,
        uh,
        ((pass as f64) * scale).round().max(1.0) as usize,
    );
    {
        let mut data = surface.data().ok()?;
        for y in 0..uh {
            data[y * stride..y * stride + uw].copy_from_slice(&buf[y * uw..y * uw + uw]);
        }
    }
    surface.mark_dirty();
    Some(surface)
}

/// Per-pass radius for a halo that is to reach `reach` pixels, and the reach
/// it actually achieves.
///
/// Three box passes spread THREE times their radius, not one. Reserving a
/// single radius of room, as everything here first did, truncates the halo at
/// the surface edge: measured on a block blurred at radius 16 with 16px of
/// margin, the very edge still reads 38 of a 221 peak — a hard rectangle
/// around the text rather than a glow. With 3r of margin it reads 0.
///
/// Dividing here rather than widening the padding keeps `radius` meaning what
/// the documentation says it means, which is how far the light carries; the
/// alternative was a padding three times the number the user typed, taken out
/// of the width that decides where lines wrap.
fn blur_passes(reach: f64) -> (usize, f64) {
    // Support of three passes of radius p is exactly 3p, and the pixel AT 3p
    // still takes a share — so the room needed is 3p + 1 and the arithmetic
    // runs backwards from `reach` rather than forwards from the radius. The
    // halo then reaches at most what was asked for and is zero by the edge.
    let pass = ((reach - 1.0) / 3.0).floor().max(0.0);
    if pass < 1.0 {
        return (0, 0.0);
    }
    (pass as usize, pass * 3.0 + 1.0)
}

/// Three box passes, which approximate a Gaussian closely enough for a halo.
///
/// The passes run in `f32` and quantise to a byte once at the end. Rounding
/// back to `u8` between passes loses most of a thin stroke: traced on a single
/// lit pixel at radius 3 it went 255 -> 36 -> 5 -> 1 while the image total
/// fell from 255 to 41, because `255/9` is 28, `28/9` is 3 and `3/9` is 0. A
/// glyph stem against a 12px radius is exactly that sparse, so the visible
/// result was no halo at all.
///
/// Each pass is a moving sum, so the cost is one add and one subtract per
/// pixel whatever the radius — the naive form is O(radius) per pixel and turns
/// a 12px halo into eleven times the work for the same picture.
fn blur(buf: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let mut a: Vec<f32> = buf.iter().map(|&v| f32::from(v)).collect();
    let mut b = vec![0.0f32; w * h];
    for _ in 0..3 {
        box_pass(&a, &mut b, w, h, r, 1, w);
        box_pass(&b, &mut a, h, w, r, w, 1);
    }
    for (dst, src) in buf.iter_mut().zip(a) {
        *dst = src.round().clamp(0.0, 255.0) as u8;
    }
}

/// One box pass along `step`, over `runs` lines of `len` pixels.
///
/// `step`/`lead` let the same code run horizontally and vertically: the caller
/// swaps them rather than transposing the buffer.
///
/// The divisor is the FULL window even where it hangs off the edge, so the
/// missing pixels count as zero and the halo fades into nothing. Normalising
/// by the clamped count instead would keep the edge at full brightness and
/// leave a bright rim around the surface.
fn box_pass(
    src: &[f32],
    dst: &mut [f32],
    len: usize,
    runs: usize,
    r: usize,
    step: usize,
    lead: usize,
) {
    let inv = 1.0 / (2 * r + 1) as f32;
    for run in 0..runs {
        let base = run * lead;
        // Bounds checks dominate this loop — three per pixel through an
        // indexing closure measured 3.3ns per operation, an order off what an
        // add and a subtract cost. Sub-slicing once per run lets the compiler
        // drop them.
        let src = &src[base..];
        let dst = &mut dst[base..];
        let mut sum: f32 = (0..=r.min(len - 1)).map(|i| src[i * step]).sum();
        for i in 0..len {
            dst[i * step] = sum * inv;
            if i + r + 1 < len {
                sum += src[(i + r + 1) * step];
            }
            if i >= r {
                sum -= src[(i - r) * step];
            }
        }
    }
}

/// How many characters are on screen in this phase.
///
/// One place, because the glow mask and the draw path have to agree on it: a
/// mask built for a different count than the ink is drawn with shows as a halo
/// that leads or trails the text.
fn visible_chars(hud: &Hud, phase: Phase) -> usize {
    let total = hud.timeline.chars();
    match phase {
        Phase::Reveal { chars } => chars,
        Phase::Hold => total,
        Phase::Vanish { p } if hud.style.vanish.is_untype() => hud.timeline.untype_visible(p),
        Phase::Vanish { .. } => total,
        Phase::Done => total,
    }
}

/// Frame state shared between the tick callback and the draw callback.
struct Frame {
    t0: Cell<Option<i64>>,
    phase: Cell<Phase>,
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
    // Blurred here for the same reason the layout is: both depend only on the
    // text and the font, and the vanish redraws every frame. The scale is the
    // monitor's, not the display's — two outputs can differ, and the mask has
    // to match the one it will be painted on.
    // The mask depends on how many characters are showing, so it is cached by
    // that count and rebuilt when it changes — per keystroke while typing, per
    // erased character while untyping, never per frame. `usize::MAX` is the
    // "nothing built yet" marker, since 0 is a legitimate count.
    let glow_scale = f64::from(monitor.scale_factor());
    let glow_cache: RefCell<(usize, Option<Rc<gtk::cairo::ImageSurface>>)> =
        RefCell::new((usize::MAX, None));
    let (tw, th) = layout.pixel_size();
    area.set_content_width(tw + (pad * 2.0) as i32);
    area.set_content_height(th + (pad * 2.0) as i32);

    let frame = Rc::new(Frame {
        t0: Cell::new(None),
        phase: Cell::new(Phase::Reveal { chars: 0 }),
        blink: Cell::new(false),
    });

    area.set_draw_func({
        let hud = hud.clone();
        let frame = frame.clone();
        move |_area, cr, _w, _h| {
            let phase = frame.phase.get();
            let glow = hud.glow.as_ref().and_then(|(rgba, g)| {
                let visible = visible_chars(&hud, phase);
                let mut cache = glow_cache.borrow_mut();
                if cache.0 != visible {
                    let mask = glow_mask(
                        &layout,
                        &hud.text,
                        visible,
                        hud.timeline.chars(),
                        g.radius,
                        pad,
                        glow_scale,
                    );
                    *cache = (visible, mask.map(Rc::new));
                }
                cache.1.clone().map(|m| (m, *rgba, g.alpha))
            });
            draw(
                cr,
                &hud,
                &layout,
                glow.as_ref().map(|(m, c, a)| (m.as_ref(), *c, *a)),
                phase,
                frame.blink.get(),
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
            let blink = show_caret(&hud.style.reveal, &hud.style.vanish, phase, t_ms);
            let changed = phase != frame.phase.get() || blink != frame.blink.get();
            frame.phase.set(phase);
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
        HAlign::Left => {
            window.set_anchor(Edge::Left, true);
            window.set_margin(Edge::Left, style.margin);
        }
        HAlign::Right => {
            window.set_anchor(Edge::Right, true);
            window.set_margin(Edge::Right, style.margin);
        }
        HAlign::Center => {}
    }
    match style.valign {
        VAlign::Top => {
            window.set_anchor(Edge::Top, true);
            window.set_margin(Edge::Top, style.margin);
        }
        VAlign::Bottom => {
            window.set_anchor(Edge::Bottom, true);
            window.set_margin(Edge::Bottom, style.margin);
        }
        VAlign::Center => {}
    }
}

/// How wide the text may be on this monitor, in logical pixels.
fn text_budget(monitor: &gdk::Monitor, style: &Style, pad: f64) -> i32 {
    let geom = monitor.geometry();
    // Margins only bite on an anchored axis; a centred one keeps the full width.
    let margins = if style.halign == HAlign::Center {
        0
    } else {
        style.margin
    };
    (geom.width() - margins - (pad * 2.0) as i32).max(1)
}

fn draw(
    cr: &gtk::cairo::Context,
    hud: &Hud,
    layout: &pango::Layout,
    glow: Option<(&gtk::cairo::ImageSurface, gdk::RGBA, f64)>,
    phase: Phase,
    caret_on: bool,
) {
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
        visible = hud.timeline.untype_visible(vanish_p);
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
                // About the centre on both axes: scaling from x=0 pushed the
                // right edge out of the surface and clipped it.
                let (tw, _) = layout.pixel_size();
                let (cx, cy) = (tw as f64 / 2.0, th / 2.0);
                cr.translate(cx, cy);
                cr.scale(sx, sy);
                cr.translate(-cx, -cy);
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

    if let Some((mask, colour, glow_alpha)) = glow {
        paint_glow(cr, mask, layout, colour, alpha * glow_alpha, whiten, pad);
    }
    // Blink state is decided once per tick; recomputing it here would be a
    // second source of truth for the same thing.
    let caret = caret_on.then(|| caret_rect(layout, &hud.text, visible));
    // Every halo goes down before any ink, so the caret's light sits under the
    // glyphs rather than over the letter it follows.
    if let (Some(caret), Some((_, colour, glow_alpha)), Some((_, g))) =
        (caret.as_ref(), glow.as_ref(), hud.glow.as_ref())
    {
        paint_caret_glow(cr, caret, *colour, g.radius, *glow_alpha, alpha, whiten);
    }
    paint_text(cr, layout, hud, visible, total, alpha, whiten, pad);
    if let Some(caret) = caret {
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

/// Paint the halo: the blurred mask, in the glow colour, under the glyphs.
///
/// No clipping here. The mask already contains only the revealed glyphs, which
/// is what lets the halo fade off the last typed one instead of being severed
/// by a rectangle — see `glow_mask`.
///
/// The mask is device-resolution while the context is in logical pixels, so it
/// is drawn under an inverse scale. It is offset by `-pad` because the mask was
/// rendered with the text at `+pad` inside it, and the caller has already
/// translated the context to the text origin.
fn paint_glow(
    cr: &gtk::cairo::Context,
    mask: &gtk::cairo::ImageSurface,
    layout: &pango::Layout,
    colour: gdk::RGBA,
    // `alpha` arrives with the halo's own opacity already multiplied into the
    // frame's, since nothing here needs the two apart.
    alpha: f64,
    whiten: f64,
    pad: f64,
) {
    let scale = f64::from(mask.width()) / ((f64::from(layout.pixel_size().0)) + pad * 2.0);
    if !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let _ = cr.save();
    set_color(cr, colour, alpha, whiten);
    cr.scale(1.0 / scale, 1.0 / scale);
    // Positions are in mask pixels from here on, hence pad through the scale.
    let _ = cr.mask_surface(mask, -pad * scale, -pad * scale);
    let _ = cr.restore();
}

/// The caret's own halo.
///
/// The cached text mask cannot carry it: the caret moves every keystroke,
/// while the mask is built once from the full string. So this blurs a
/// rectangle instead — cheap, because a caret is a few thousand pixels against
/// the message's megapixel, and it reuses the same `blur` the text mask does
/// so the two fall off identically.
///
/// Without it the caret is the one thing on screen not emitting light, which
/// reads as a flat object pasted into a glowing line rather than as the write
/// head of the same terminal.
fn paint_caret_glow(
    cr: &gtk::cairo::Context,
    caret: &Caret,
    colour: gdk::RGBA,
    radius: f64,
    glow_alpha: f64,
    alpha: f64,
    whiten: f64,
) {
    let (pass, reach) = blur_passes(radius.max(0.0));
    let r = reach;
    let w = (caret.w + r * 2.0).ceil() as i32;
    let h = (caret.h + r * 2.0).ceil() as i32;
    if w <= 0 || h <= 0 {
        return;
    }
    let Some(mut surface) = gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).ok()
    else {
        return;
    };
    {
        let Ok(mcr) = gtk::cairo::Context::new(&surface) else {
            return;
        };
        mcr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        mcr.rectangle(r, r, caret.w, caret.h);
        let _ = mcr.fill();
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let (uw, uh) = (w as usize, h as usize);
    let mut buf = vec![0u8; uw * uh];
    {
        let Ok(data) = surface.data() else { return };
        for y in 0..uh {
            buf[y * uw..y * uw + uw].copy_from_slice(&data[y * stride..y * stride + uw]);
        }
    }
    blur(&mut buf, uw, uh, pass);
    {
        let Ok(mut data) = surface.data() else { return };
        for y in 0..uh {
            data[y * stride..y * stride + uw].copy_from_slice(&buf[y * uw..y * uw + uw]);
        }
    }
    surface.mark_dirty();

    set_color(cr, colour, alpha * glow_alpha, whiten);
    let _ = cr.mask_surface(&surface, caret.x - r, caret.y - r);
}

/// Stroke + fill the glyphs, clipped to whatever has been typed so far.
#[expect(
    clippy::too_many_arguments,
    reason = "one draw call: layout, phase and the two colour modifiers travel together"
)]
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
        // The slack past the caret is the stroke, not `pad`: pad also reserves
        // caret and halo room, and using it here would reveal the leading edge
        // of the next glyph before it has been "typed".
        //
        // Both rectangles start at the surface edge, so the third argument is
        // a WIDTH that has to carry that `pad` as well as the distance to the
        // caret. Writing the right edge there instead left the revealed text
        // ending `pad` short of the caret — measured at 60px at 72pt with no
        // glow at all, and it grew with the glow radius, which widens pad.
        let slack = hud.outline_width.max(1.0);
        cr.rectangle(-pad, -pad, w as f64 + pad * 2.0, caret.y + pad);
        cr.rectangle(-pad, caret.y, pad + caret.x + slack, caret.h);
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
        // A multi-word family exercises the parse too: the size is the last
        // token, not the second one.
        assert!((w("Some Wide Family 72") - 72.0 / 14.0).abs() < 1e-9);
        assert!(w("Some Wide Family 24") < w("Some Wide Family 48"));
        // A configured value stays absolute, however odd.
        let pinned = outline_width(
            &pango::FontDescription::from_string("Some Wide Family 24"),
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

    /// Families to exercise the geometry against: the two fontconfig generics,
    /// which resolve anywhere, plus any of the named ones this machine has.
    ///
    /// Metrics differ per family — DejaVu Sans is 1.57x its point size per
    /// line, Liberation Sans 1.50x — so a geometry rule that holds for one is
    /// not shown to hold at all. A named family that is absent is skipped
    /// rather than failed; the generics guarantee the test still runs.
    fn probe_families() -> Vec<String> {
        let map = pangocairo::FontMap::default();
        let present: Vec<String> = map
            .list_families()
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        let mut out = vec!["Sans".to_string(), "Monospace".to_string()];
        for want in [
            "DejaVu Sans",
            "DejaVu Serif",
            "Liberation Sans",
            "Noto Sans",
            "Cantarell",
        ] {
            if present.iter().any(|p| p == want) {
                out.push(want.to_string());
            }
        }
        out
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
        // Line count with the budget alone, before the width is shrunk back.
        let reference = bare_layout(&long, "Sans 36");
        reference.set_alignment(pango::Alignment::Center);
        reference.set_width(600 * pango::SCALE);
        reference.set_wrap(pango::WrapMode::WordChar);
        let expected = reference.line_count();

        let layout = bare_layout(&long, "Sans 36");
        layout.set_alignment(pango::Alignment::Center);
        fit_width(&layout, 600);
        assert!(layout.line_count() > 1, "text did not wrap");
        assert_eq!(
            layout.line_count(),
            expected,
            "shrinking the width re-flowed the text"
        );
        let (text_width, _) = layout.pixel_size();
        assert!(
            text_width <= 600,
            "wrapped width {text_width} exceeds the budget"
        );
    }

    #[test]
    fn the_built_in_default_font_shapes_text() {
        // The default has to work on a box that has never heard of the
        // author's typeface, hence the fontconfig generic rather than a named
        // family. Whether the resolved face is really fixed-width is the
        // machine's business, not ours — what must hold everywhere is that the
        // description shapes something at all.
        let hud = Hud::new(Style::default(), "wayhud".into(), 1).unwrap();
        let (w, h) = bare_layout(&hud.text, &hud.style.font).pixel_size();
        assert!(w > 0 && h > 0, "the default font gave a {w}x{h} layout");
    }

    #[test]
    fn the_caret_follows_a_proportional_font() {
        // Nothing in the geometry may assume a fixed advance. Until this
        // test, caret_rect was never run against a shaped layout at all — the
        // caret tests cover show_caret, which is pure boolean logic, and pad()
        // reads the font description without resolving it. A caret placed as
        // "column times cell width" would have sailed through both. "iWiW" is
        // the classic pair: in a proportional face those glyphs differ.
        let text = "iWiW";
        let layout = bare_layout(text, "Sans 72");
        let mut last_x = f64::NEG_INFINITY;
        for i in 0..=text.chars().count() {
            let c = caret_rect(&layout, text, i);
            assert!(c.w > 0.0 && c.h > 0.0, "caret {i} is {}x{}", c.w, c.h);
            assert!(
                c.x > last_x,
                "caret {i} at x={} did not advance past {last_x}",
                c.x
            );
            last_x = c.x;
        }
    }

    #[test]
    fn padding_covers_the_caret_of_a_proportional_font() {
        // The surface is `text width + 2 * pad` and the text starts at `pad`,
        // so the caret past the last character has to fit in the right-hand
        // pad — including the fallback width, which is derived from the line
        // height rather than from any glyph.
        let style = Style {
            font: "Sans 72".into(),
            reveal: TW,
            ..Style::default()
        };
        let text = "iWiW";
        let hud = Hud::new(style, text.into(), 1).unwrap();
        let layout = bare_layout(text, &hud.style.font);
        let (tw, _) = layout.pixel_size();
        let end = caret_rect(&layout, text, text.chars().count());
        assert!(
            end.x + end.w <= tw as f64 + hud.pad(),
            "caret ends at {} outside a {tw}px layout with {} of padding",
            end.x + end.w,
            hud.pad()
        );
    }

    #[test]
    fn a_proportional_font_wraps_and_fits_like_a_monospaced_one() {
        // fit_width measures the shaped text rather than counting columns, so
        // a variable advance must not push the block past its budget.
        for font in ["Sans 36", "Monospace 36"] {
            let layout = bare_layout(&"iW ".repeat(60), font);
            layout.set_alignment(pango::Alignment::Center);
            fit_width(&layout, 600);
            let (w, _) = layout.pixel_size();
            assert!(
                w > 0 && w <= 600,
                "{font}: wrapped to {w}px of a 600px budget"
            );
        }
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
    fn padding_covers_the_caret_past_the_last_character_on_every_font() {
        // The caret drawn past the end falls back to half the line height, and
        // padding sized only for the stroke clipped it to a sliver. The bound
        // used to be `points * 0.7`, on the stated assumption that a line is at
        // most 1.4x the point size: DejaVu Sans is 1.57x, so the reserve was
        // already 5px short of what it reserved for and only the slack in
        // `pad` hid it. This asserts the identity instead of a number, across
        // every family the machine has.
        for family in probe_families() {
            let spec = format!("{family} 72");
            let hud = Hud::new(
                Style {
                    font: spec.clone(),
                    reveal: TW,
                    ..Style::default()
                },
                "text".into(),
                1,
            )
            .expect("hud should build");
            let layout = bare_layout("text", &spec);
            let past_end = caret_rect(&layout, "text", 4).w;
            assert!(
                hud.pad() >= past_end,
                "{spec}: pad {:.1} does not cover a {past_end:.1}px caret",
                hud.pad()
            );
        }
    }

    #[test]
    fn no_caret_means_no_caret_padding() {
        let style = Style {
            font: "Sans 72".into(),
            reveal: Reveal::Instant,
            ..Style::default()
        };
        let hud = Hud::new(style, "text".into(), 1).unwrap();
        assert!(
            hud.pad() < 30.0,
            "instant reveal should not reserve caret room"
        );
    }

    #[test]
    fn outline_none_switches_the_stroke_off() {
        // TOML has no null, so a preset inheriting an outline needs a value
        // that means "no outline".
        let style = Style {
            outline: Some("none".into()),
            ..Style::default()
        };
        assert!(Hud::new(style, "x".into(), 1).unwrap().outline.is_none());
    }

    #[test]
    fn the_blur_spreads_symmetrically_and_fades_at_the_edge() {
        // One lit pixel in the middle: after three box passes the energy must
        // sit around it evenly, and nothing may be brighter than the source.
        let (w, h, r) = (41usize, 41usize, 4usize);
        let mut buf = vec![0u8; w * h];
        buf[20 * w + 20] = 255;
        blur(&mut buf, w, h, r);
        let at = |x: usize, y: usize| buf[y * w + x];
        assert!(at(20, 20) > 0, "the centre went dark");
        for d in 1..=r {
            assert_eq!(
                at(20 - d, 20),
                at(20 + d, 20),
                "asymmetric horizontally at {d}"
            );
            assert_eq!(
                at(20, 20 - d),
                at(20, 20 + d),
                "asymmetric vertically at {d}"
            );
        }
        assert!(
            at(20, 20) < 255,
            "the centre kept all its energy, so nothing was spread"
        );
        // The property that catches the bug this test was written for: a box
        // filter moves energy around, it does not consume it. Rounding to a
        // byte between passes took 255 down to a total of 41 and a centre of
        // 1 — a glyph stem blurred into nothing at all.
        let total: u32 = buf.iter().map(|&v| u32::from(v)).sum();
        assert!(total > 200, "the blur ate the signal: 255 in, {total} out");
        // Normalising by the full window is what makes the halo die out rather
        // than leave a bright rim along the surface edge.
        assert_eq!(at(0, 0), 0, "energy reached a far corner");
    }

    #[test]
    fn a_lit_edge_pixel_does_not_wrap_to_the_other_side() {
        // A pass that ran off the end of a row into the next one would show up
        // here as light appearing on the opposite margin.
        let (w, h) = (16usize, 4usize);
        let mut buf = vec![0u8; w * h];
        buf[w] = 255; // leftmost pixel of row 1
        blur(&mut buf, w, h, 3);
        assert!(buf[w + 1] > 0, "no spread along the row");
        assert_eq!(buf[w - 1], 0, "wrapped onto the previous row");
        assert_eq!(buf[2 * w - 1], 0, "wrapped to the far end of the row");
    }

    #[test]
    fn padding_grows_with_the_glow_radius() {
        // The halo is drawn past the glyph edge; without room in the surface
        // the blur is cut into a straight line, which is the one artefact a
        // glow cannot survive.
        let bare = Hud::new(
            Style {
                font: "Sans 72".into(),
                reveal: Reveal::Instant,
                ..Style::default()
            },
            "x".into(),
            1,
        )
        .unwrap();
        let lit = Hud::new(
            Style {
                font: "Sans 72".into(),
                reveal: Reveal::Instant,
                glow: Some(crate::config::Glow {
                    radius: 20.0,
                    ..crate::config::Glow::default()
                }),
                ..Style::default()
            },
            "x".into(),
            1,
        )
        .unwrap();
        let reach = blur_passes(20.0).1;
        assert!(
            lit.pad() >= bare.pad() + reach,
            "pad {} does not cover a {reach}px halo over {}",
            lit.pad(),
            bare.pad()
        );
    }

    #[test]
    fn a_zero_radius_glow_is_no_glow_at_all() {
        // How a preset takes back a glow inherited from [style.default]: TOML
        // has no null, the same reason `outline` needs the literal "none".
        let hud = Hud::new(
            Style {
                glow: Some(crate::config::Glow {
                    radius: 0.0,
                    ..crate::config::Glow::default()
                }),
                ..Style::default()
            },
            "x".into(),
            1,
        )
        .unwrap();
        assert!(hud.glow.is_none());
        assert_eq!(
            hud.pad(),
            Hud::new(Style::default(), "x".into(), 1).unwrap().pad()
        );
    }

    /// Coverage of one pass, rendered into an A8 target and counted.
    fn lit_pixels(paint: impl Fn(&gtk::cairo::Context), w: i32, h: i32) -> usize {
        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("A8 target");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            paint(&cr);
        }
        surface.flush();
        let data = surface.data().expect("pixels");
        data.iter().filter(|&&v| v > 0).count()
    }

    #[test]
    fn the_halo_reaches_pixels_the_glyphs_do_not() {
        // Proves the glow pass paints rather than merely building a mask. The
        // sway session reachable from here renders nothing, so this is the only
        // place the halo is shown to arrive anywhere at all.
        let radius = 10.0;
        let style = Style {
            font: "Sans 48".into(),
            reveal: Reveal::Instant,
            glow: Some(crate::config::Glow {
                color: "#ffffff".into(),
                radius,
                alpha: 1.0,
            }),
            ..Style::default()
        };
        let hud = Hud::new(style, "o".into(), 1).expect("hud should build");
        let layout = bare_layout(&hud.text, &hud.style.font);
        let pad = hud.pad();
        let (tw, th) = layout.pixel_size();
        let (w, h) = (
            (f64::from(tw) + pad * 2.0) as i32,
            (f64::from(th) + pad * 2.0) as i32,
        );
        let mask = glow_mask(&layout, &hud.text, 1, 1, radius, pad, 1.0).expect("mask");
        let (colour, _) = hud.glow.as_ref().expect("glow resolved");

        let glyphs = lit_pixels(
            |cr| {
                cr.translate(pad, pad);
                cr.move_to(0.0, 0.0);
                pangocairo::functions::layout_path(cr, &layout);
                let _ = cr.fill();
            },
            w,
            h,
        );
        let halo = lit_pixels(
            |cr| {
                cr.translate(pad, pad);
                paint_glow(cr, &mask, &layout, *colour, 1.0, 0.0, pad);
            },
            w,
            h,
        );

        assert!(glyphs > 0, "the glyph itself did not render");
        assert!(
            halo > glyphs,
            "the halo covers {halo} pixels against the glyph's {glyphs}, so it \
             is not reaching past the ink"
        );
    }

    /// Rightmost lit pixel of a partial reveal, in text coordinates, plus the
    /// caret position it is supposed to reach and the padding in force.
    fn revealed_ink_end(radius: f64) -> (f64, f64, f64) {
        let glow = (radius > 0.0).then(|| crate::config::Glow {
            color: "#ffffff".into(),
            radius,
            alpha: 1.0,
        });
        let style = Style {
            font: "Sans 72".into(),
            reveal: TW,
            outline: None,
            glow,
            ..Style::default()
        };
        let text = "mmmmm";
        let hud = Hud::new(style, text.into(), 1).expect("hud should build");
        let layout = bare_layout(text, &hud.style.font);
        let pad = hud.pad();
        let (tw, th) = layout.pixel_size();
        let (w, h) = (
            (f64::from(tw) + pad * 2.0) as i32,
            (f64::from(th) + pad * 2.0) as i32,
        );
        let visible = 3usize;
        let caret = caret_rect(&layout, text, visible);
        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("target");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            cr.translate(pad, pad);
            paint_text(&cr, &layout, &hud, visible, text.len(), 1.0, 0.0, pad);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        let mut rightmost = 0usize;
        for y in 0..(h as usize) {
            for x in 0..(w as usize) {
                if data[y * stride + x] > 0 {
                    rightmost = rightmost.max(x);
                }
            }
        }
        (caret.x, rightmost as f64 - pad, pad)
    }

    #[test]
    fn the_revealed_text_reaches_the_caret_whatever_the_padding() {
        // The clip rectangles start at the surface edge, so the third argument
        // is a width that must carry `pad` as well as the distance to the
        // caret. Written as a right edge instead, the revealed text ended
        // `pad` short: 60px at 72pt with no glow, 124px at radius 64, because
        // the halo widens pad. On screen that is the caret running ahead of
        // the text with a gap between the two.
        // The tolerance is one glyph advance, not a pixel count: what the rule
        // says is "the text reaches the caret", and how far the ink of an `m`
        // stops short of its own advance is the font's business, not ours.
        let advance = caret_rect(&bare_layout("mmmmm", "Sans 72"), "mmmmm", 3).w;
        let mut gaps = Vec::new();
        for radius in [0.0f64, 12.0, 64.0] {
            let (caret_x, ink_end, pad) = revealed_ink_end(radius);
            let gap = caret_x - ink_end;
            assert!(
                gap < advance,
                "radius {radius}: text ends {gap:.1}px before the caret, more \
                 than the {advance:.1}px advance of a glyph (pad {pad:.1})"
            );
            gaps.push(gap);
        }
        // The defect was the gap tracking pad, so pin that it no longer does.
        let widest = gaps.iter().fold(f64::MIN, |a, &b| a.max(b));
        let tightest = gaps.iter().fold(f64::MAX, |a, &b| a.min(b));
        assert!(
            widest - tightest < 2.0,
            "the gap still follows the glow radius: {gaps:?}"
        );
    }

    #[test]
    fn the_halo_survives_below_the_line_being_typed() {
        // The clip for the caret's line was exactly its line box, but the halo
        // reaches a radius past it. Above, the rectangle covering the earlier
        // lines let it through; below there was nothing, so the glow was cut
        // flat along the bottom of the line until the whole message finished.
        let radius = 14.0;
        let style = Style {
            font: "Sans 72".into(),
            reveal: TW,
            outline: None,
            glow: Some(crate::config::Glow {
                color: "#ffffff".into(),
                radius,
                alpha: 1.0,
            }),
            ..Style::default()
        };
        let text = "mmmmm";
        let hud = Hud::new(style, text.into(), 1).expect("hud");
        let layout = bare_layout(text, &hud.style.font);
        let pad = hud.pad();
        let (tw, th) = layout.pixel_size();
        let (w, h) = (
            (f64::from(tw) + pad * 2.0) as i32,
            (f64::from(th) + pad * 2.0) as i32,
        );
        let mask = glow_mask(
            &layout,
            text,
            text.chars().count(),
            text.chars().count(),
            radius,
            pad,
            1.0,
        )
        .expect("mask");
        let (colour, _) = hud.glow.as_ref().expect("glow");
        let visible = 3usize;
        let caret = caret_rect(&layout, text, visible);

        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("target");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            cr.translate(pad, pad);
            paint_glow(&cr, &mask, &layout, *colour, 1.0, 0.0, pad);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        // A vertical run crossing the bottom of the line box, under ink that
        // has been revealed. The old clip severed the halo exactly there, so
        // what this looks for is a step, not a particular brightness: how far
        // below the box the light still carries is the radius's business.
        let x = (pad + caret.x / 2.0) as usize;
        let from = (pad + caret.y + caret.h / 2.0) as usize;
        let to = ((pad + caret.y + caret.h + 2.0 * radius) as usize).min(h as usize - 1);
        let run: Vec<i32> = (from..=to)
            .map(|y| i32::from(data[y * stride + x]))
            .collect();
        let peak = run.iter().copied().max().unwrap_or(0);
        let step = run
            .windows(2)
            .map(|p| (p[1] - p[0]).abs())
            .max()
            .unwrap_or(0);
        assert!(peak > 0, "no halo along the sampled column at all");
        assert!(
            step * 6 < peak,
            "the halo steps by {step} of a {peak} peak crossing the line box, \
             which is the clip cutting it rather than a falloff"
        );
    }

    #[test]
    fn the_blur_dies_out_inside_the_room_reserved_for_it() {
        // Three box passes spread three times their radius. Reserving one
        // radius of margin, which everything here first did, left the halo
        // hitting the surface edge at 38 of a 221 peak — a hard rectangle
        // around the text and another around the caret, which is what the
        // screenshots showed. `blur_passes` is the whole reason those two
        // numbers now agree.
        for reach in [9.0f64, 24.0, 48.0] {
            let (pass, actual) = blur_passes(reach);
            assert!(
                actual <= reach,
                "reach {reach} promises less room than the {actual} it spreads"
            );
            let m = actual as usize;
            let n = 60 + 2 * m;
            let mut buf = vec![0u8; n * n];
            for y in m..m + 60 {
                for x in m..m + 60 {
                    buf[y * n + x] = 255;
                }
            }
            blur(&mut buf, n, n, pass);
            let mid = n / 2;
            assert!(
                buf[mid * n] == 0,
                "reach {reach}: the halo still reads {} at the surface edge",
                buf[mid * n]
            );
            assert!(buf[mid * n + mid] > 0, "reach {reach}: nothing survived");
        }
    }

    #[test]
    fn the_caret_glows_past_its_own_rectangle() {
        // The caret used to be the only thing on screen not emitting light,
        // which read as a flat object pasted into a glowing line rather than
        // the write head of the same terminal.
        let caret = Caret {
            x: 20.0,
            y: 20.0,
            w: 18.0,
            h: 40.0,
        };
        let white = gdk::RGBA::parse("#ffffff").expect("colour");
        let solid = lit_pixels(
            |cr| {
                cr.rectangle(caret.x, caret.y, caret.w, caret.h);
                let _ = cr.fill();
            },
            120,
            120,
        );
        let halo = lit_pixels(
            |cr| paint_caret_glow(cr, &caret, white, 10.0, 1.0, 1.0, 0.0),
            120,
            120,
        );
        assert!(solid > 0, "the caret rectangle itself did not render");
        assert!(
            halo > solid,
            "the caret halo covers {halo} pixels against the caret's {solid}"
        );
    }

    #[test]
    fn a_zero_radius_caret_halo_paints_nothing_past_the_caret() {
        // radius 0 is the "no glow" setting; the caret must not gain a halo
        // the rest of the text does not have.
        let caret = Caret {
            x: 20.0,
            y: 20.0,
            w: 18.0,
            h: 40.0,
        };
        let white = gdk::RGBA::parse("#ffffff").expect("colour");
        let solid = lit_pixels(
            |cr| {
                cr.rectangle(caret.x, caret.y, caret.w, caret.h);
                let _ = cr.fill();
            },
            120,
            120,
        );
        let halo = lit_pixels(
            |cr| paint_caret_glow(cr, &caret, white, 0.0, 1.0, 1.0, 0.0),
            120,
            120,
        );
        assert_eq!(halo, solid, "a zero radius must be the bare rectangle");
    }

    #[test]
    fn the_halo_has_no_step_where_the_reveal_ends() {
        // The bug behind this: the halo was blurred from the whole message and
        // then clipped with a rectangle, so at the caret it fell from bright to
        // nothing between two pixels — a straight vertical line down the
        // screen, and another under the line being typed. Clipping the ink
        // before the blur instead lets it fade the way it does off every other
        // edge. What is asserted is the absence of a step, not the absence of
        // light: a halo that ends abruptly is the artefact, one that reaches
        // past the caret and dies out is correct.
        let radius = 16.0;
        let style = Style {
            font: "Sans 72".into(),
            reveal: TW,
            outline: None,
            glow: Some(crate::config::Glow {
                color: "#ffffff".into(),
                radius,
                alpha: 1.0,
            }),
            ..Style::default()
        };
        let text = "mmmmm";
        let hud = Hud::new(style, text.into(), 1).expect("hud");
        let layout = bare_layout(text, &hud.style.font);
        let pad = hud.pad();
        let (tw, th) = layout.pixel_size();
        let (w, h) = (
            (f64::from(tw) + pad * 2.0) as i32,
            (f64::from(th) + pad * 2.0) as i32,
        );
        let visible = 3usize;
        let mask = glow_mask(&layout, text, visible, 5, radius, pad, 1.0).expect("mask");
        let (colour, _) = hud.glow.as_ref().expect("glow");
        let caret = caret_rect(&layout, text, visible);

        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("target");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            cr.translate(pad, pad);
            paint_glow(&cr, &mask, &layout, *colour, 1.0, 0.0, pad);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");

        // A horizontal run through the middle of the line, from inside the
        // revealed text to well past where the clip used to cut.
        let y = (pad + caret.y + caret.h / 2.0) as usize;
        let from = (pad + caret.x - 3.0 * radius).max(0.0) as usize;
        let to = ((pad + caret.x + 3.0 * radius) as usize).min(w as usize - 1);
        let run: Vec<i32> = (from..=to)
            .map(|x| i32::from(data[y * stride + x]))
            .collect();
        let peak = run.iter().copied().max().unwrap_or(0);
        assert!(peak > 0, "no halo along the sampled row at all");
        let step = run
            .windows(2)
            .map(|p| (p[1] - p[0]).abs())
            .max()
            .unwrap_or(0);
        // Measured: a correct falloff gives a peak of 155 against a worst
        // step of 14, a ratio of 11. The truncated halo this guards against
        // dropped about 40% of the peak in one pixel, a ratio near 2.5 — which
        // is why the first version of this bound, at 2, let it through.
        assert!(
            step * 6 < peak,
            "the halo steps by {step} of a {peak} peak in one pixel, which is \
             an edge rather than a falloff"
        );
    }

    #[test]
    fn the_glow_mask_is_built_at_device_resolution() {
        // A mask built at logical size and scaled up is a blur of a blur, soft
        // on exactly the HiDPI outputs this runs on.
        let layout = bare_layout("glow", "Sans 40");
        let (tw, _) = layout.pixel_size();
        let pad = 24.0;
        for scale in [1.0, 2.0] {
            let mask =
                glow_mask(&layout, "glow", 4, 4, 8.0, pad, scale).expect("mask should build");
            let want = (((tw as f64) + pad * 2.0) * scale).ceil() as i32;
            assert_eq!(mask.width(), want, "scale {scale}");
        }
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
