# wayhud — conventions

A one-shot layer-shell overlay for sway: show text, wait, exit. Rust + GTK4 +
gtk4-layer-shell, drawn by hand with pango/cairo. Runs on the user's own
machines (NixOS, sway, three HiDPI outputs).

Run these exactly as written — they are what CI and the pre-commit hook run.
Dropping `--locked` lets cargo update the committed lockfile, at which point
the build is no longer the one CI checked; `-D warnings` is what turns the
`[lints.clippy]` table in Cargo.toml from advice into a failure.

- build: `nix develop --command cargo build --release --locked`
- fmt: `nix develop --command cargo fmt --all --check`
- lint: `nix develop --command cargo clippy --all-targets --locked -- -D warnings`
- test: `nix develop --command cargo test --locked`
- audit: `nix develop --command cargo deny check advisories sources`
- dead deps: `nix develop --command cargo machete`
- man pages: `nix develop --command groff -man -Tutf8 -ww -z man/man1/wayhud.1`

Install the hook once per clone: `git config core.hooksPath hooks`. It lints
the staged tree rather than the working one, so an unstaged fix on disk cannot
carry a broken hunk through.

## Layout

- `main.rs` — CLI, the flag-over-preset overrides, and the GTK main loop.
- `config.rs` — the TOML presets and the resolved `Style`. Presets are held as
  raw tables so a preset can be merged onto `[style.default]` before serde
  fills in defaults; deserialising first would make "unset" and "set to the
  default" indistinguishable.
- `outputs.rs` — which monitors a message lands on. `current` goes out to sway
  over IPC, because Wayland gives a client no way to ask which output is
  focused.
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
  repaint of a static string across three HiDPI outputs is pure heat. The glow
  mask obeys the same rule from the other side: it is blurred once per output,
  next to the pango layout, because it depends on nothing that changes and the
  vanish repaints every frame.
- **The blur passes run in `f32` and quantise to a byte once at the end.**
  Rounding between passes destroys a thin stroke: traced on one lit pixel it
  went 255 → 36 → 5 → 1 while the image total fell from 255 to 41, because
  255/9 is 28, 28/9 is 3 and 3/9 is 0. A glyph stem against a 12px radius is
  exactly that sparse, so the halo rendered as nothing at all.
- **The clip rectangles start at the surface edge, so their third argument is
  a WIDTH that has to carry `pad`.** Writing the right edge there instead left
  the revealed text ending `pad` short of the caret — 60px at 72pt with no glow
  at all, growing with the glow radius, which is what finally made it visible
  after months. Same trap on the height: the caret's line needs a radius of
  room below it or the halo is cut flat along the line box.
- **Anything that widens the padding is bounded by `MAX_EDGE_PX`.** The
  outline stroke and the glow radius both feed `pad`, and `pad` is subtracted
  from the monitor width to get the wrapping budget; unbounded, either starves
  the text until every line breaks after one word.
