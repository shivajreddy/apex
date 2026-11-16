<p align="center">
  <img src="https://raw.githubusercontent.com/shivajreddy/apex/main/assets/logo.png" width="128" alt="Apex logo">
</p>

# Apex

Ultra-fast, ultra-lightweight launcher for Windows. Raycast, but native.

- Pure Win32 + Direct2D. No Electron, no webview, no runtime.
- Single small binary. Minimal RAM. Instant startup.
- One dependency: the `windows` crate.

## Status

`v0.6.6`, pre-release. Working today: global hotkey (runs elevated so it works
over admin windows), fuzzy app search with icons (desktop + Store apps),
frecency ranking, quicklinks, apex commands, a tray icon, aliases, an actions
panel, and a live acrylic backdrop. See [ROADMAP.md](ROADMAP.md) for
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
| `start_on_startup` | elevated logon task (`Apex Elevated Logon`) |

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
| `Esc` | dismiss; in a quicklink's argument field, back to the query |
| `↑` `↓` | move selection |
| `Enter` | open |
| `Ctrl+K` | actions for the selected result |
| `←` `→` `Home` `End` | move the caret; `Ctrl` jumps by word, `Shift` selects |
| `Ctrl+A` | select all |
| `Ctrl+X` `Ctrl+C` `Ctrl+V` | cut, copy, paste - in the query and in any field |
| `Tab` `Shift+Tab` | into and between a quicklink's argument fields; next field, in forms |

The mouse works too: hover to highlight, click to launch, wheel to scroll,
drag the scrollbar (or click its track to jump), click in the query to place
the caret.

Summoning apex with an empty query lists your most-used entries under
`Suggestions`, then everything else - every app, quicklink and script -
under `Commands`. The list scrolls, so the window stays the same height
whether you have ten entries or three hundred. `Suggestions` is omitted
entirely until you have launched something.

Note: `Ctrl+Esc` normally opens the Start menu. Apex claims it with a
low-level keyboard hook, on its own thread so a busy moment never makes it
miss the chord - before the shell sees it, so it wins even over reserved
combos (the Win key still opens Start).

If summoning is unreliable, another app is almost certainly intercepting the
key first. `Ctrl+Esc` is a shell chord, and tools that install their own
keyboard hooks - a tiling window manager, Windhawk, remappers - sit in the
same hook chain and can swallow the real keypress before Apex sees it. The
robust fix is a chord nothing else claims:

```toml
[hotkey]
modifiers = "ctrl"
key = "space"
```

Tiling window managers also try to manage the popup. Tell yours to leave it
alone - in GlazeWM, a `window_rules` `ignore` on `window_process: 'apex'`.

If the Start menu is open when you summon apex, apex closes it first: Start
sits in a shell layer above every other window and would otherwise cover the
launcher.

### Aliases

`Ctrl+K` on any app or quicklink, `Set Alias…`, and type a short name. Typing
that alias afterwards puts it first. Aliases live in `[aliases]`; a
quicklink's alias points at its key (`gh = "github"`). Esc in a form or prompt
opened from the actions panel steps back to that panel.

### Quicklinks

A quicklink opens a link, folder or program by name. Type `create quicklink`
to make one, or `Ctrl+K` on an existing one to edit or delete it.

A placeholder in the link makes the quicklink take an argument, filled in
right in the search bar, the way Raycast does it. Select the quicklink and a
field for each argument appears after the query: `Tab` moves into them
(`Shift+Tab` back, and `←` `→` carry on from either end of one), `Esc`
returns to the query, and `Enter` opens the link with the values
substituted in - percent-encoded when the link is a URL. Typing the
quicklink's alias and then a space jumps straight into its first argument,
and Backspace in an empty one steps back out. `Enter` with a required
argument still empty puts the caret there instead of opening anything.

```toml
[quicklinks.gify]
name = 'Gify'
link = 'https://giphy.com/search/{argument name="search"}'
open_with = 'chrome'        # optional; defaults to the system handler
```

Raycast's syntax works as written, so links copy straight across:

| Placeholder | |
|---|---|
| `{argument name="search"}` | an argument; placeholders sharing a name share one field |
| `{argument}` | an unnamed argument, a field of its own each time |
| `{argument name="lang" default="en"}` | optional: left empty, the default is used |
| `{argument name="repo" \| raw}` | substituted as typed, not percent-encoded |
| `{query}` | apex's original form, named by the word inside |

A link takes up to three arguments. The modifiers `raw`, `percent-encode`,
`trim`, `uppercase` and `lowercase` chain after a `|`; any other is ignored,
and `options="..."` is accepted but shown as a plain text field. Raycast's
other placeholders, such as `{clipboard}` or `{date}`, are not filled in
automatically: each becomes an argument of that name. A `{GUID}`, as in
`shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}`, is left alone.

A link that is a path may start with `~` and use `%NAME%` environment
variables - expanded in the link's own text, never in a typed value - so one
config can serve machines whose user folders differ.

Prefer `'single quotes'`: they keep Windows paths like `C:\tools\x.exe`
literal, with no escaping, and Raycast's double quotes as they are.

### Scripts

Every script in a script folder is a command: drop `flush-dns.ps1` into one
and typing `flush` finds it. Apex runs each with the program that runs that
kind of script - not whatever its file type opens, which for a `.ps1` is
usually an editor:

| Script | Runs with |
|---|---|
| `.ps1` | PowerShell 7 (`pwsh`), else Windows PowerShell |
| `.bat` `.cmd` | `cmd` |
| `.sh` | Git Bash - never WSL's `bash` |
| `.py` `.pyw` | the Python launcher (`py`), else `python` |

The folders are the `[scripts]` list:

```toml
[scripts]
1 = '~\dotfiles\windows\scripts\apex'
2 = 'D:\tools\scripts'
```

With no `[scripts]` section at all, apex looks in
`~\dotfiles\windows\scripts\apex`, so every machine that shares a dotfiles
repo gets its scripts with no config. Listing folders replaces that default,
so include it to keep it; an empty `[scripts]` means no folders. A folder a
machine does not have is skipped without complaint, so one list can serve
several machines. Paths may start with `~` and use `%NAME%` variables.
Folders are read one level deep - helpers can live in a subfolder - at
startup and on `Apex: Reload`, which is what picks up a new script. A
script's name is its file name, without the extension.

A script runs from its own folder, as you rather than as administrator
(`Ctrl+K` offers **Run as Administrator**), and with no window at all: it
runs in the background, with its input closed, so one that stops to ask for
something ends rather than waiting unseen. `Ctrl+K` > **Run in Terminal**
runs it once in a terminal instead, to see its output, or why it failed;
**Open in Explorer** shows where it lives. PowerShell runs scripts with
`-ExecutionPolicy Bypass`, as a shortcut to them would: choosing to run one
is the decision the policy exists to ask for.

For a script that takes arguments, or should always show its window, make a
quicklink to it - a quicklink whose link is a script runs it the same way:

```toml
[quicklinks.deploy]
name = 'Deploy'
link = '~\scripts\deploy.ps1 -Env {argument name="env" default="staging"}'
window = 'keep'
```

Whatever follows the script's path is its arguments, split like a command
line - wrap one with spaces in double quotes. Placeholders work as in any
other link, and a value typed into one stays a single argument, spaces and
all. `window` sets what the script's window does:

| `window` | |
|---|---|
| `hidden` | no window - the default |
| `keep` | a terminal that stays open afterwards, at a prompt in the script's folder |
| `close` | a terminal that closes when the script ends |

With `open_with` set, a script is opened by that program instead of run - in
an editor, say.

### Curating what shows up

`Commands` lists every installed application, which usually includes
uninstallers and bundled helpers you will never launch. `Ctrl+K` on any row
offers **Hide from Apex**; `Apex: Manage Hidden Entries` lists what you hid,
with **Unhide** to put it back.

For the whole picture, `Apex: Sources & Commands` lists every source apex
draws from - its own commands, quicklinks, scripts, each source folder, and
the applications - as sections, with each entry marked **On** or **Off**. `Enter`
flips the highlighted entry (turned-off rows are drawn faded), and `Ctrl+K`
also offers **Turn Off All** / **Turn On All** for that source. It is the
same switch as hiding: an entry that is off never appears in results.

### Source folders

Portable apps and loose scripts never get a Start Menu entry, so apex cannot
see them by default. `Apex: Add Source Folder` takes a folder and indexes
what is in it - `.exe` and `.lnk` files, and the scripts apex runs (`.ps1`,
`.bat`, `.cmd`, `.sh`, `.py`, `.pyw`) - one level deep. A script there runs
as one in a quicklink does, with no window; `Ctrl+K` > **Run in Terminal**
shows one. `Apex: Manage Source Folders` lists what you have added, with
**Remove Source Folder** behind `Ctrl+K`; removing reindexes immediately.

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
| `Apex: Reload` | re-read apps, quicklinks, scripts, aliases and config from disk |
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
| `Apex: Sources & Commands` | every source and its items, each with an on/off switch |

Reload picks up hand-edits to `config.toml`, so editing it in your dotfiles
and reloading is enough. `[general]` and `[hotkey]` still need a restart.

## Configuration

`%APPDATA%\apex\config.toml` - auto-created with commented defaults on first
run. Symlink it into your dotfiles if that's how you roll.

```toml
[general]
start_menu = true        # Start menu entry, refreshed each launch
start_on_startup = true  # run Apex at sign-in (elevated logon task)
tray_icon = true         # tray icon: Open / Reload / Open Config / Quit

[appearance]
backdrop = "acrylic"     # blur what's behind the window; "none" for solid
theme = "system"         # follow Windows' light/dark setting; or "dark", "light"
opacity = 0.5            # tint over the blur: 0.0 clear .. 1.0 solid
animation = true         # fade in on summon; false shows it instantly

[hotkey]
modifiers = "ctrl"       # ctrl, alt, shift, win, joined with '+'; or "none"
key = "escape"           # a-z, 0-9, f1-f24, space, escape, tab, grave, enter

[plugins]
search = true            # disabled plugins are never constructed: zero cost
quicklinks = true
scripts = true
commands = true
```

The frosted background is Windows 11's own system backdrop
(`DWMWA_SYSTEMBACKDROP_TYPE`, the same material Flow Launcher and PowerToys
use), composited live by DWM behind an ordinary window: a video, a window
change or a workspace switch behind apex blurs through as it happens, and it
is there on the very first frame of every summon. `opacity` is the tint apex
paints over it. On Windows before 22H2 (22621) apex falls back to the older
blur-behind; with "Transparency effects" off, Windows draws it as a flat tint.

Apex only ever rewrites the sections it owns - `[aliases]`, `[quicklinks.*]`,
`[sources]`, `[hidden]`, and single `[general]` keys - by line surgery. Every
other section, and every comment, is preserved byte-for-byte.

Launch history is *not* kept here. It lives in
`%LOCALAPPDATA%\apex\frecency.tsv`, because it rewrites on every launch and is
machine-local - the config file is yours, and often lives in a git repo.

The application index lives beside it, as `index.bin`. A helper process
(`apex --index`, the same exe) enumerates installed apps and extracts their
icons, writes the file, and exits, so the shell's enumeration and imaging
libraries never load into the launcher itself. Apex reads the last index at
startup and refreshes it in the background; `Apex: Reload` refreshes it on
demand. It is safe to delete.

## Runs as administrator

Apex requires administrator (its manifest asks for it). This is deliberate:
the global hotkey is a low-level keyboard hook, and while an **elevated**
window (Task Manager, an installer, regedit) has focus, Windows UIPI hides
keyboard input from a medium-integrity process - so a non-elevated apex simply
cannot be summoned over those. Running elevated is the only way the hotkey
works everywhere.

- **First launch prompts for UAC.** After that, start-at-login uses a
  scheduled task with highest privileges (`Apex Elevated Logon`), so apex
  starts elevated at sign-in with no prompt. `start_on_startup = false`
  removes the task. (The `HKCU` Run key can't launch an elevation-required
  exe, so it is no longer used.)
- **Apps you launch still run unelevated.** An elevated process would
  otherwise start everything as administrator - Windows never consults the
  target's manifest. So apex hands each launch to Explorer, which forwards it
  to the desktop shell running as you, and the app comes back at normal
  integrity. A program with arguments - a script's interpreter, a quicklink's
  `open_with` - can't be forwarded that way, so apex asks the desktop's
  Explorer to start it through its automation object, the documented route
  from an elevated process to an unelevated one; only with no Explorer
  running as the shell does it start elevated.
- **Run as Administrator on demand.** `Ctrl+K` on any app or script offers
  **Run as Administrator**, which starts it with apex's own elevated token -
  no UAC prompt, since you already answered one for apex. Not offered for
  Store apps, which cannot be elevated.
- To stop apex running elevated, uninstall this build and the task:
  `schtasks /delete /tn "Apex Elevated Logon" /f`.

## Known limitations

**Aliases are machine-specific.** Desktop apps get an AppUserModelID of the
form `Microsoft.AutoGenerated.{GUID}`, generated per machine, so an alias set
on one box will not resolve on another. Quicklinks have no such problem.

## License

MIT
