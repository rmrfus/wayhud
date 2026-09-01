# wayhud

Big heads-up messages over everything on sway. Text goes up as a
`zwlr_layer_shell_v1` overlay, optionally typed out terminal-style with a
caret and a blip per keystroke, and vanishes with a CRT power-off squash.

```zsh
wayhud "SYSTEM ONLINE"
wayhud -o all -t 10 --position top "BUILD FAILED"
journalctl -f -u nginx | wayhud --typewriter 0 --color '#fb4934'
```

## Build

Everything lives in the flake devShell (Rust + GTK4 + gtk4-layer-shell +
libpulse):

```zsh
nix develop            # or: direnv allow
cargo build --release
./target/release/wayhud "HELLO"
```

## Usage

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
| `--no-sound`     | —         | Stay quiet regardless of the style                        |
| `--raw`          | —         | Take the argument literally (no `\n` / `\t` expansion)    |
| `--config`       | XDG path  | Config file location                                      |

`\n` and `\t` in the argument are expanded, because sway's `exec` runs through
`sh`, which has no `$'...'`. Text arriving on stdin is never touched.

`--output current` asks sway over its IPC socket which output has focus —
Wayland itself has no way to tell a client that. Without `SWAYSOCK` it fails
rather than guessing.

## Config

`$XDG_CONFIG_HOME/wayhud/config.toml`, a flat map of presets. Presets do not
inherit from each other; every unset field falls back to the compiled-in
default. See `config.example.toml`.

```toml
[style.default]
font = "LythMono Nerd Font 72"
color = "#b8bb26"

[style.alert]
color = "#fb4934"
reveal = { kind = "typewriter", cps = 45, cursor = true }
vanish = { kind = "collapse", ms = 300 }
```

## sway

```
bindsym $mod+Shift+h exec wayhud "LOCKED\nBACK IN 5"
for_window [namespace="wayhud"] ...   # the layer namespace is "wayhud"
```

Two concurrent invocations are two processes and two layer surfaces: the
compositor stacks them. That is deliberate — this is a one-shot tool, not a
daemon.

## Sound

The typewriter blip is synthesised, not sampled — no assets, the binary is
self-contained. The knobs are the ones from
[blyamk](https://github.com/rmrfus/blyamk); dial a sound in there with
`blyamk -v` and copy the numbers into `[style.X.sound]`.

The whole track is mixed before the first frame and handed to PulseAudio in one
write, so the clicks land exactly on the characters instead of drifting.
