<p align="center">
  <img src="assets/logo.png" width="128" alt="apex logo">
</p>

# apex

Ultra-fast, ultra-lightweight launcher for Windows. Raycast, but native.

- Pure Win32 + Direct2D. No Electron, no webview, no runtime.
- Single small binary. Minimal RAM. Instant startup.

## Status

Early development.

## Build

```
cargo build --release
```

## Usage

- `Ctrl+Esc` — toggle the launcher (configurable)
- `Esc` — dismiss
- `↑/↓` — move selection, `Enter` — launch

Note: `Ctrl+Esc` normally opens the Start menu; apex takes it over while
running (the Win key still opens Start).

## Configuration

`%APPDATA%\apex\config.toml` — auto-created with commented defaults on
first run. Symlink it into your dotfiles if that's how you roll.

```toml
[general]
start_menu = true        # Start menu entry, refreshed each launch
start_on_startup = true  # run apex at sign-in (HKCU Run key)

[hotkey]
modifiers = "ctrl"       # ctrl, alt, shift, win, joined with '+'; or "none"
key = "escape"           # a-z, 0-9, f1-f24, space, escape, tab, grave, enter

[plugins]
search = true            # disabled plugins are never constructed: zero cost
```

Changes take effect on restart. Disabling a `[general]` flag removes its
registration (shortcut / Run key) on the next launch.
