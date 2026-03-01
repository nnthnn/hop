# hop

A configurable X11 Alt+Tab window switcher written in Rust, with ARGB transparency and compositor blur support.

## Features

- ARGB 32-bit transparency — tile backgrounds are semi-transparent, icons are fully opaque
- Compositor blur hint via `_KDE_NET_WM_BLUR_BEHIND_REGION` (works with picom `blur-background = true`)
- TOML configuration file for colors, fonts, tile dimensions, and blur radius
- Pure Rust X11 via `x11rb` — no unsafe C bindings
- EWMH-compliant window list (`_NET_CLIENT_LIST_STACKING`)
- Passive key grab so the switcher doesn't need to own a hotkey daemon

## Building

```fish
cd ~/source/hop
cargo build --release
```

The binary ends up at `target/release/hop`.

## Installing

```fish
cp target/release/hop ~/.local/bin/hop
```

## Configuration

Copy the example config and edit it:

```fish
mkdir -p ~/.config/hop
cp config.example.toml ~/.config/hop/config.toml
```

Config is read from `$XDG_CONFIG_HOME/hop/config.toml` (defaults to `~/.config/hop/config.toml`). If no config is found, built-in Dracula-themed defaults are used.

### Options

```toml
[window]
tile_width   = 200     # width of each window tile in px
tile_height  = 150     # height of each window tile in px
icon_size    = 64      # icon size in px
border_width = 4       # outer window border in px
position     = "center" # "center" or "x,y"

[colors]
background = "#282a36"  # tile background color
bg_alpha   = 0.80       # tile background opacity (0.0–1.0)
foreground = "#f8f8f2"  # text and placeholder icon color
frame      = "#bd93f9"  # selected tile border color
inactive   = "#44475a"  # unselected tile border color
border     = "#6272a4"  # outer window border color

[font]
name = "Roboto"
size = 11

[blur]
enabled = true
radius  = 10  # hint to compositor; actual blur is controlled by picom config

[keys]
modifier = "Alt"
next     = "Tab"
prev     = "Shift+Tab"
cancel   = "Escape"
```

> **Note:** `[keys]` values are documented for future use. Currently the keybindings (Alt+Tab, Alt+Shift+Tab, Alt+Escape) are hardcoded.

## Autostart (XFCE)

Create `~/.config/autostart/hop.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=hop
Exec=/home/<user>/.local/bin/hop
Hidden=false
X-XFCE-Autostart-Override=true
```

You will also need to disable xfwm4's built-in Alt+Tab:

```
xfconf-query -c xfwm4 -p /general/cycle_windows_key -s ""
```

## Compositor (picom)

For blur to work, add to your `~/.config/picom/picom.conf`:

```
blur-background = true;
blur-method = "dual_kawase";
blur-strength = 8;
```

## Implementation Status

- [x] ARGB 32-bit window with compositor transparency
- [x] XRender tile background fill with configurable alpha
- [x] Frame borders (selected / unselected colors)
- [x] EWMH window list (`_NET_CLIENT_LIST_STACKING`)
- [x] Window activation via `_NET_ACTIVE_WINDOW`
- [x] `_KDE_NET_WM_BLUR_BEHIND_REGION` blur hint
- [x] TOML config loading with sane defaults
- [ ] Icon rendering from `_NET_WM_ICON` ARGB data
- [ ] Window title text rendering (Pango or XRender glyphs)
- [ ] Config-driven keybindings (currently hardcoded)
- [ ] `position = "x,y"` support (currently always centered)
- [ ] Release build + install instructions in a Makefile
