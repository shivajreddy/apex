<p align="center">
  <img src="assets/logo.png" width="96" alt="Apex logo">
</p>

# Apex Roadmap

Where Apex is, how it got here, and where it's going.

**Status:** `v0.2.0` — pre-release, in active development.
**Working today:** global hotkey, app search with icons, frecency ranking,
quicklinks, apex commands, tray icon, aliases.

---

## Guiding principles

These drive every decision below; when a feature conflicts with one, the
feature loses.

1. **Ultra light.** A user who only launches apps pays only for the
   launcher. Disabled plugins are never constructed — no memory, no
   threads, no startup cost.
2. **Ultra fast.** Instant summon, instant results. Work happens off the
   UI thread; the hot path stays allocation-light.
3. **Native.** Pure Win32 + Direct2D. No Electron, no webview, no runtime.
4. **Gradual.** Ship a great launcher first. Everything else arrives as
   opt-in plugins, one at a time.
5. **User-owned config.** Plain TOML, hand-editable, dotfiles-friendly.
   Apex never rewrites parts of the file it doesn't own.

---

## Where we are

| | |
|---|---|
| Binary size | 530 KB |
| Memory, idle | ~11.8 MB private |
| App index | ~150 apps in ~1 s (background thread) |
| Tests | 57 unit tests |

Idle memory is what it is because icon extraction runs during the startup
scan, loading the Windows imaging DLLs immediately and never unloading them.
There is no cheaper "before first use" state to report: the helper-process
icon cache below is the fix.

### Shipped

**Core shell**
- Borderless `WS_POPUP` window, Windows 11 rounded corners, dark mode
- Hidden from taskbar and Alt+Tab (`WS_EX_TOOLWINDOW`)
- Opens on the monitor under the cursor, per-monitor DPI aware
- Height animates to fit content
- Single instance via named mutex

**Summon & dismiss**
- Global hotkey via `WH_KEYBOARD_LL`, so Apex can claim system-reserved
  chords like `Ctrl+Esc` (Start menu) — the Raycast approach
- `RegisterHotKey` retained as a secondary path
- Reliable focus stealing without injecting synthetic input
- Dismiss on `Esc`, focus loss, outside click, or typing into another app

**Rendering**
- Direct2D + DirectWrite, software rasterizer (skips D3D/DXGI, saving
  ~50 MB) — the scene is small and redraws only on input
- Renderer released entirely while hidden
- Dark theme, blinking caret, selection highlight, result rows with icons

**Search plugin** (the first plugin)
- Indexes the shell `AppsFolder`: desktop **and** UWP/Store apps, the same
  list the Start menu shows
- fzy-style fuzzy ranking: word-start, camelCase, and consecutive-run
  bonuses; gap, lead, and length penalties
- Real app icons via `IShellItemImageFactory`, extracted once at index time
- Launch through `shell:AppsFolder\<AppUserModelID>`

**Frecency**
- One decaying weight per entry, 14-day half-life: a single number captures
  both how often and how recently something was picked
- Bonus saturates, so heavy use can outrank a slightly better match but never
  beats an exact alias
- Owned by the shell and keyed on `(plugin, payload)`, so every plugin
  inherits ranking without implementing it
- Empty query lists the most-used entries, padded from each plugin's own
  catalogue
- Machine-local, in `%LOCALAPPDATA%\apex\frecency.tsv`

**Quicklinks plugin**
- Links, folders and programs opened by name, as `[quicklinks.<slug>]`
  sub-tables in the config
- A `{token}` in the link takes an argument, prompting by the token's name and
  percent-encoding only for URLs
- Created and edited in-app through a multi-field form; slugs survive renames
  so launch history isn't orphaned
- Optional `open_with` to route through a specific program

**Commands plugin**
- `Apex: ` namespace — Reload, Restart, Quit, Open Config, Open Config Folder,
  Toggle Tray Icon, Toggle Start at Login, Clear Launch History
- Hidden keywords, so `startup` finds "Toggle Start at Login" and `exit` finds
  "Quit"
- Excluded from launch history and the default list

**Actions & aliases**
- `Ctrl+K` actions panel on the selected result
- `Set Alias…` / `Remove Alias`, persisted to `[aliases]` in config
- Exact alias match jumps the app to the top, shown as a pill beside the name
- `Open in Explorer` reveals where an entry came from

**Config & integration**
- `%APPDATA%\apex\config.toml`, auto-created with commented defaults
- Dependency-free TOML-subset parser, both quote styles
- Writes are line surgery on `[aliases]`, `[quicklinks.*]` and single
  `[general]` keys; every other line and comment is preserved byte-for-byte
- `[general]` Start menu entry, run-at-login and tray icon, all self-healing
  and cleanly removed when disabled
- `[hotkey]` custom chord, `[plugins]` per-plugin toggles
- Tray icon with Open / Reload / Open Config / Quit
- Clipboard paste in the query, prompts and forms
- Embedded app icon and version metadata

### Version history

| Version | Contents |
|---|---|
| `v0.2.0` (current) | Frecency ranking, quicklinks, apex commands, tray icon, clipboard paste, alias pills, icon fallback. |
| `v0.1.0` | Global hotkey, app search with icons, launch, aliases, actions panel. First usable launcher. |

---

## Roadmap

### v0.2 — "Feels finished" (shipped, partly)

Polish the launcher until it's the fastest path to any app.

- [x] **Frecency ranking** — recently and frequently launched apps rank
      first. The single biggest perceived-quality win.
- [x] **Tray icon** with Open / Reload / Open Config / Quit
- [x] **Live index refresh** — via `Apex: Reload`. Still manual; watching for
      installs and removals is the remaining half.
- [~] **Text editing** in the query — clipboard paste and word delete are in;
      caret movement and selection are not.
- [ ] **Icon cache on disk** — skip re-extraction at every start, and move
      extraction into a helper process so the shell imaging DLLs stay out
      of the resident set (the ~11.8 MB idle figure above is entirely this)
- [ ] **Mouse support** — hover to highlight, click to launch
- [ ] **Blur / acrylic backdrop** — needs per-pixel alpha, which the current
      `ID2D1HwndRenderTarget` cannot do; a layered window driven by
      `UpdateLayeredWindow` is the likely route
- [ ] **Fade/scale animation** on summon
- [ ] **`run_as_admin` setting** — elevated logon task, so the hotkey works
      over Task Manager and other elevated windows

### v0.3 — "More than apps"

Additional plugins. Disabled ones are never constructed and cost nothing.

- [x] ~~**Plugin: Web search**~~ — absorbed by quicklinks: a `{query}` token
      *is* a web search
- [x] ~~**Plugin: Folder jump**~~ — absorbed by quicklinks: a link can be a
      directory
- [ ] **Plugin: Window switcher** — jump to any open window
- [ ] **Plugin: System commands** — lock, sleep, restart, empty recycle bin
      (distinct from the `Apex:` commands, which act on apex itself)
- [ ] **Prefix routing** — a leading token selects a plugin, so plugins
      don't all pay the cost of every keystroke
- [ ] **Quicklink aliases** — aliases are app-only today
- [ ] **More actions** — Run as administrator, Copy path, Pin to top
      (Open file location shipped as *Open in Explorer*)

### v0.4 — "Extensible"

- [ ] **Stable plugin ABI** so plugins can live outside the binary
- [ ] **External plugin processes** — crash and memory isolation, letting
      third-party plugins ship without bloating the core
- [ ] **Plugin manifest and discovery**
- [ ] **Settings UI** rendered in Apex itself
- [ ] **Themes** — colors, fonts, and sizing from config

### v1.0 — "Ship it"

- [ ] Signed installer (also unlocks `uiAccess`, fixing elevated-window
      focus for good)
- [ ] Auto-update
- [ ] Stable config and plugin APIs, with migrations
- [ ] Documentation site
- [ ] Performance budget enforced in CI (binary size, idle memory, summon
      latency)

---

## Known limitations

- **Elevated windows.** While an elevated window (e.g. Task Manager) has
  focus, Windows UIPI hides input from non-elevated apps: the hotkey falls
  through to the shell and Apex can't take focus. Running Apex elevated
  avoids it; `run_as_admin` (v0.2) and a signed `uiAccess` build (v1.0)
  are the real fixes.
- **Idle memory** sits around 11.8 MB because icon extraction loads Windows
  imaging DLLs that are never unloaded. The helper-process icon cache
  addresses this.
- **Aliases** are bare TOML keys, so they're normalized to lowercase
  `a-z 0-9 - _ .` (spaces become `-`).
- **Aliases don't travel between machines.** Desktop apps get an
  AppUserModelID of the form `Microsoft.AutoGenerated.{GUID}`, generated per
  machine, so a shared config file resolves them on one box and not the
  other. Quicklinks have no such problem. Per-machine config files are the
  workaround today.

## Non-goals

- Electron, webviews, or any bundled runtime
- Telemetry or network calls in the core
- Features that cost memory when switched off
