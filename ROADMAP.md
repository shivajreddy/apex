<p align="center">
  <img src="assets/logo.png" width="96" alt="Apex logo">
</p>

# Apex Roadmap

Where Apex is, how it got here, and where it's going.

**Status:** `v0.1.0` — pre-release, in active development.
**Working today:** global hotkey, app search with icons, launch, aliases.

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
| Binary size | 172 KB |
| Memory, idle before first use | ~1.6 MB private |
| Memory, after use | ~11 MB private |
| App index | 133 apps in ~1.0 s (background thread) |
| Tests | 16 unit tests + scripted UI/e2e checks |

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

**Actions & aliases**
- `Ctrl+K` actions panel on the selected result
- `Set Alias…` / `Remove Alias`, persisted to `[aliases]` in config
- Exact alias match jumps the app to the top with a badge

**Config & integration**
- `%APPDATA%\apex\config.toml`, auto-created with commented defaults
- Dependency-free TOML-subset parser
- `[general]` Start menu entry and run-at-login, both self-healing and
  cleanly removed when disabled
- `[hotkey]` custom chord, `[plugins]` per-plugin toggles
- Embedded app icon and version metadata

### Version history

| Version | Contents |
|---|---|
| `v0.1.0` (current) | Everything above. First usable launcher. |

---

## Roadmap

### v0.2 — "Feels finished"

Polish the launcher until it's the fastest path to any app.

- [ ] **Frecency ranking** — recently and frequently launched apps rank
      first. The single biggest perceived-quality win.
- [ ] **Tray icon** with Settings / Restart / Quit (retires the `Ctrl+Q`
      dev shortcut)
- [ ] **Icon cache on disk** — skip re-extraction at every start, and move
      extraction into a helper process so the shell imaging DLLs stay out
      of the resident set (targets ~2 MB idle again)
- [ ] **Text editing** in the query: caret movement, word delete, selection
- [ ] **Mouse support** — hover to highlight, click to launch
- [ ] **Fade/scale animation** on summon
- [ ] **Live index refresh** when apps are installed or removed
- [ ] **`run_as_admin` setting** — elevated logon task, so the hotkey works
      over Task Manager and other elevated windows

### v0.3 — "More than apps"

Additional plugins, all **off by default**. Enabling one is a deliberate
line in the config; disabled ones cost nothing.

- [ ] **Plugin: Window switcher** — jump to any open window
- [ ] **Plugin: System commands** — lock, sleep, restart, empty recycle bin
- [ ] **Plugin: Web search** — hand the query to a search engine
- [ ] **Plugin: Folder jump** — open frequently used directories
- [ ] **Prefix routing** — a leading token selects a plugin, so plugins
      don't all pay the cost of every keystroke
- [ ] **More actions** — Run as administrator, Open file location, Copy
      path, Pin to top

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
- **Idle memory after first use** sits around 11 MB because icon
  extraction loads Windows imaging DLLs that are never unloaded. The v0.2
  helper-process icon cache addresses this.
- **Aliases** are bare TOML keys, so they're normalized to lowercase
  `a-z 0-9 - _ .` (spaces become `-`).

## Non-goals

- Electron, webviews, or any bundled runtime
- Telemetry or network calls in the core
- Features that cost memory when switched off
