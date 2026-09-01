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
journalctl -f -u nginx | wayhud --typewriter 0 --color '#fb4934'
```

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

Needs Rust, plus the GTK4 / layer-shell / PulseAudio development libraries:
`gtk4`, `gtk4-layer-shell`, `glib`, `cairo`, `pango`, `gdk-pixbuf`, `graphene`,
`libpulse`. On a Nix box the devShell provides all of them:

```sh
nix develop            # or: direnv allow
cargo build --release
install -Dm755 target/release/wayhud ~/.local/bin/wayhud
install -Dm644 man/man1/wayhud.1 ~/.local/share/man/man1/wayhud.1
```

## Usage

See `man wayhud` for the full reference.

| Flag             | Default   | Meaning                                                  |
| ---------------- | --------- | -------------------------------------------------------- |
| `TEXT`           | —         | Message. Omit or pass `-` to read stdin.                  |
| `-o, --output`   | `current` | `current`, `all`, or `DP-3,eDP-1`                         |
| `-t, --timeout`  | `5`       | Hold in seconds, counted from the END of the reveal       |
| `-s, --style`    | `default` | Preset from the config file                               |
| `--font`         | —         | Pango description, e.g. `"LythMono Nerd Font 72"`         |
| `--color`        | —         | Any CSS colour GTK parses                                 |
| `--outline`      | —         | Outline colour, or `none` for flat glyphs                 |
| `--position`     | —         | `center`, `top`, `bottom-right`, …                        |
| `--typewriter`   | —         | Characters/second; `0` reveals instantly                  |
| `--vanish`       | —         | Exit effect, optionally `:MS` — see below                 |
| `--no-sound`     | —         | Stay quiet regardless of the style                        |
| `--raw`          | —         | Take the argument literally (no `\n` / `\t` expansion)    |
| `--config`       | XDG path  | Config file location                                      |

The hold timeout is measured from the **end** of the reveal, not from start-up,
so a slow typewriter doesn't eat into the reading time.

`\n` and `\t` in the argument are expanded, because sway's `exec` runs through
`sh`, which has no `$'...'`. Text arriving on stdin is used verbatim.

`--output current` asks sway over its IPC socket which output has focus —
Wayland itself gives a client no way to find that out. Without `SWAYSOCK` it
fails rather than guessing.

## Vanish effects

`--vanish <kind>[:<ms>]`, or `vanish = { kind = "...", ms = ... }` in a preset.
Without `:MS` the flag keeps whatever duration the preset already had, so you
can cycle through effects without restating the timing.

| Kind          | What it looks like                                                      |
| ------------- | ----------------------------------------------------------------------- |
| `instant`     | Gone on the frame the hold expires.                                      |
| `fade`        | Alpha to zero.                                                           |
| `collapse`    | CRT power-off: squashes to a bright line, blooms wider, blinks out.      |
| `wash-down`   | A soft edge sweeps top to bottom, erasing as it passes.                  |
| `wash-up`     | The same, bottom to top.                                                 |
| `untype`      | The caret walks back and eats the text, blipping on the way out.         |
| `dissolve`    | Falls apart into blocks in a fixed pseudo-random order.                  |

`untype` is the only one that makes noise — it is typing, so it clicks. It also
gets a caret even after an instant reveal, since otherwise characters would
disappear with nothing touching them.

## Config

`$XDG_CONFIG_HOME/wayhud/config.toml`, a flat map of named presets. Presets do
**not** inherit from each other: every field left unset falls back to the
built-in default, never to `[style.default]`. A missing file is fine; a
malformed one — or an unknown key — is an error, so a typo gets reported
instead of quietly doing nothing.

```toml
[style.default]
font = "LythMono Nerd Font 72"
color = "#b8bb26"
outline = "#1d2021"
timeout_ms = 5000
reveal = { kind = "typewriter", cps = 28, cursor = true }
vanish = { kind = "collapse", ms = 420 }

[style.alert]
color = "#fb4934"
font = "LythMono Nerd Font 96"
vanish = { kind = "wash-up", ms = 300 }
```

See [`config.example.toml`](config.example.toml) for every key with its
default.

Note that the font family has to match fontconfig exactly. `LythMono Nerd Font`
resolves; `Lyth Mono` silently falls back to the default font — as does any
other family name that doesn't exist.

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
cargo test
cargo clippy --all-targets -- -D warnings
groff -man -Tutf8 -ww -z man/man1/wayhud.1   # man page lint
```

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
