# wayhud — conventions

A one-shot layer-shell overlay for sway: show text, wait, exit. Rust + GTK4 +
gtk4-layer-shell, drawn by hand with pango/cairo. Runs on the user's own
machines (NixOS, sway, three HiDPI outputs).

- build: `nix develop --command cargo build --release`
- test: `nix develop --command cargo test`
- lint: `nix develop --command cargo clippy --all-targets -- -D warnings`

Install the hook once per clone: `git config core.hooksPath hooks`.

## Layout

- `timeline.rs` — the reveal/hold/vanish state machine. Pure milliseconds, no
  GTK; this is where the animation contract is tested.
- `hud.rs` — one layer-shell window per output, plus the draw callback.
- `synth.rs` — blip synthesis, vendored from `blyamk`. Pure functions.
- `sound.rs` — mixes the blip track and plays it.

## Non-negotiables

- **The built-in default font is the fontconfig generic `Monospace`, not a
  named family.** A family that does not resolve is not an error: fontconfig
  substitutes the system default without a word of warning, and that default is
  proportional — the one thing a HUD must not be. A named family has to be
  spelled the way fontconfig spells it, which is rarely how the vendor writes
  it (`fc-match "Fira Code"` lands on DejaVu Sans; `FiraCode Nerd Font` is the
  name that resolves). Check with `fc-match` before putting one in a doc.
- **No `GtkApplication`.** It registers on the session bus and probes for the
  `Inhibit` portal, which nothing provides under sway (`gtk.portal` declares it
  but is `UseIn=gnome`; `wlr.portal` only does Screenshot/ScreenCast), so every
  run printed a GDK warning. A plain `GtkWindow` plus a `glib::MainLoop` does
  everything this tool needs, and showing two overlays for two invocations is
  the spec — the single-instance behaviour was something we had to switch off
  anyway.
- **The layer namespace stays `"wayhud"`.** It is visible on the wire and
  people key sway rules off it; renaming it breaks their configs.
- **The surface keeps an empty input region.** At `Layer::Overlay` the window
  otherwise swallows pointer events for everything underneath it.
- **Size the window from the full text, never the partially revealed text.**
  Sizing it per frame makes the window grow under the typewriter and drag the
  message across the screen.
- **Clamp the pango layout width to the monitor, then shrink it back to the
  measured text.** The surface is sized from the text, so an unclamped long
  line is silently clipped at both screen edges — but leaving the width at the
  wrapping budget breaks alignment: pango positions lines for a non-left
  `line_align` inside the layout width, so a centred line lands hundreds of
  pixels outside a surface sized to the text and nothing renders at all.
- **Everything on screen is in logical pixels.** Every output here runs at
  `scale = 2`; mixing in device pixels looks right on exactly one monitor.
- **The outline width scales with the font unless the config pins it.** cairo
  centres the stroke and the fill covers its inner half, so a constant width is
  a fixed number of pixels of halo against a stem that isn't: 21% of the stem
  at 72pt, 62% at 24pt.
- **Mask effects paint into a `push_group` first.** Masking the stroke and the
  fill as they are drawn erases them at different rates and leaves the outline
  hanging in the air after the fill is gone.
- **`block_life` must stay deterministic.** Re-rolling the dissolve pattern per
  frame turns a decay into static.
- **Redraw only when the frame actually changes.** A 60 fps cairo text-path
  repaint of a static string across three HiDPI outputs is pure heat.
