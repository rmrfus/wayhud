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
wayhud --vanish 'untype,ms=900' "THIS MESSAGE WILL SELF DESTRUCT"
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
| `--line-align`  | —         | `left`, `center`, `right` — lines inside the block                 |
| `--reveal`      | —         | `instant` or `typewriter`; `cps=`, `cursor=`, `jitter=`, `scroll=` |
| `--vanish`      | —         | Effect name; `ms=`, and `dir=` on `wash`                           |
| `--sound`       | —         | `on`/`off`; `freq=`, `decay_ms=`, `gain=`, `every=`                |
| `--raw`         | —         | Literal argument (no escape expansion)                             |
| `--config`      | XDG path  | Config file location                                               |

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

All effects except `instant` accept `ms=`. `wash` also accepts `dir=down`
(default) or `dir=up`:

```sh
wayhud --vanish 'wash,dir=up,ms=700' "DONE"
```

Omitting `ms` preserves the preset's duration, or uses 420 ms when switching
from instant. `--vanish 'ms=800'` changes the current effect's duration.
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
| `line_align`    | `left` `center` `right` | `left`                     | Alignment of lines inside the block                   |
| `timeout_ms`    | int, ms (max 3600000)   | `5000`                     | Hold after reveal                                     |
| `reveal`        | table                   | typewriter, 28 cps, cursor | How the text appears                                  |
| `vanish`        | table                   | collapse, 420 ms           | How it goes away                                      |
| `sound`         | table                   | on, 2100 Hz, gain 0.22     | The typewriter blip                                   |

A few settings affect layout:

- `margin` is measured to the surface. Visible text is inset further by
  padding for the outline, glow and caret. Centred axes ignore margins.
- `outline_width` defaults to font size / 14. `outline = "none"` disables it.
- `glow = { color = "#b8bb26", radius = 12.0, alpha = 0.55 }` adds a halo
  behind the outline. A larger radius increases padding and reduces wrapping
  width. `radius = 0` disables inherited glow.
- `line_align` aligns lines within the block, independently of its position.
- `scanlines = { period = 4.0, strength = 0.35, duty = 0.5 }` cuts dimmed
  bands across the message. `period` is in **device** pixels, so the raster
  keeps its pitch whatever the font size or output scale. The bands go over
  the finished message, cutting the glow with the glyphs, and do not widen
  the padding. `strength = 0` disables an inherited raster.

`Monospace` uses the system's fontconfig default. Check named families with
`fc-match "Family Name"`; unavailable fonts fall back silently.

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

Blips are synthesised with parameters matching
[blyamk](https://github.com/rmrfus/blyamk). Values from `blyamk -v` can be copied
into a preset's `sound` table. `every = N` plays a blip every N characters;
whitespace is silent.

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
