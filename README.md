# wayhud

[![CI](https://github.com/rmrfus/wayhud/actions/workflows/ci.yml/badge.svg)](https://github.com/rmrfus/wayhud/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/rmrfus/wayhud?logo=github)](https://github.com/rmrfus/wayhud/releases/latest)
[![License](https://img.shields.io/github/license/rmrfus/wayhud)](LICENSE)

Big heads-up messages over everything on sway. Text goes up as a
`zwlr_layer_shell_v1` overlay, optionally typed out terminal-style with a caret
and a synthesised blip per keystroke, and leaves with one of several exit
animations.

![wayhud — a message typed out over the desktop, then dissolving](assets/demo.gif)

> Real capture, bottom-right corner of one output: `--typewriter 50` typing a
> three-line message in, then `--vanish dissolve` taking it apart.

The overlay keeps an empty input region, so it never steals a click or a
keystroke from whatever is underneath.

```sh
wayhud "SYSTEM ONLINE"
wayhud -o all -t 10 --position top "BUILD FAILED"
wayhud --vanish untype:900 "THIS MESSAGE WILL SELF DESTRUCT"
journalctl -n 3 -u nginx | wayhud --typewriter 0 --color '#fb4934'
```

Piped input is read to EOF before anything is shown, so a stream that never
ends (`journalctl -f`, `tail -f`) will never display.

## Install

### As a Nix package (flake)

The flake exposes a `default` package — no clone needed:

```sh
nix run   github:rmrfus/wayhud -- "HELLO"   # run without installing
nix build github:rmrfus/wayhud              # ./result/bin/wayhud
nix profile install github:rmrfus/wayhud    # install into your profile
```

Pull it into a NixOS / home-manager flake as an input:

```nix
{
  inputs.wayhud.url = "github:rmrfus/wayhud";
  # ...
  environment.systemPackages = [ inputs.wayhud.packages.${system}.default ];
}
```

### From source

Needs `pkg-config` and Rust 1.92 or newer (the floor comes from the gtk-rs
crates, not from our own code), plus the development headers for GTK4,
layer-shell and PulseAudio: `gtk4`, `gtk4-layer-shell`, `glib`, `cairo`,
`pango`, `gdk-pixbuf`, `graphene`, `libpulse` — the `-dev` or `-devel` half of
whatever your distribution calls them. On a Nix box `nix develop` (or `direnv
allow`) provides the lot.

```sh
make                                  # cargo build --release
sudo make install                     # into /usr/local
make install PREFIX="$HOME/.local"    # or just for yourself
make uninstall                        # the PREFIX you installed with
```

`make install` is there because `cargo install` copies the binary and nothing
else. Both man pages go with it — `man 5 wayhud` is the config reference the
rest of this README keeps pointing at. Packagers get the usual staging
variable, and `install` never rebuilds, so running it under `sudo` cannot
leave `target/` owned by root:

```sh
make && make install DESTDIR="$pkgdir" PREFIX=/usr
```

## Usage

See `man 1 wayhud` for the full reference, and `man 5 wayhud` for the config
file format.

| Flag            | Default   | Meaning                                                |
| --------------- | --------- | ------------------------------------------------------ |
| `TEXT`          | —         | Message. Omit or pass `-` to read stdin.               |
| `-o, --output`  | `current` | `current`, `all`, or `DP-3,eDP-1`                      |
| `-t, --timeout` | `5`       | Hold in seconds, counted from the END of the reveal    |
| `-s, --style`   | `default` | Preset from the config file                            |
| `--font`        | —         | Pango description, e.g. `"Monospace 72"`               |
| `--color`       | —         | Any CSS colour GTK parses                              |
| `--outline`     | —         | Outline colour, or `none` for flat glyphs              |
| `--position`    | —         | `center`, `top`, `bottom-right`, …                     |
| `--typewriter`  | —         | Characters/second; `0` reveals instantly               |
| `--jitter`      | —         | Stagger keystroke gaps by ±this fraction (0–1)         |
| `--vanish`      | —         | Exit effect, optionally `:MS` — see below              |
| `--no-sound`    | —         | Stay quiet regardless of the style                     |
| `--raw`         | —         | Take the argument literally (no `\n` / `\t` expansion) |
| `--config`      | XDG path  | Config file location                                   |

The hold timeout is measured from the **end** of the reveal, not from start-up,
so a slow typewriter doesn't eat into the reading time.

The whole lifetime — reveal plus hold plus vanish — is capped at one hour, and a
message that would exceed it is refused rather than shown: nothing can dismiss a
HUD early, so a fat-fingered `--timeout`, a `--typewriter 0.01` or an absurd
`--vanish fade:99999999` would all strand it on screen.

`--jitter` needs a typewriter reveal: with `--typewriter 0`, or a preset whose
`reveal` is instant, it is an error rather than a flag that quietly does
nothing.

`\n` and `\t` in the argument are expanded, because sway's `exec` runs through
`sh`, which has no `$'...'`. Text arriving on stdin is used verbatim.

`--output current` asks sway over its IPC socket which output has focus —
Wayland itself gives a client no way to find that out. The socket is located
the way `swaymsg` does it (`I3SOCK`, `SWAYSOCK`, then asking `i3` or `sway`
for its path); if none of that works, it fails rather than guessing. A named
connector that doesn't exist is reported on stderr and skipped, so
`-o DP-3,DP-9` still shows up on DP-3 with DP-9 unplugged; matching nothing at
all is an error.

## Vanish effects

`--vanish <kind>[:<ms>]`, or `vanish = { kind = "...", ms = ... }` in a preset.
Without `:MS` the flag keeps whatever duration the preset already had, so you
can cycle through effects without restating the timing.

| `--vanish`  | In a preset                       | What it looks like                                                  |
| ----------- | --------------------------------- | ------------------------------------------------------------------- |
| `instant`   | `{ kind = "instant" }`            | Gone on the frame the hold expires.                                 |
| `fade`      | `{ kind = "fade" }`               | Alpha to zero.                                                      |
| `collapse`  | `{ kind = "collapse" }`           | CRT power-off: squashes to a bright line, blooms wider, blinks out. |
| `wash-down` | `{ kind = "wash", dir = "down" }` | A soft edge sweeps top to bottom, erasing as it passes.             |
| `wash-up`   | `{ kind = "wash", dir = "up" }`   | The same, bottom to top.                                            |
| `untype`    | `{ kind = "untype" }`             | The caret walks back and eats the text, blipping on the way out.    |
| `dissolve`  | `{ kind = "dissolve" }`           | Falls apart into blocks in a fixed pseudo-random order.             |

The two spellings are not interchangeable: the flag folds the direction into
the name, the config keeps one `wash` kind with a separate `dir`. The flag also
takes `none` for `instant`, `crt` for `collapse`, and a bare `wash` for
`wash-down`.

`untype` is the only one that makes noise — it is typing, so it clicks. It also
gets a caret even after an instant reveal, since otherwise characters would
disappear with nothing touching them.

## Config

`$XDG_CONFIG_HOME/wayhud/config.toml` — or `~/.config/wayhud/config.toml` when
that variable is unset, empty, or not an absolute path — is a flat map of named
presets picked with `--style`.
A missing file is fine: every preset is then just the built-in defaults. A
malformed one, or an unknown key, is an error, so a typo gets reported instead
of quietly doing nothing.

`[style.default]` is the base for every other preset: a key a preset does not
set is taken from there, and only then from the built-in default. Sub-tables
(`reveal`, `vanish`, `sound`) merge key by key — except when the preset picks a
different `kind`, which replaces the table outright, since the leftover keys
would belong to the other variant.

```toml
[style.default]
font = "FiraCode Nerd Font 72"   # a real family; must match fontconfig
color = "#b8bb26"
outline = "#1d2021"
timeout_ms = 5000
reveal = { kind = "typewriter", cps = 28, cursor = true }
vanish = { kind = "collapse", ms = 420 }

# Inherits the font, outline and timeout above; changes what it names.
[style.alert]
color = "#fb4934"
vanish = { kind = "wash", ms = 300, dir = "up" }
```

### Keys

| Key             | Type                    | Default                    | Meaning                                               |
| --------------- | ----------------------- | -------------------------- | ----------------------------------------------------- |
| `font`          | Pango description       | `"Monospace 72"`           | Family and size in points                             |
| `color`         | CSS colour              | `"#b8bb26"`                | Glyph fill                                            |
| `outline`       | CSS colour              | `"#1d2021"`                | Stroke colour; `"none"` or omit for none              |
| `outline_width` | float, logical px       | font size / 14             | Stroke width; unset it scales with the font           |
| `halign`        | `left` `center` `right` | `center`                   | Horizontal placement on the output                    |
| `valign`        | `top` `center` `bottom` | `center`                   | Vertical placement                                    |
| `margin`        | int, logical px         | `64`                       | Gap from the anchored edge; ignored on a centred axis |
| `line_align`    | `left` `center` `right` | `left`                     | Alignment of lines inside the block                   |
| `timeout_ms`    | int, ms (max 3600000)   | `5000`                     | Hold, counted from the END of the reveal              |
| `reveal`        | table                   | typewriter, 28 cps, cursor | How the text appears                                  |
| `vanish`        | table                   | collapse, 420 ms           | How it goes away                                      |
| `sound`         | table                   | on, 2100 Hz, gain 0.22     | The typewriter blip                                   |

`reveal` is `{ kind = "instant" }` or
`{ kind = "typewriter", cps = F, cursor = BOOL, jitter = F }`. `jitter` (0–1,
default 0) staggers each keystroke gap by up to that fraction either way, so
the typing stops sounding like a metronome; the blips use the same moments as
the glyphs, so they cannot drift apart.

`vanish` takes the kinds from the *In a preset* column under
[Vanish effects](#vanish-effects) plus `ms`; note that `wash` is one kind there
carrying a `dir` of `down` or `up`, not the two names the flag uses.

`sound` is `{ enabled, freq, decay_ms, gain, every }` — knob names match
[blyamk](https://github.com/rmrfus/blyamk), so a sound dialled in there with
`blyamk -v` transfers verbatim. `every = N` blips once per N characters;
whitespace never blips.

Full reference with every value type and range: **`man 5 wayhud`**, or
[`config.example.toml`](config.example.toml) for a working file.

The default `Monospace` is a fontconfig generic — like `Sans` and `Serif` — so
it resolves to whatever the system has configured for that role and works on a
box that has never heard of your typeface.

A real family name has to be spelled the way fontconfig spells it, which is
rarely how the vendor writes it. `FiraCode Nerd Font` resolves; `Fira Code`
falls back to the system default, and so does any other name that doesn't
exist. Nothing warns you — and for a HUD that fallback is usually a
proportional face where a fixed-width one was meant. Check the name first:

```sh
fc-match "Fira Code"        # DejaVu Sans — not what you asked for
fc-match "FiraCode Nerd Font"
```

## sway

```
bindsym $mod+Shift+h exec wayhud "LOCKED\nBACK IN 5"
```

The layer namespace is `wayhud`, so compositor rules can key off it.

Two concurrent invocations are two processes and two layer surfaces, which the
compositor stacks. That is deliberate: this is a one-shot tool, not a daemon.

## Sound

The typewriter blip is synthesised rather than sampled, so the binary carries
no assets. The knobs are the ones from
[blyamk](https://github.com/rmrfus/blyamk); dial a sound in there with
`blyamk -v` and copy the numbers into a `[style.X.sound]` block.

The whole click track is mixed before the first frame and handed to PulseAudio
in one write, so the clicks stay locked to the characters instead of inheriting
the sound server's per-write scheduling jitter. If no sound server is
reachable, the message still goes up and the failure is reported on stderr.

## Development

```sh
nix develop
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo deny check advisories sources           # RustSec, and source pinning
cargo machete                                 # dependencies nothing imports
groff -man -Tutf8 -ww -z man/man1/wayhud.1    # man page lint
groff -man -Tutf8 -ww -z man/man5/wayhud.5
nix build                                     # what `nix run github:…` does
```

That is the whole of CI, in the same order and with the same flags. `--locked`
matters: without it cargo may update the committed lockfile, and a build that
did is not the build CI checked. `nix build` is worth running before a push —
it compiles the flake and runs the suite a second time in a sandbox with no
network and no `$HOME`, which is where a test that quietly depended on either
finally says so.

Install the pre-commit hook once per clone — it lints the index, not the
working tree:

```sh
git config core.hooksPath hooks
```

`CLAUDE.md` records the non-obvious constraints (why the text is drawn by hand
instead of with a `GtkLabel`, why there is no `GtkApplication`, why the window
is sized from the full text). Worth reading before changing the rendering path.

## License

MIT — see [LICENSE](LICENSE).
