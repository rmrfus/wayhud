//! Layer-shell windows drawn with Pango and Cairo. Cairo paths provide
//! text outlines, which GTK4 CSS does not support.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, LayerShell};

use crate::config::{Dir, Glow, HAlign, LineAlign, Reveal, Scanlines, Style, VAlign, Vanish};
use crate::timeline::{Phase, Timeline};

/// Transparent padding in logical pixels, calculated separately for each axis.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pad {
    x: f64,
    y: f64,
}

/// Parsed and validated message data for draw callbacks.
pub struct Hud {
    pub style: Style,
    pub text: String,
    pub timeline: Timeline,
    fill: gdk::RGBA,
    outline: Option<gdk::RGBA>,
    /// Resolved glow parameters. Absent when disabled or radius is zero.
    glow: Option<(gdk::RGBA, Glow)>,
    /// Resolved scanline parameters. Absent when disabled or strength is zero.
    scanlines: Option<Scanlines>,
    font: pango::FontDescription,
    outline_width: f64,
    /// Fixed caret width in logical pixels, measured from the advance of `M`.
    /// Using the current glyph advance would resize it in proportional fonts.
    caret_width: f64,
}

impl Hud {
    /// `seed` drives the typewriter jitter; pass a fixed one to make a run
    /// reproducible.
    pub fn new(style: Style, text: String, seed: u64) -> Result<Hud> {
        let fill = gdk::RGBA::parse(&style.color)
            .with_context(|| format!("bad color {:?}", style.color))?;
        let outline = match style.outline.as_deref() {
            // `none` disables an inherited outline; TOML has no null value.
            None | Some("none") => None,
            Some(c) => {
                Some(gdk::RGBA::parse(c).with_context(|| format!("bad outline color {c:?}"))?)
            }
        };
        // Normalise zero radius to no glow before allocating masks.
        let glow = match &style.glow {
            Some(g) if g.radius > 0.0 => {
                let rgba = gdk::RGBA::parse(&g.color)
                    .with_context(|| format!("bad glow color {:?}", g.color))?;
                Some((rgba, g.clone()))
            }
            _ => None,
        };
        // Zero strength collapses to no scanlines here rather than at every use
        // site: it is how a preset switches off a set inherited from the base.
        let scanlines = style
            .scanlines
            .as_ref()
            .filter(|s| s.strength > 0.0)
            .cloned();
        let font = pango::FontDescription::from_string(&style.font);
        // Reject an empty font family; Pango would otherwise fall back
        // silently.
        anyhow::ensure!(
            font.family().is_some(),
            "font {:?} has no family; expected something like \
             \"Monospace 72\"",
            style.font
        );
        // Use the default font map because this runs before `gtk::init`.
        let caret_width = {
            let ctx = pangocairo::FontMap::default().create_context();
            let probe = pango::Layout::new(&ctx);
            probe.set_font_description(Some(&font));
            probe.set_text("M");
            let (cell, _, line) = caret_pos(&probe, "M", 1);
            if cell > 0.0 {
                cell
            } else {
                // Fall back to half the line height if `M` has no advance.
                line * 0.5
            }
        };
        let timeline = Timeline::new(&text, &style.reveal, style.timeout_ms, &style.vanish, seed);
        let outline_width = outline_width(&font, style.outline_width);
        Ok(Hud {
            fill,
            outline,
            glow,
            scanlines,
            outline_width,
            caret_width,
            font,
            timeline,
            style,
            text,
        })
    }

    /// Padding for the stroke, glow and caret beyond the final character.
    fn pad(&self) -> Pad {
        let stroke = self.outline_width.max(0.0).ceil();
        // Reserve glow reach on both axes to avoid clipping at the surface
        // edge.
        let glow = self
            .glow
            .as_ref()
            .map_or(0.0, |(_, g)| blur_passes(g.radius).1.ceil());
        // Reserve the measured caret width horizontally; its height fits in the
        // line box.
        let caret = if matches!(self.style.reveal, Reveal::Typewriter { cursor: true, .. }) {
            self.caret_width.ceil()
        } else {
            0.0
        };
        Pad {
            x: stroke + glow + caret + 8.0,
            y: stroke + glow + 8.0,
        }
    }

    /// Whether typing scrolls earlier lines up from the bottom of the block.
    fn scrolls(&self) -> bool {
        matches!(self.style.reveal, Reveal::Typewriter { scroll: true, .. })
    }

    /// Build a layout bounded by `max_width` logical pixels.
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

/// Stroke width in logical pixels. Default to font size / 14 to scale with
/// the glyphs; the fill covers the inner half of the centred stroke.
fn outline_width(font: &pango::FontDescription, configured: Option<f64>) -> f64 {
    if let Some(w) = configured {
        return w.max(0.0);
    }
    match font_points(font) {
        // Use the fallback width when the font description has no size.
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

/// Wrap to `max_width`, then shrink to the measured text width.
/// Pango aligns lines within this width, which must match the surface.
fn fit_width(layout: &pango::Layout, max_width: i32) {
    if max_width <= 0 {
        return;
    }
    layout.set_width(max_width * pango::SCALE);
    // Allow character breaks for tokens wider than the monitor.
    layout.set_wrap(pango::WrapMode::WordChar);
    let (text_width, _) = layout.pixel_size();
    if text_width > 0 {
        layout.set_width(text_width * pango::SCALE);
    }
}

/// Device-resolution A8 glow mask of the revealed glyphs. Clip ink before
/// blurring to avoid a hard edge at the caret. Rebuild when the visible count
/// changes, blurring only the region containing ink and its halo.
fn glow_mask(
    layout: &pango::Layout,
    text: &str,
    visible: usize,
    total: usize,
    radius: f64,
    pad: Pad,
    scale: f64,
) -> Option<gtk::cairo::ImageSurface> {
    let (tw, th) = layout.pixel_size();
    let w = (((tw as f64) + pad.x * 2.0) * scale).ceil() as i32;
    let h = (((th as f64) + pad.y * 2.0) * scale).ceil() as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    let (pass, reach) = blur_passes(radius);
    let mut surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).ok()?;
    // Restrict the blur to the nonzero region plus halo reach.
    let (mut live_w, mut live_h) = (w, h);
    {
        let mcr = gtk::cairo::Context::new(&surface).ok()?;
        mcr.scale(scale, scale);
        mcr.translate(pad.x, pad.y);
        if visible < total {
            let (cx, cy, ch) = caret_pos(layout, text, visible);
            // Include completed lines and the typed part of the current line.
            // The blur supplies the halo beyond these bounds.
            mcr.rectangle(-pad.x, -pad.y, f64::from(tw) + pad.x * 2.0, cy + pad.y);
            mcr.rectangle(-pad.x, cy, pad.x + cx, ch);
            mcr.clip();
            // Completed lines use the full width, even when the current line is
            // shorter.
            live_w = if cy > 0.0 {
                w
            } else {
                (((pad.x + cx + reach) * scale).ceil() as i32).clamp(1, w)
            };
            live_h = ((((pad.y + cy + ch + reach) * scale).ceil()) as i32).clamp(1, h);
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

/// Derive the per-pass radius and actual reach from the requested reach.
fn blur_passes(reach: f64) -> (usize, f64) {
    // Three passes have support through pixel 3p, so reserve 3p + 1 pixels.
    let pass = ((reach - 1.0) / 3.0).floor().max(0.0);
    if pass < 1.0 {
        return (0, 0.0);
    }
    (pass as usize, pass * 3.0 + 1.0)
}

/// Approximate a Gaussian with three box passes. Use f32 until final byte
/// quantisation to preserve faint strokes. Moving sums keep cost O(pixels)
/// regardless of radius.
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

/// Box pass along `step`, over `runs` lines of `len` pixels. Swap step/lead
/// for the other axis. Divide by the full window, treating out-of-bounds
/// pixels as zero so the halo fades at the edge.
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
        // Slice once per run to let the compiler eliminate inner-loop bounds
        // checks.
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

/// An A8 raster of dimmed horizontal bands, sized to the whole padded surface
/// and rendered at DEVICE resolution so the gaps land on the physical pixel
/// grid. Built in logical pixels they would fall on half-pixels at `scale = 2`
/// and beat against it, which reads as moire rather than as a raster.
///
/// Depends on nothing that changes, so the caller builds it once per output.
/// Held as a whole surface rather than a repeating tile because `period` is a
/// float: a tile would have to round it to a whole number of pixels, and the
/// rounding error would walk down the message as a drifting band.
fn scanline_mask(w: i32, h: i32, sl: &Scanlines) -> Option<gtk::cairo::ImageSurface> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).ok()?;
    {
        let mcr = gtk::cairo::Context::new(&surface).ok()?;
        // Colour is ignored in A8; only the alpha channel survives. Everything
        // outside a gap passes through untouched.
        mcr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        let _ = mcr.paint();
        // Source, not Over: a gap dims what is under it to a set level rather
        // than compositing towards it, so `strength` means what it says.
        mcr.set_operator(gtk::cairo::Operator::Source);
        mcr.set_source_rgba(1.0, 1.0, 1.0, 1.0 - sl.strength);
        let gap = sl.period * sl.duty;
        let mut y = 0.0;
        while y < f64::from(h) {
            mcr.rectangle(0.0, y, f64::from(w), gap);
            y += sl.period;
        }
        let _ = mcr.fill();
    }
    Some(surface)
}

/// Vertical offset that places the current line at the bottom of the fixed
/// text block. Use the caret line box to handle mixed font heights.
/// The visible count drives the offset during both reveal and untype.
fn scroll_offset(layout: &pango::Layout, text: &str, visible: usize) -> f64 {
    let (_, th) = layout.pixel_size();
    let (_, cy, ch) = caret_pos(layout, text, visible);
    // Clamp rounding errors so the final line has zero offset.
    (f64::from(th) - (cy + ch)).max(0.0)
}

/// Visible count shared by the glyph drawing and glow mask.
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

/// Show one overlay. `on_first_frame` runs once across all windows to start
/// audio at the animation epoch. `on_closed` runs when this window finishes.
pub fn present(
    monitor: &gdk::Monitor,
    hud: Rc<Hud>,
    on_first_frame: Rc<dyn Fn()>,
    on_closed: impl Fn() + 'static,
) -> Result<()> {
    // Use a plain window to avoid GtkApplication session-bus and portal probes.
    let window = gtk::Window::new();
    window.add_css_class("wayhud");

    window.init_layer_shell();
    window.set_monitor(Some(monitor));
    // Keep this namespace stable for compositor rules.
    window.set_namespace(Some("wayhud"));
    window.set_layer(gtk_layer_shell::Layer::Overlay);
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);
    apply_anchors(&window, &hud.style);

    let area = gtk::DrawingArea::new();
    window.set_child(Some(&area));

    // Size from the complete text so typing cannot move or resize the surface.
    let pad = hud.pad();
    let max_width = text_budget(monitor, &hud.style, pad);
    // Cache the shaped layout across frames.
    let layout = Rc::new(hud.layout_for(&area, max_width));
    // Cache glow by visible character count at this monitor's scale.
    // `usize::MAX` marks an unbuilt mask; zero is a valid visible count.
    let device_scale = f64::from(monitor.scale_factor());
    let glow_cache: RefCell<(usize, Option<Rc<gtk::cairo::ImageSurface>>)> =
        RefCell::new((usize::MAX, None));
    let (tw, th) = layout.pixel_size();
    area.set_content_width(tw + (pad.x * 2.0) as i32);
    area.set_content_height(th + (pad.y * 2.0) as i32);
    // Built once per output, like the layout: the raster depends on nothing
    // that changes, and a vanish repaints every frame.
    let scanlines = hud.scanlines.as_ref().and_then(|sl| {
        let w = ((f64::from(tw) + pad.x * 2.0) * device_scale).ceil() as i32;
        let h = ((f64::from(th) + pad.y * 2.0) * device_scale).ceil() as i32;
        scanline_mask(w, h, sl).map(Rc::new)
    });

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
                        device_scale,
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
                scanlines.as_ref().map(|m| (m.as_ref(), device_scale)),
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

    // Set an empty input region after the surface exists. Fail if it cannot be
    // set, since the overlay would otherwise intercept pointer events.
    let surface = window
        .surface()
        .context("window has no surface after present; cannot make it click-through")?;
    surface.set_input_region(Some(&gtk::cairo::Region::create()));
    Ok(())
}

fn apply_anchors(window: &gtk::Window, style: &Style) {
    // An axis with neither edge anchored is centred by the compositor.
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
fn text_budget(monitor: &gdk::Monitor, style: &Style, pad: Pad) -> i32 {
    let geom = monitor.geometry();
    // Only anchored axes subtract margins from the available width.
    let margins = if style.halign == HAlign::Center {
        0
    } else {
        style.margin
    };
    (geom.width() - margins - (pad.x * 2.0) as i32).max(1)
}

fn draw(
    cr: &gtk::cairo::Context,
    hud: &Hud,
    layout: &pango::Layout,
    glow: Option<(&gtk::cairo::ImageSurface, gdk::RGBA, f64)>,
    scanlines: Option<(&gtk::cairo::ImageSurface, f64)>,
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

    // Untype changes the visible count before drawing.
    if vanish_p > 0.0 && hud.style.vanish.is_untype() {
        visible = hud.timeline.untype_visible(vanish_p);
    }

    // Translate ink, caret and glow together for terminal mode; zero otherwise.
    let scroll = if hud.scrolls() {
        scroll_offset(layout, &hud.text, visible)
    } else {
        0.0
    };

    // The raster belongs to the screen rather than to the message, so it goes
    // on outside every transform below: a collapse then squashes the picture
    // under stationary gaps, the way a tube does.
    if scanlines.is_some() {
        cr.push_group();
    }

    let _ = cr.save();
    cr.translate(pad.x, pad.y + scroll);

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
                // Scale around the centre to keep the effect within the
                // surface.
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

    // Group the stroke and fill before masking so they disappear together.
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
    // Use the blink state computed by the tick callback.
    let caret = caret_on.then(|| caret_rect(layout, &hud.text, visible, hud.caret_width));
    // Paint all halos before ink, including the caret halo.
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
                    let _ = cr.mask_surface(&surface, -pad.x, -pad.y);
                }
            }
            // Unreachable: `masked` requires Wash or Dissolve. A panic here
            // would abort the process from the draw callback.
            _ => {}
        }
    }

    let _ = cr.restore();

    if let Some((mask, scale)) = scanlines {
        let _ = cr.pop_group_to_source();
        // The mask is at device resolution while the context is in logical
        // pixels. Scaling the context would carry the popped group with it, so
        // the conversion goes into the pattern matrix instead.
        let pattern = gtk::cairo::SurfacePattern::create(mask);
        pattern.set_matrix(gtk::cairo::Matrix::new(scale, 0.0, 0.0, scale, 0.0, 0.0));
        let _ = cr.mask(pattern);
    }
}

/// Paint the glow mask without further clipping. Inverse-scale from device
/// to logical pixels and offset by `-pad` to align with the text origin.
fn paint_glow(
    cr: &gtk::cairo::Context,
    mask: &gtk::cairo::ImageSurface,
    layout: &pango::Layout,
    colour: gdk::RGBA,
    // `alpha` combines frame opacity and glow opacity.
    alpha: f64,
    whiten: f64,
    pad: Pad,
) {
    let scale = f64::from(mask.width()) / ((f64::from(layout.pixel_size().0)) + pad.x * 2.0);
    if !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let _ = cr.save();
    set_color(cr, colour, alpha, whiten);
    cr.scale(1.0 / scale, 1.0 / scale);
    // Positions are in mask pixels from here on, hence pad through the scale.
    let _ = cr.mask_surface(mask, -pad.x * scale, -pad.y * scale);
    let _ = cr.restore();
}

/// Blur a separate rectangle for the caret halo using the same blur as the
/// text. This keeps caret blinking independent of the cached text mask.
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
    pad: Pad,
) {
    if visible < total {
        let (cx, cy, ch) = caret_pos(layout, &hud.text, visible);
        let (w, _) = layout.pixel_size();
        // Clip completed lines and the typed part of the current line. Allow
        // only stroke slack past the caret to avoid exposing the next glyph.
        // Rectangles start at `-pad`, so their widths must include that
        // padding.
        let slack = hud.outline_width.max(1.0);
        cr.rectangle(-pad.x, -pad.y, w as f64 + pad.x * 2.0, cy + pad.y);
        cr.rectangle(-pad.x, cy, pad.x + cx + slack, ch);
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

/// Mask surviving blocks using fixed pseudo-random lifetimes.
fn dissolve_mask(tw: f64, th: f64, pad: Pad, p: f64) -> Option<gtk::cairo::ImageSurface> {
    let w = (tw + pad.x * 2.0).ceil() as i32;
    let h = (th + pad.y * 2.0).ceil() as i32;
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

/// Caret position and line height from Pango; width is resolved separately.
fn caret_pos(layout: &pango::Layout, text: &str, visible_chars: usize) -> (f64, f64, f64) {
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
    (x, r.y() as f64 / sc, r.height() as f64 / sc)
}

/// Caret position with the fixed font-derived width.
fn caret_rect(layout: &pango::Layout, text: &str, visible_chars: usize, width: f64) -> Caret {
    let (x, y, h) = caret_pos(layout, text, visible_chars);
    Caret { x, y, w: width, h }
}

fn show_caret(reveal: &Reveal, vanish: &Vanish, phase: Phase, t_ms: f64) -> bool {
    // An explicit `cursor = false` wins everywhere.
    if matches!(reveal, Reveal::Typewriter { cursor: false, .. }) {
        return false;
    }
    let typing = matches!(reveal, Reveal::Typewriter { .. });
    match phase {
        Phase::Reveal { .. } => typing,
        // Keep blinking during the hold.
        Phase::Hold => typing && ((t_ms / 530.0) as u64).is_multiple_of(2),
        // Untype uses a caret even after an instant reveal.
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
        scroll: false,
    };
    const NO_CURSOR: Reveal = Reveal::Typewriter {
        cps: 20.0,
        cursor: false,
        jitter: 0.0,
        scroll: false,
    };
    const UNTYPE: Vanish = Vanish::Untype { ms: 300 };
    const FADE: Vanish = Vanish::Fade { ms: 300 };

    #[test]
    fn outline_scales_with_the_font_unless_pinned() {
        let w = |spec: &str| outline_width(&pango::FontDescription::from_string(spec), None);
        // Default scaling preserves 5.0 at 72pt; also exercise a multi-word
        // family.
        assert!((w("Some Wide Family 72") - 72.0 / 14.0).abs() < 1e-9);
        assert!(w("Some Wide Family 24") < w("Some Wide Family 48"));

        let pinned = outline_width(
            &pango::FontDescription::from_string("Some Wide Family 24"),
            Some(9.0),
        );
        assert_eq!(pinned, 9.0);
    }

    #[test]
    fn outline_width_survives_a_font_with_no_size() {
        // A size-free font description must still produce a visible stroke.
        let w = outline_width(&pango::FontDescription::from_string("Sans"), None);
        assert!(w > 0.0, "got {w}");
    }

    #[test]
    fn negative_configured_width_is_clamped_not_passed_to_cairo() {
        let w = outline_width(&pango::FontDescription::from_string("Sans 20"), Some(-3.0));
        assert_eq!(w, 0.0);
    }

    /// Test geometry with fontconfig generics and installed named families.
    /// Skip unavailable named families; line metrics vary between fonts.
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

    /// Create a Pango layout without a GTK widget or display.
    fn bare_layout(text: &str, font: &str) -> pango::Layout {
        let ctx = pangocairo::FontMap::default().create_context();
        let layout = pango::Layout::new(&ctx);
        layout.set_font_description(Some(&pango::FontDescription::from_string(font)));
        layout.set_text(text);
        layout
    }

    #[test]
    fn alignment_does_not_push_the_text_out_of_the_window() {
        // Alignment must use the measured block width, not the wrapping budget.
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
        // Shrinking width must preserve line breaks.
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
        let hud = Hud::new(Style::default(), "wayhud".into(), 1).unwrap();
        let (w, h) = bare_layout(&hud.text, &hud.style.font).pixel_size();
        assert!(w > 0 && h > 0, "the default font gave a {w}x{h} layout");
    }

    #[test]
    fn the_caret_follows_a_proportional_font() {
        // Measure caret positions on shaped proportional text with varying
        // advances.
        let text = "iWiW";
        let layout = bare_layout(text, "Sans 72");
        let mut last_x = f64::NEG_INFINITY;
        for i in 0..=text.chars().count() {
            let c = caret_rect(&layout, text, i, 40.0);
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
        // The caret beyond the final character must fit in the horizontal
        // padding.
        let style = Style {
            font: "Sans 72".into(),
            reveal: TW,
            ..Style::default()
        };
        let text = "iWiW";
        let hud = Hud::new(style, text.into(), 1).unwrap();
        let layout = bare_layout(text, &hud.style.font);
        let (tw, _) = layout.pixel_size();
        let end = caret_rect(&layout, text, text.chars().count(), hud.caret_width);
        assert!(
            end.x + end.w <= tw as f64 + hud.pad().x,
            "caret ends at {} outside a {tw}px layout with {} of horizontal padding",
            end.x + end.w,
            hud.pad().x
        );
    }

    #[test]
    fn a_proportional_font_wraps_and_fits_like_a_monospaced_one() {
        // Proportional glyph advances must stay within the wrapping budget.
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
        // Instant reveal needs no caret before untyping.
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
        // Padding must fit the actual caret width for each available font.
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
            let past_end = hud.caret_width;
            assert!(
                hud.pad().x >= past_end,
                "{spec}: horizontal pad {:.1} does not cover a {past_end:.1}px caret",
                hud.pad().x
            );
            // Caret width must not increase vertical padding.
            assert!(
                hud.pad().y < past_end,
                "{spec}: vertical pad {:.1} is reserving caret room it cannot use",
                hud.pad().y
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
            hud.pad().x < 30.0,
            "instant reveal should not reserve caret room"
        );
    }

    #[test]
    fn outline_none_switches_the_stroke_off() {
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
        // Blur must preserve total coverage within quantisation tolerance.
        let total: u32 = buf.iter().map(|&v| u32::from(v)).sum();
        assert!(total > 200, "the blur ate the signal: 255 in, {total} out");
        // Zero padding must let the halo fade at the surface edge.
        assert_eq!(at(0, 0), 0, "energy reached a far corner");
    }

    #[test]
    fn a_lit_edge_pixel_does_not_wrap_to_the_other_side() {
        // A horizontal pass must not spill into the next row.
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
        // Surface padding must contain the halo.
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
            lit.pad().x >= bare.pad().x + reach && lit.pad().y >= bare.pad().y + reach,
            "pad {:?} does not cover a {reach}px halo over {:?}",
            lit.pad(),
            bare.pad()
        );
    }

    /// Total coverage of each row of a rendered message, so a test can compare
    /// what a scanline gap let through against what the row beside it did.
    fn row_sums(hud: &Hud, layout: &pango::Layout, scale: f64) -> Vec<u32> {
        let pad = hud.pad();
        let (tw, th) = layout.pixel_size();
        let w = (f64::from(tw) + pad.x * 2.0).ceil() as i32;
        let h = (f64::from(th) + pad.y * 2.0).ceil() as i32;
        let mask = hud.scanlines.as_ref().and_then(|sl| {
            scanline_mask(
                (f64::from(w) * scale) as i32,
                (f64::from(h) * scale) as i32,
                sl,
            )
        });
        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("surface");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            draw(
                &cr,
                hud,
                layout,
                None,
                mask.as_ref().map(|m| (m, scale)),
                Phase::Hold,
                false,
            );
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        (0..h as usize)
            .map(|y| {
                (0..w as usize)
                    .map(|x| u32::from(data[y * stride + x]))
                    .sum()
            })
            .collect()
    }

    fn scanline_style(strength: f64) -> Style {
        Style {
            font: "Sans 72".into(),
            scanlines: Some(crate::config::Scanlines {
                period: 4.0,
                strength,
                duty: 0.5,
            }),
            ..Style::default()
        }
    }

    #[test]
    fn a_gap_dims_to_the_strength_asked_for_and_the_rest_stays_clear() {
        let sl = crate::config::Scanlines {
            period: 4.0,
            strength: 0.75,
            duty: 0.5,
        };
        let mut mask = scanline_mask(8, 8, &sl).expect("mask");
        mask.flush();
        let stride = mask.stride() as usize;
        let data = mask.data().expect("pixels");
        // duty 0.5 of period 4: rows 0-1 are gap, rows 2-3 are clear.
        for (y, want) in [(0, 64u8), (1, 64), (2, 255), (3, 255), (4, 64), (6, 255)] {
            let got = data[y * stride];
            assert!(got.abs_diff(want) <= 1, "row {y}: wanted {want}, got {got}");
        }
    }

    #[test]
    fn scanlines_dim_the_ink_they_cross() {
        // Against the same message drawn without them, so glyph antialiasing
        // cannot be mistaken for a gap: half of every period is dimmed to
        // 1 - strength, which must show up as that share of the total ink.
        let strength = 0.6;
        let bare = Hud::new(
            Style {
                font: "Sans 72".into(),
                ..Style::default()
            },
            "MMMM".into(),
            1,
        )
        .unwrap();
        let striped = Hud::new(scanline_style(strength), "MMMM".into(), 1).unwrap();
        let layout = bare_layout(&bare.text, &bare.style.font);
        let clear: u32 = row_sums(&bare, &layout, 1.0).iter().sum();
        let dimmed: u32 = row_sums(&striped, &layout, 1.0).iter().sum();
        assert!(clear > 0, "nothing was drawn at all");
        // duty 0.5 leaves half the ink untouched and dims the other half.
        let want = 1.0 - strength * 0.5;
        let got = f64::from(dimmed) / f64::from(clear);
        assert!(
            (got - want).abs() < 0.05,
            "raster passed {got:.3} of the ink, expected about {want:.3}"
        );
    }

    #[test]
    fn scanlines_do_not_change_the_surface_size() {
        // The raster is painted inside the padding, so unlike the glow it must
        // not widen it; a message would otherwise move away from its edge.
        let bare = Hud::new(
            Style {
                font: "Sans 72".into(),
                ..Style::default()
            },
            "M".into(),
            1,
        )
        .unwrap();
        let striped = Hud::new(scanline_style(0.6), "M".into(), 1).unwrap();
        assert_eq!(bare.pad(), striped.pad());
    }

    #[test]
    fn a_zero_strength_raster_is_no_raster_at_all() {
        // How a preset takes back scanlines inherited from [style.default].
        let hud = Hud::new(scanline_style(0.0), "M".into(), 1).unwrap();
        assert!(hud.scanlines.is_none());
    }

    #[test]
    fn a_zero_radius_glow_is_no_glow_at_all() {
        // Zero radius disables inherited glow.
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
        // Verify the glow paints coverage into the target.
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
            (f64::from(tw) + pad.x * 2.0) as i32,
            (f64::from(th) + pad.y * 2.0) as i32,
        );
        let mask = glow_mask(&layout, &hud.text, 1, 1, radius, pad, 1.0).expect("mask");
        let (colour, _) = hud.glow.as_ref().expect("glow resolved");

        let glyphs = lit_pixels(
            |cr| {
                cr.translate(pad.x, pad.y);
                cr.move_to(0.0, 0.0);
                pangocairo::functions::layout_path(cr, &layout);
                let _ = cr.fill();
            },
            w,
            h,
        );
        let halo = lit_pixels(
            |cr| {
                cr.translate(pad.x, pad.y);
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
            (f64::from(tw) + pad.x * 2.0) as i32,
            (f64::from(th) + pad.y * 2.0) as i32,
        );
        let visible = 3usize;
        let caret = caret_rect(&layout, text, visible, hud.caret_width);
        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("target");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            cr.translate(pad.x, pad.y);
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
        (caret.x, rightmost as f64 - pad.x, pad.x)
    }

    #[test]
    fn the_revealed_text_reaches_the_caret_whatever_the_padding() {
        // Revealed ink must reach within one glyph advance of the caret.
        // The tolerance allows for font side bearings.
        let advance = caret_pos(&bare_layout("mmmmm", "Sans 72"), "mmmmm", 4).0
            - caret_pos(&bare_layout("mmmmm", "Sans 72"), "mmmmm", 3).0;
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
        // The ink-to-caret gap must be independent of padding.
        let widest = gaps.iter().fold(f64::MIN, |a, &b| a.max(b));
        let tightest = gaps.iter().fold(f64::MAX, |a, &b| a.min(b));
        assert!(
            widest - tightest < 2.0,
            "the gap still follows the glow radius: {gaps:?}"
        );
    }

    #[test]
    fn the_halo_survives_below_the_line_being_typed() {
        // The glow must continue below the current line box.
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
            (f64::from(tw) + pad.x * 2.0) as i32,
            (f64::from(th) + pad.y * 2.0) as i32,
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
        let caret = caret_rect(&layout, text, visible, hud.caret_width);

        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("target");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            cr.translate(pad.x, pad.y);
            paint_glow(&cr, &mask, &layout, *colour, 1.0, 0.0, pad);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        // Measure a vertical run across the line box boundary for abrupt
        // falloff.
        let x = (pad.x + caret.x / 2.0) as usize;
        let from = (pad.y + caret.y + caret.h / 2.0) as usize;
        let to = ((pad.y + caret.y + caret.h + 2.0 * radius) as usize).min(h as usize - 1);
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
        // Padding must cover the combined reach of all three blur passes.
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
    fn the_caret_keeps_one_width_across_the_message() {
        // Caret width must stay fixed across proportional glyphs.
        for family in probe_families() {
            let spec = format!("{family} 72");
            let text = "iWmil";
            let hud = Hud::new(
                Style {
                    font: spec.clone(),
                    reveal: TW,
                    ..Style::default()
                },
                text.into(),
                1,
            )
            .expect("hud should build");
            let layout = bare_layout(text, &spec);
            let widths: Vec<f64> = (0..=text.chars().count())
                .map(|i| caret_rect(&layout, text, i, hud.caret_width).w)
                .collect();
            assert!(
                widths.iter().all(|w| (w - widths[0]).abs() < f64::EPSILON),
                "{spec}: the caret changes width along the message: {widths:?}"
            );
            assert!(widths[0] > 0.0, "{spec}: the caret has no width at all");
            // The block caret must be at least as wide as the narrowest glyph.
            let narrow = caret_pos(&bare_layout("i", &spec), "i", 1).0;
            assert!(
                widths[0] >= narrow,
                "{spec}: caret {} is narrower than an `i` at {narrow}",
                widths[0]
            );
        }
    }

    #[test]
    fn the_caret_glows_past_its_own_rectangle() {
        // The caret must receive its own glow.
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
        // Zero radius disables caret glow too.
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
        // Partial-reveal glow must fade smoothly past the caret. Clipping after
        // blur would produce an abrupt step.
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
            (f64::from(tw) + pad.x * 2.0) as i32,
            (f64::from(th) + pad.y * 2.0) as i32,
        );
        let visible = 3usize;
        let mask = glow_mask(&layout, text, visible, 5, radius, pad, 1.0).expect("mask");
        let (colour, _) = hud.glow.as_ref().expect("glow");
        let caret = caret_rect(&layout, text, visible, hud.caret_width);

        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("target");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            cr.translate(pad.x, pad.y);
            paint_glow(&cr, &mask, &layout, *colour, 1.0, 0.0, pad);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");

        // Sample horizontally from revealed ink through the glow edge.
        let y = (pad.y + caret.y + caret.h / 2.0) as usize;
        let from = (pad.x + caret.x - 3.0 * radius).max(0.0) as usize;
        let to = ((pad.x + caret.x + 3.0 * radius) as usize).min(w as usize - 1);
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
        // Measured smooth falloff has peak/step near 11; clipped falloff near
        // 2.5. The threshold must distinguish them.
        assert!(
            step * 6 < peak,
            "the halo steps by {step} of a {peak} peak in one pixel, which is \
             an edge rather than a falloff"
        );
    }

    #[test]
    fn the_glow_mask_is_built_at_device_resolution() {
        // Build masks at device resolution to avoid resampling blur on HiDPI.
        let layout = bare_layout("glow", "Sans 40");
        let (tw, _) = layout.pixel_size();
        let pad = Pad { x: 24.0, y: 24.0 };
        for scale in [1.0, 2.0] {
            let mask =
                glow_mask(&layout, "glow", 4, 4, 8.0, pad, scale).expect("mask should build");
            let want = (((tw as f64) + pad.x * 2.0) * scale).ceil() as i32;
            assert_eq!(mask.width(), want, "scale {scale}");
        }
    }

    #[test]
    fn dissolve_blocks_are_stable_and_in_range() {
        // Block lifetimes must remain stable across frames.
        for (x, y) in [(0, 0), (3, 7), (41, 2)] {
            let a = block_life(x, y);
            assert_eq!(a, block_life(x, y));
            assert!((0.0..1.0).contains(&a), "{a} out of range");
        }
        // Neighbouring blocks must have different lifetimes.
        assert_ne!(block_life(5, 5), block_life(6, 5));
    }

    /// First and last rows containing ink after `draw`.
    fn ink_rows(hud: &Hud, layout: &pango::Layout, phase: Phase) -> (usize, usize) {
        let pad = hud.pad();
        let (tw, th) = layout.pixel_size();
        let w = (f64::from(tw) + pad.x * 2.0).ceil() as i32;
        let h = (f64::from(th) + pad.y * 2.0).ceil() as i32;
        let mut surface =
            gtk::cairo::ImageSurface::create(gtk::cairo::Format::A8, w, h).expect("surface");
        {
            let cr = gtk::cairo::Context::new(&surface).expect("context");
            draw(&cr, hud, layout, None, None, phase, false);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().expect("pixels");
        let rows: Vec<usize> = (0..h as usize)
            .filter(|y| (0..w as usize).any(|x| data[y * stride + x] > 0))
            .collect();
        (
            rows.first().copied().expect("nothing was drawn at all"),
            rows.last().copied().expect("nothing was drawn at all"),
        )
    }

    #[test]
    fn both_fills_ship_and_start_from_opposite_ends_of_the_box() {
        // After one of three lines, default mode paints at the top and terminal
        // mode at the bottom. Check rendered ink rather than offsets alone.
        let text = "aaa\nbbb\nccc";
        let font = "Monospace 36";
        let hud = |scroll| {
            Hud::new(
                Style {
                    font: font.into(),
                    glow: None,
                    reveal: Reveal::Typewriter {
                        cps: 20.0,
                        cursor: true,
                        jitter: 0.0,
                        scroll,
                    },
                    ..Style::default()
                },
                text.into(),
                1,
            )
            .expect("hud")
        };
        let layout = bare_layout(text, font);
        let (_, th) = layout.pixel_size();
        let phase = Phase::Reveal { chars: 3 };

        let (classic_top, classic_bottom) = ink_rows(&hud(false), &layout, phase);
        let (terminal_top, terminal_bottom) = ink_rows(&hud(true), &layout, phase);

        assert!(
            classic_bottom < terminal_top,
            "the two fills overlap: default ends at {classic_bottom}, \
             terminal starts at {terminal_top}"
        );
        // Check each mode against the block bounds.
        let third = f64::from(th) / 3.0;
        assert!(
            f64::from(classic_top as i32) < third,
            "the default fill must start at the top of the box, not row {classic_top}"
        );
        assert!(
            f64::from(terminal_bottom as i32) > third * 2.0,
            "terminal mode must write at the bottom of the box, not row {terminal_bottom}"
        );
    }

    #[test]
    fn a_finished_message_draws_the_same_either_way() {
        // Both modes must place the fully revealed text identically.
        let text = "aaa\nbbb\nccc";
        let font = "Monospace 36";
        let hud = |scroll| {
            Hud::new(
                Style {
                    font: font.into(),
                    glow: None,
                    reveal: Reveal::Typewriter {
                        cps: 20.0,
                        cursor: true,
                        jitter: 0.0,
                        scroll,
                    },
                    ..Style::default()
                },
                text.into(),
                1,
            )
            .expect("hud")
        };
        let layout = bare_layout(text, font);
        assert_eq!(
            ink_rows(&hud(false), &layout, Phase::Hold),
            ink_rows(&hud(true), &layout, Phase::Hold),
            "the finished message moved between the two fills"
        );
    }

    #[test]
    fn only_a_scrolling_preset_moves_the_block() {
        let hud = |reveal| {
            Hud::new(
                Style {
                    reveal,
                    ..Style::default()
                },
                "a\nb".into(),
                1,
            )
            .expect("hud")
            .scrolls()
        };
        assert!(!hud(TW), "a plain typewriter must fill the box downwards");
        assert!(!hud(Reveal::Instant));
        assert!(hud(Reveal::Typewriter {
            cps: 20.0,
            cursor: true,
            jitter: 0.0,
            scroll: true,
        }));
    }

    #[test]
    fn the_terminal_reveal_pins_the_write_head_to_the_bottom_line() {
        // The current line must end at the block bottom across font families.
        let text = "one\ntwo\nthree\nfour";
        let total = text.chars().count();
        for family in probe_families() {
            let layout = bare_layout(text, &format!("{family} 36"));
            let (_, th) = layout.pixel_size();
            for visible in 0..=total {
                let dy = scroll_offset(&layout, text, visible);
                let (_, cy, ch) = caret_pos(&layout, text, visible);
                assert!(
                    (dy + cy + ch - f64::from(th)).abs() < 1e-6,
                    "{family}: {visible} chars in, the written text ends at \
                     {} rather than the block bottom {th}",
                    dy + cy + ch
                );
            }
            assert_eq!(
                scroll_offset(&layout, text, total),
                0.0,
                "{family}: the finished message must sit where it always did"
            );
            assert!(
                scroll_offset(&layout, text, 0) > 0.0,
                "{family}: the first line must start pushed down"
            );
        }
    }

    #[test]
    fn the_terminal_offset_only_ever_falls() {
        // The offset decreases at line breaks and stays fixed within each line.
        let text = "aa\nbb\ncc";
        let layout = bare_layout(text, "Monospace 36");
        let steps: Vec<f64> = (0..=text.chars().count())
            .map(|v| scroll_offset(&layout, text, v))
            .collect();
        assert!(
            steps.windows(2).all(|w| w[1] <= w[0]),
            "offset went back up: {steps:?}"
        );
        // Three lines, so exactly two drops.
        let drops = steps.windows(2).filter(|w| w[1] < w[0]).count();
        assert_eq!(drops, 2, "one drop per newline consumed: {steps:?}");
    }

    #[test]
    fn a_single_line_message_never_scrolls() {
        // Single-line text needs no scroll offset.
        let text = "no newlines here";
        let layout = bare_layout(text, "Monospace 36");
        for visible in 0..=text.chars().count() {
            assert_eq!(scroll_offset(&layout, text, visible), 0.0, "at {visible}");
        }
    }
}
