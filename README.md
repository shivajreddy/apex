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

- `Ctrl+Esc` — toggle the launcher
- `Esc` — dismiss
- `↑/↓` — move selection, `Enter` — launch

Note: `Ctrl+Esc` normally opens the Start menu; apex takes it over while
running (the Win key still opens Start).
