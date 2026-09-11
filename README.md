<p align="center">
  <img src="https://raw.githubusercontent.com/shivajreddy/apex/main/assets/logo.png" width="128" alt="Apex logo">
</p>

# Apex

Ultra-fast, ultra-lightweight launcher for Windows. Raycast, but native.

- Pure Win32 + Direct2D. No Electron, no webview, no runtime.
- Single small binary. Minimal RAM. Instant startup.
- One dependency: the `windows` crate.

## Status

`v0.2.2`, pre-release. Working today: global hotkey, fuzzy app search with
icons (desktop + Store apps), frecency ranking, quicklinks, apex commands, a
tray icon, aliases, and an actions panel. See [ROADMAP.md](ROADMAP.md) for
what's shipped and what's next.

## Install

```powershell
cargo install apex-launcher
```

The crate is `apex-launcher` because `apex` was taken; the binary is `apex`.
For the latest commit rather than the last release:

```powershell
cargo install --git https://github.com/shivajreddy/apex
```

### From source

```powershell
cargo build --release

New-Item -ItemType Directory -Path "$env:LOCALAPPDATA\Programs\Apex" -Force
Copy-Item .\target\release\apex.exe "$env:LOCALAPPDATA\Programs\Apex\apex.exe" -Force
Start-Process "$env:LOCALAPPDATA\Programs\Apex\apex.exe"
```

Install somewhere stable rather than running from `target\release\`: `cargo
clean` wipes that directory, and a running apex holds a lock on its own exe
that makes the next build fail.

Running apex is what installs it. `setup::ensure` runs on every launch and
registers, idempotently, from the `[general]` flags:

| Flag | Effect |
|---|---|
| `start_menu` | Start menu shortcut, pointing at the running exe |
| `start_on_startup` | `Apex` value under the `HKCU` `Run` key |

Both point at wherever the exe currently is, and both are removed on the next
launch if you set the flag to `false`.

### Upgrade

```powershell
cargo build --release
Get-Process apex -ErrorAction SilentlyContinue | Stop-Process -Force
Copy-Item .\target\release\apex.exe "$env:LOCALAPPDATA\Programs\Apex\apex.exe" -Force
Start-Process "$env:LOCALAPPDATA\Programs\Apex\apex.exe"
```

## Usage

| Key | |
|---|---|
| `Ctrl+Esc` | toggle the launcher (configurable) |
| `Esc` | dismiss |
| `↑` `↓` | move selection |
| `Enter` | open |
| `Ctrl+K` | actions for the selected result |
| `Ctrl+V` | paste, in the query and in any field |
| `Tab` | next field, in forms |

The mouse works too: hover to highlight, click to launch, wheel to scroll.

Summoning apex with an empty query lists your most-used entries under
`Suggestions`, then everything else - every app, quicklink and command -
under `Commands`. The list scrolls, so the window stays the same height
whether you have ten entries or three hundred. `Suggestions` is omitted
entirely until you have launched something.

Note: `Ctrl+Esc` normally opens the Start menu. Apex claims it with a
low-level keyboard hook - before the shell sees it - so it wins even over
reserved combos (the Win key still opens Start).

### Aliases

`Ctrl+K` on any app, `Set Alias…`, and type a short name. Typing that alias
afterwards puts the app first.

### Quicklinks

A quicklink opens a link, folder or program by name. Type `create quicklink`
to make one, or `Ctrl+K` on an existing one to edit or delete it.

A `{token}` in the link makes the quicklink take an argument: running it asks
for a value using the token's name, then substitutes it in, percent-encoding
when the link is a URL.

```toml
[quicklinks.github]
name = 'Search GitHub'
link = 'https://github.com/search?q={query}'
open_with = 'chrome'        # optional; defaults to the system handler
```

Prefer `'single quotes'`: they keep Windows paths like `C:\tools\x.exe`
literal, with no escaping.

### Curating what shows up

`Commands` lists every installed application, which usually includes
uninstallers and bundled helpers you will never launch. `Ctrl+K` on any row
offers **Hide from Apex**; `Apex: Manage Hidden Entries` lists what you hid,
with **Unhide** to put it back.

### Source folders

Portable apps and loose scripts never get a Start Menu entry, so apex cannot
see them by default. `Apex: Add Source Folder` takes a folder and indexes
what is in it - `.exe`, `.lnk`, `.bat`, `.cmd` and `.ps1`, one level deep.
`Apex: Manage Source Folders` lists what you have added, with **Remove Source
Folder** behind `Ctrl+K`; removing reindexes immediately.

```toml
[sources]
1 = 'D:\PortableApps'
```

Scanning is deliberately not recursive: a source pointed at a deep tree, or
at a drive root by mistake, would stall indexing. The keys are just indices -
a bare TOML key cannot hold a Windows path.

### Apex commands

Type `apex` to list them all; each is also reachable by its own word, and by
hidden synonyms (`startup` finds Toggle Start at Login, `exit` finds Quit).

| Command | |
|---|---|
| `Apex: Reload` | re-read apps, quicklinks, aliases and config from disk |
| `Apex: Restart` | relaunch, the only way to apply `[hotkey]` / `[general]` |
| `Apex: Quit` | exit |
| `Apex: Open Config` | open `config.toml` in your editor |
| `Apex: Open Config Folder` | reveal `%APPDATA%\apex` |
| `Apex: Toggle Tray Icon` | show/hide the tray icon, and remember the choice |
| `Apex: Toggle Start at Login` | flip `start_on_startup` and apply it now |
| `Apex: Clear Launch History` | reset frecency ranking |
| `Apex: Add Source Folder` | index an extra folder of apps and scripts |
| `Apex: Manage Source Folders` | review source folders, and remove them |
| `Apex: Manage Hidden Entries` | review what you have hidden, and restore it |

Reload picks up hand-edits to `config.toml`, so editing it in your dotfiles
and reloading is enough. `[general]` and `[hotkey]` still need a restart.

## Configuration

`%APPDATA%\apex\config.toml` - auto-created with commented defaults on first
run. Symlink it into your dotfiles if that's how you roll.

```toml
[general]
start_menu = true        # Start menu entry, refreshed each launch
start_on_startup = true  # run Apex at sign-in (HKCU Run key)
tray_icon = true         # tray icon: Open / Reload / Open Config / Quit

[hotkey]
modifiers = "ctrl"       # ctrl, alt, shift, win, joined with '+'; or "none"
key = "escape"           # a-z, 0-9, f1-f24, space, escape, tab, grave, enter

[plugins]
search = true            # disabled plugins are never constructed: zero cost
quicklinks = true
commands = true
```

Apex only ever rewrites the sections it owns - `[aliases]`, `[quicklinks.*]`,
`[sources]`, `[hidden]`, and single `[general]` keys - by line surgery. Every
other section, and every comment, is preserved byte-for-byte.

Launch history is *not* kept here. It lives in
`%LOCALAPPDATA%\apex\frecency.tsv`, because it rewrites on every launch and is
machine-local - the config file is yours, and often lives in a git repo.

## Known limitations

While an **elevated** window (e.g. Task Manager) has focus, Windows UIPI hides
keyboard input from non-elevated apps: the hotkey falls through to the shell
and Apex cannot take focus. Running Apex elevated avoids this entirely; an
opt-in `run_as_admin` setting (elevated logon task) is planned.

**Aliases are machine-specific.** Desktop apps get an AppUserModelID of the
form `Microsoft.AutoGenerated.{GUID}`, generated per machine, so an alias set
on one box will not resolve on another. Quicklinks have no such problem.

## License

MIT
