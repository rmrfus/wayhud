# wayhud — development

One-shot text overlay for sway. Rust, GTK4 and gtk4-layer-shell; text is drawn
with Pango and Cairo.

## Checks

```sh
nix develop --command cargo build --release --locked
nix develop --command cargo fmt --all --check
nix develop --command cargo clippy --all-targets --locked -- -D warnings
nix develop --command cargo test --locked
nix develop --command cargo deny check advisories sources
nix develop --command cargo machete
nix develop --command groff -man -Tutf8 -ww -z man/man1/wayhud.1
nix develop --command groff -man -Tutf8 -ww -z man/man5/wayhud.5
nix build
```

Install the pre-commit hook with `git config core.hooksPath hooks`. It checks
the staged tree. Keep `--locked` and `-D warnings` in build and lint commands.

## Layout

- `src/main.rs`: CLI, preset overrides, GTK main loop.
- `src/spec.rs`: comma-separated CLI specs and field validation.
- `src/config.rs`: TOML presets, inheritance, defaults and range checks.
- `src/outputs.rs`: GDK monitors and focused-output lookup through sway IPC.
- `src/timeline.rs`: reveal, hold and vanish timing, shared with audio.
- `src/hud.rs`: layer-shell windows, text layout and drawing.
- `src/synth.rs`: blip synthesis from blyamk.
- `src/sound.rs`: track mixing and PulseAudio playback.

## Constraints

- Use `Monospace` as the default; unresolved families silently fall back.
  Check named families with `fc-match`.
- Use `GtkWindow` and `glib::MainLoop`. `GtkApplication` probes the Inhibit
  portal and caused GDK warnings under sway. Keep invocations independent.
- One message per invocation is still the default and the whole of the
  one-shot path; `--listen` is a second contract, not a replacement. Both go
  through `Session` and the same `present`, so a listener cannot drift away
  from what one invocation renders.
- A listener's windows hide between messages rather than closing, and re-arm
  their tick when one arrives: an idle listener must not hold a frame callback
  open all day.
- Reshaping a message and resizing its surface happens on arrival, never in
  the draw callback: a resize queued from inside a draw is a resize during a
  draw.
- Clients send text and nothing else. The style is the listener's, which is
  what lets the block be sized before the first message and keeps a
  notification hook from deciding what the overlay looks like.
- The socket is a datagram: one send is one message, so a body carrying
  newlines needs no framing. A FIFO would block the sender until a reader
  exists, hanging a hook for every notification while nothing listens.
- Keep the namespace `wayhud`; renaming it breaks compositor rules.
- Set an empty input region; otherwise the overlay intercepts pointer events.
- Size the surface from the full text; sizing each prefix moves the window
  during typing.
- Wrap to monitor width, then shrink the layout to measured text width;
  otherwise centred lines can land outside the surface and appear blank.
- The block width is one number, carried by `Block`: pango aligns lines inside
  it and the surface is sized from it, so reading the two from different places
  draws a centred line outside the surface. `width` pins it; unset it is the
  measured text.
- Never mix device pixels into screen geometry: at scale=2, sizes double.
  Convert device-resolution masks back to logical coordinates when painting.
- Scale outline width with the font unless configured; a fixed 5px stroke
  grows from 21% of the stem at 72pt to 62% at 24pt.
- Group stroke and fill before masking; separate masks leave the outline
  visible after the fill disappears.
- `untype` is timed in `cps`, not `ms`: it erases a character at a time, so a
  fixed duration takes a long message apart faster per character than a short
  one, and a listener's block erases quicker the more it holds.
- A listener resumes from what is actually revealed, not from the length of
  the block: counting all of it snaps a half-typed line to finished the moment
  the next message lands.
- Keep `block_life` deterministic; rerolling per frame produces static.
- Redraw only on state changes; static text must not reshape at 60 fps.
  Cache glow by visible count so it follows reveal and untype steps.
- Blur in `f32`, quantising only at the end. Per-pass bytes gave
  255 -> 36 -> 5 -> 1 at one pixel; thin-stroke halos disappeared.
- Clip revealed ink before blur; clipping afterwards cut 75 from a 77 peak
  in one pixel, leaving a straight bright edge at the caret.
- Three radius-r box passes reach 3r, not r; derive r from requested reach.
  At r=16 with 16px padding, the edge retained 38 of a 221 peak: a hard rim.
- Clip widths starting at `-pad` must include `pad`; otherwise ink ends
  exactly pad short of the caret: 60px at 72pt without glow.
- Keep caret width fixed from font metrics; glyph advances made it jump
  from 27px before `i` to 94px before `m` in DejaVu Sans at 72pt.
- Reserve caret width only horizontally; vertical reserve left a 55px gap
  below bottom-anchored Sans 48 text at margin=0.
- Trim trailing newlines on both input paths; Pango adds an empty line,
  shifting centred text by half a line and parking the caret below it.
- Build the scanline mask in device pixels, once per output, and apply it
  outside every transform: it is a property of the screen, so it must cut the
  glow too and must not squash with a collapse. In logical pixels the gaps
  land on half-pixels at scale=2 and beat into moire.
- Bound outline width and glow radius by `MAX_EDGE_PX` (128); excessive
  padding reduces wrapping width until every word breaks onto a new line.
