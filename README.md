# wayhud

[![CI](https://github.com/rmrfus/wayhud/actions/workflows/ci.yml/badge.svg)](https://github.com/rmrfus/wayhud/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/rmrfus/wayhud?logo=github)](https://github.com/rmrfus/wayhud/releases/latest)
[![License](https://img.shields.io/github/license/rmrfus/wayhud)](LICENSE)

Text overlays for sway, with typewriter animation, a caret, synthesised sound,
scanlines and exit effects. The overlay is click-through and does not take
keyboard focus.

[![wayhud typing a status block over the desktop, then clearing it](assets/demo.gif)](assets/demo.mp4)

<sub>Demo font: [Conthrax](https://www.dafont.com/conthrax.font).</sub>

```sh
wayhud "SYSTEM ONLINE"
wayhud -o all -t 10 --position top "BUILD FAILED"
wayhud --vanish 'untype,cps=20' "THIS MESSAGE WILL SELF DESTRUCT"
journalctl -n 3 -u nginx | wayhud --reveal instant --color '#fb4934'
wayhud --scanlines '4,strength=0.5' --glow '#33ff33' --color '#33ff33' "ONLINE"
```

## Install

### Nix

```sh
nix run github:rmrfus/wayhud -- "HELLO"
nix build github:rmrfus/wayhud           # ./result/bin/wayhud
nix profile install github:rmrfus/wayhud
```

Add the flake input:

```nix
inputs.wayhud.url = "github:rmrfus/wayhud";
```

Then add the package in a NixOS module with access to `inputs`:

```nix
environment.systemPackages = [
  inputs.wayhud.packages.${pkgs.stdenv.hostPlatform.system}.default
];
```

### From source

Requires Rust 1.92+, `pkg-config`, and development packages for `gtk4`,
`gtk4-layer-shell`, `glib`, `cairo`, `pango`, `gdk-pixbuf`, `graphene` and
`libpulse`. `nix develop` provides these dependencies.

```sh
make
sudo make install                  # /usr/local
make install PREFIX="$HOME/.local" # user installation
make uninstall                     # use the same PREFIX
```

`make install` installs the binary and both man pages. It requires a prior
build and supports staging with `DESTDIR`:

```sh
make && make install DESTDIR="$pkgdir" PREFIX=/usr
```

## Usage

See `man 1 wayhud` for CLI details and `man 5 wayhud` for configuration.

| Flag            | Default   | Meaning                                                            |
| --------------- | --------- | ------------------------------------------------------------------ |
| `TEXT`          | —         | Message, at most 100000 chars. Omit or pass `-` to read stdin.     |
| `-o, --output`  | `current` | `current`, `all`, or `DP-3,eDP-1`                                  |
| `-t, --timeout` | `5`       | Hold in **seconds**, after the reveal                              |
| `-s, --style`   | `default` | Preset from the config file                                        |
| `--font`        | —         | Pango description, e.g. `"Monospace 72"`                           |
| `--color`       | —         | Any CSS colour GTK parses                                          |
| `--outline`     | —         | Colour or `none`; `width=`                                         |
| `--glow`        | —         | Colour or `none`; `radius=`, `alpha=`                              |
| `--scanlines`   | —         | Period in device px or `none`; `strength=`, `duty=`                |
| `--position`    | —         | `bottom-left`, `center`, …; `halign=`, `valign=`                   |
| `--margin`      | —         | Gap from the anchored edge, logical px                             |
| `--width`       | —         | Width of the text block, logical px                                |
| `--lines`       | —         | Height of the block in lines                                       |
| `--line-align`  | —         | `left`, `center`, `right` — lines inside the block                 |
| `--reveal`      | —         | `instant` or `typewriter`; `cps=`, `cursor=`, `jitter=`, `scroll=` |
| `--vanish`      | —         | Effect name; `ms=`, `cps=` on `untype`, `dir=` on `wash`           |
| `--sound`       | —         | `on`/`off`; `freq=`, `decay_ms=`, `detune=`, `gain=`, …            |
| `--raw`         | —         | Literal argument (no escape expansion)                             |
| `--config`      | XDG path  | Config file location                                               |
| `--listen`      | —         | Stay up, showing what arrives on the socket                        |
| `--send`        | —         | Send TEXT to a running listener and exit                           |
| `--socket`      | XDG path  | Listener socket                                                    |

### Flag fields

Composite flags take an optional bare value followed by comma-separated
`key=value` fields:

```sh
wayhud --glow '#b8bb26,radius=12,alpha=0.7' "HELLO"
wayhud --reveal 'typewriter,cps=50,scroll=true' "READY"
```

The bare value sets the colour for `--glow` and `--outline`, the kind for
`--reveal` and `--vanish`, or on/off for `--sound`. Named fields use the TOML
names, except `--outline width=` corresponds to `outline_width`.

Omitted fields keep the preset's values: `--glow 'alpha=0.3'` changes only
opacity. Unknown fields, duplicate values and invalid ranges are errors.
Typewriter fields require a typewriter preset or an explicit
`--reveal 'typewriter,cps=50'`.

### Input and timing

Arguments expand `\n`, `\t` and `\\` unless `--raw` is set. Stdin does not
expand escapes. Trailing newlines are removed on both paths; messages over
100000 characters are rejected with an error.

Stdin reading stops at EOF or 400001 bytes, whichever comes first. Reaching
the byte limit is an error. Nothing displays before EOF: a quiet `tail -f`
can wait indefinitely, while a continuous stream that reaches the limit
exits with an error.

`--timeout` is the hold time after reveal, in seconds; the config uses
`timeout_ms`. Reveal, hold and vanish together must fit within one hour.
Overlays cannot be dismissed early.

### Outputs

`--output current` queries sway's focused output through IPC. Socket lookup
uses `I3SOCK`, then `SWAYSOCK`, then asks `i3` or `sway` for the path. Failure
to locate the focused output is an error.

Missing named connectors are reported and skipped. If no outputs match, the
command fails.

### Terminal mode

The default typewriter fills the block from the top. With `scroll=true`, the
current line stays at the bottom and earlier lines move up:

```sh
wayhud --reveal 'scroll=true' --position bottom-left \
       "CHECKING DISKS\nMOUNTING /\nBRINGING UP eth0\nOK"
```

The surface is sized for the full message and stays fixed in both modes.
Terminal mode works at any position; `untype` reverses the scroll as lines
are erased. Use `scroll=false` to override a terminal-mode preset.

### Vanish effects

| Kind       | Effect                                                    |
| ---------- | --------------------------------------------------------- |
| `instant`  | Disappear when the hold ends                              |
| `fade`     | Fade to transparent                                       |
| `collapse` | Squash to a bright line, flash and disappear              |
| `wash`     | Erase with a moving soft edge                             |
| `untype`   | Erase characters in reverse order, with a caret and sound |
| `dissolve` | Disappear in pseudo-random blocks                         |

All effects except `instant` and `untype` accept `ms=`; `untype` is timed
with `cps=` instead. `wash` also accepts `dir=down` (default) or `dir=up`:

```sh
wayhud --vanish 'wash,dir=up,ms=700' "DONE"
```

`untype` is timed with `cps` rather than `ms`, because it is the one effect
that works a character at a time: a fixed duration erases a long message
faster per character than a short one, and a listener's block, which grows,
would vanish quicker the more it held. A longer block now takes
proportionally longer, and the unit matches `reveal.cps`. Lines are not a
measure here: a long line takes longer than a short one.

The erase runs for `(chars + 1) / cps` seconds — a beat for each character
taken off, and one more with the block empty. Without that last beat the final
character leaves with the overlay rather than being erased off it, and stands
a beat longer than every other for it.

Omitting `ms` preserves the preset's duration, or uses 420 ms when switching
from instant; `cps` falls back to 60 the same way. `--vanish 'ms=800'` changes
the current effect's duration.
`--vanish` no longer accepts the pre-1.0 aliases `wash-up`, `crt`, or `none`
(for `instant`). `--outline none` and `--glow none` remain valid.

## Config

The default path is `$XDG_CONFIG_HOME/wayhud/config.toml`, falling back to
`~/.config/wayhud/config.toml` when the variable is unset or not absolute.
Use `--config` to select another file.

Presets inherit from `[style.default]`, then from built-in defaults.
Sub-tables merge field by field; changing `kind` replaces that sub-table.
A missing file uses built-in defaults. Malformed files, unknown keys and
unknown preset names are errors.

```toml
[style.default]
font = "Monospace 72"
color = "#b8bb26"
outline = "#1d2021"
timeout_ms = 5000
reveal = { kind = "typewriter", cps = 28, cursor = true }
vanish = { kind = "collapse", ms = 420 }

[style.alert]
color = "#fb4934"
vanish = { kind = "wash", ms = 300, dir = "up" }
```

Select a preset with `wayhud --style alert "BUILD FAILED"`. See
[config.example.toml](config.example.toml) for more presets and `man 5 wayhud`
for all keys, defaults and ranges.

### Keys

| Key             | Type                    | Default                    | Meaning                                               |
| --------------- | ----------------------- | -------------------------- | ----------------------------------------------------- |
| `font`          | Pango description       | `"Monospace 72"`           | Family and size in points                             |
| `color`         | CSS colour              | `"#b8bb26"`                | Glyph fill                                            |
| `outline`       | CSS colour              | `"#1d2021"`                | Stroke colour; `"none"` disables it                   |
| `outline_width` | float, 0–128 logical px | font size / 14             | Stroke width; unset it scales with the font           |
| `glow`          | table                   | —                          | Halo behind the glyphs; `radius = 0` disables it      |
| `scanlines`     | table                   | —                          | Raster bands; `strength = 0` disables them            |
| `halign`        | `left` `center` `right` | `center`                   | Horizontal placement on the output                    |
| `valign`        | `top` `center` `bottom` | `center`                   | Vertical placement                                    |
| `margin`        | int, logical px         | `64`                       | Gap from the anchored edge to the surface             |
| `width`         | int, logical px         | measured text              | Pins the block; unset wraps to the monitor            |
| `lines`         | int                     | measured text              | Reserves the block height; also what a listener keeps |
| `line_align`    | `left` `center` `right` | `left`                     | Alignment of lines inside the block                   |
| `timeout_ms`    | int, ms (max 3600000)   | `5000`                     | Hold after reveal                                     |
| `reveal`        | table                   | typewriter, 28 cps, cursor | How the text appears                                  |
| `vanish`        | table                   | collapse, 420 ms           | How it goes away                                      |
| `sound`         | table                   | on, 2100 Hz, gain 0.22     | The typewriter blip; see Sound below                  |

A few settings affect layout:

- `margin` is measured to the surface. Visible text is inset further by
  padding for the outline, glow and caret. Centred axes ignore margins.
- `outline_width` defaults to font size / 14. `outline = "none"` disables it.
- `glow = { color = "#b8bb26", radius = 12.0, alpha = 0.55 }` adds a halo
  behind the outline. A larger radius increases padding and reduces wrapping
  width. `radius = 0` disables inherited glow.
- `width` pins the text block. Unset, lines wrap to the monitor and the
  surface is sized from the text, so it changes with the message. Set, it is
  both the wrapping budget and the block width, so the surface keeps it
  whatever arrives — the unused part is transparent, so pinning it costs
  nothing to look at. Clamped to the monitor.
- `lines` reserves the block height up front, so a message growing inside it
  does not resize the surface. Reserved room a message has not reached is
  transparent, so it costs nothing to look at. The reservation is that many
  line heights from the font, while a wrapped message occupies more screen
  lines than it has newlines — a narrow `width` therefore holds fewer than the
  count suggests. What overflows depends on `reveal.scroll`: with it the block
  scrolls and the earliest lines leave the top, without it the block fills from
  the top and the surface edge cuts the rest off.
- `line_align` aligns lines within the block, independently of its position.
  It only has room to work when `width` is pinned or the message wraps.
- `scanlines = { period = 4.0, strength = 0.35, duty = 0.5 }` cuts dimmed
  bands across the message. `period` is in **device** pixels, so the raster
  keeps its pitch whatever the font size or output scale. The bands go over
  the finished message, cutting the glow with the glyphs, and do not widen
  the padding. `strength = 0` disables an inherited raster.

`Monospace` uses the system's fontconfig default. Check named families with
`fc-match "Family Name"`; unavailable fonts fall back silently.

## Listener

`wayhud --listen` binds a datagram socket and shows what arrives on it, one
message per datagram. Same overlay, same drawing; what differs is where the
message comes from and how long the process stays.

```sh
wayhud --listen -s notify &
wayhud --send "DISK 0 OK"
printf '%s' "REACTOR NOMINAL" | socat - UNIX-SENDTO:"$XDG_RUNTIME_DIR/wayhud.sock"
```

`--send` is the client that needs no extra program. The payload is plain text
on a datagram socket, so anything that can send one works — but pick the tool
with care: `nc -uU` delivers and then waits for a reply that never comes, so
it hangs unless something kills it.

A datagram rather than a stream or a FIFO, so one send is one message: a
notification body carrying newlines needs no framing and no escaping. It also
fails the right way round — opening a FIFO for writing blocks until a reader
arrives, so a hook run from a notification daemon would leave a process hung
for every notification while nothing was listening. Sending to a socket nobody
is bound to returns an error at once, which is what a hook needs.

A message **joins** the one on screen rather than replacing it. While the
block is revealing or being held, an arrival is appended as a line and typing
carries on from wherever it had got to — so a message landing mid-line
finishes that line and goes on into the new one, rather than restarting or
snapping the old one to done. The hold then starts again.

A vanish is a commit point: what arrives during one waits for it to finish
and then starts a block of its own. That bounds a burst, but not time — a
stream arriving faster than the hold keeps restarting it, and the overlay
stays up as long as the stream does. What is bounded is the block, which
keeps the last `lines` of text, or ten when nothing is reserved, dropping
from the top.

A listener always reserves its block, because the layer surface is negotiated
once — before any message — and cannot be grown into afterwards. Unset,
`width` becomes the full monitor width and `lines` ten, both clamped to the
screen. That is a large pane, and with `scroll = true` the message sits on its
bottom line, so a listener started with no style at all puts text in a corner.
Set `width`, `lines` and a smaller `font` to place it: `[style.notify]` in
`config.example.toml` is a worked example. With `scroll = true` it reads as a
terminal — the newest line on the bottom, earlier ones rising.

The style is the listener's — a client sends text and nothing else, so a
notification hook cannot decide what the overlay looks like, and the block can
be sized before the first message arrives.

From mako, whose `on-notify=exec` hands a hook only the notification id:

```sh
# ~/.config/mako/config
# invisible=1
# on-notify=exec wayhud-notify "$id"
makoctl list -j | jq -r --argjson i "$1" \
  '.[] | select(.id==$i) | .summary + " " + .body' | wayhud --send
```

## sway

```text
bindsym $mod+Shift+h exec wayhud "LOCKED\nBACK IN 5"
```

The layer namespace is `wayhud`. Concurrent invocations create separate
overlays, stacked by the compositor.

If GTK logs `vkQueuePresentKHR` / `VK_SUBOPTIMAL_KHR` warnings, try selecting
another renderer:

```text
bindsym $mod+Shift+h exec env GSK_RENDERER=opengl wayhud "LOCKED"
```

`GSK_RENDERER=cairo` is another option. wayhud leaves renderer selection to GTK.

## Sound

The blip is synthesised, so the binary carries no samples. It is a struck
cluster: four sine partials at `1 - k * detune` of `freq`, for k of 0, 1, 2
and 2.33, with the lowest of them the loudest, plus an octave above `freq`
weighted by `brightness`. That is shaped by a 1 ms attack and an exponential
ring-out to -60 dB over `decay_ms`, then normalised so its peak is `gain`.

Which is to say: `freq` is the pitch, `decay_ms` is how long it rings,
`detune` and `brightness` are the character, and `gain` is the level.

Rates are not resolved beyond the refresh rate of the output. At 60 characters
per second a character lasts about 17 ms, one frame on a 60 Hz screen, so a
frame that slips shows one for twice as long as its neighbour — that goes for
`reveal.cps` and `vanish.cps` alike.

```toml
sound = { freq = 3200, decay_ms = 12 }    # a dry tick
sound = { freq = 900,  decay_ms = 300 }   # a small bell
sound = { detune = 0 }                    # one tone: a plain beep
sound = { every = 2 }                     # every other character
```

`every = N` blips once per N revealed characters; whitespace never blips.

Audio and animation share character timings. Tracks are mixed before display
and played through PulseAudio; the untype track is delayed until vanish starts.
Audio failures are reported on stderr and the message still displays.

## Development

```sh
nix develop
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check advisories sources
cargo machete
groff -man -Tutf8 -ww -z man/man1/wayhud.1
groff -man -Tutf8 -ww -z man/man5/wayhud.5
nix build
```

Install the hook with `git config core.hooksPath hooks`; it checks the staged
tree. CI also checks the declared Rust version and builds on aarch64.
[CLAUDE.md](CLAUDE.md) lists development commands and rendering constraints.

## License

MIT — see [LICENSE](LICENSE).
