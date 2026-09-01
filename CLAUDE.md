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

- **The font family is `LythMono Nerd Font`, not `Lyth Mono`.** fontconfig
  falls back to DejaVu Sans on the latter without a word of warning.
- **`Application` must keep `NON_UNIQUE`.** Otherwise GTK's single-instance
  machinery forwards a second invocation's arguments to the running process
  over D-Bus instead of opening a second overlay. Showing both is the spec.
- **The layer namespace stays `"wayhud"`.** It is visible on the wire and
  people key sway rules off it; renaming it breaks their configs.
- **The surface keeps an empty input region.** At `Layer::Overlay` the window
  otherwise swallows pointer events for everything underneath it.
- **Size the window from the full text, never the partially revealed text.**
  Sizing it per frame makes the window grow under the typewriter and drag the
  message across the screen.
- **Clamp the pango layout width to the monitor.** The surface is sized from
  the text, so an unclamped long line is silently clipped at both screen edges.
- **Everything on screen is in logical pixels.** Every output here runs at
  `scale = 2`; mixing in device pixels looks right on exactly one monitor.
- **Redraw only when the frame actually changes.** A 60 fps cairo text-path
  repaint of a static string across three HiDPI outputs is pure heat.
