<p align="center">
  <img src="assets/logo.png" width="96" alt="Apex logo">
</p>

# Apex Roadmap

Where Apex is, how it got here, and where it's going.

**Status:** `v0.4.0` — pre-release, in active development.
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
| Binary size | 586 KB |
| Memory, idle | ~7.5 MB private at start, ~9.7 MB after the first summon (was 12.8 / 22.4) |
| App index | ~150 apps, loaded from disk in under 1 ms; rebuilt by a helper process in ~1.5 s |
| Tests | 88 unit tests |

The launcher process never enumerates the shell or touches the imaging
stack: the index helper below does, in a process that exits. Measured in a
bare process, enumerating the AppsFolder alone loads 58 DLLs and adds 8 MB
of private memory; reading the index file adds none.

### Shipped

**Core shell**
- Borderless `WS_POPUP` window, Windows 11 rounded corners, dark mode
- Acrylic backdrop, self-rendered: on each summon, while the window is
  positioned but still hidden, apex captures the screen behind it, blurs it,
  and bakes that frost straight into the window bitmap with the dark tint on
  top — the Raycast look. It does not use the DWM system backdrop, which
  lagged, dropped to solid on re-show, and sometimes never appeared; the
  self-rendered frost is there on the first frame, every summon, on any
  Windows version. `[appearance] backdrop = "none"` paints it solid;
  `opacity` sets the tint strength over the blur.
- Instant show and hide by default. An optional 110 ms fade-in
  settle is available with `[appearance] animation = true`, but off by
  default — an instant summon reads as snappier.
- Light and dark palettes, following Windows' app theme setting at each
  summon; `[appearance] theme = "dark" | "light"` pins one.
- Hidden from taskbar and Alt+Tab (`WS_EX_TOOLWINDOW`)
- Opens on the monitor under the cursor, per-monitor DPI aware
- Height follows the content
- Single instance via named mutex

**Summon & dismiss**
- Global hotkey via `WH_KEYBOARD_LL`, so Apex can claim system-reserved
  chords like `Ctrl+Esc` (Start menu) — the Raycast approach
- The hook runs on a dedicated thread that does nothing else, so it always
  beats `LowLevelHooksTimeout`. On the UI thread the chord was silently
  dropped whenever a render, launch or setting-change broadcast was in
  flight, which is what made summoning need several presses.
- `RegisterHotKey` retained as a secondary path
- Reliable focus stealing without injecting synthetic input: the toggle
  re-focuses an already-open window rather than hiding it, so a lost
  foreground race never turns the next press into a dismiss
- Dismiss on `Esc`, focus loss, outside click, or typing into another app,
  all funnelled through one guarded path. A post-summon guard window and a
  show/hide re-entrancy interlock stop the window from dismissing itself in
  the few ms between appearing and winning focus - the bug where typing
  immediately after the hotkey made it vanish and lose the query.

**Rendering**
- Direct2D + DirectWrite, software rasterizer (skips D3D/DXGI, saving
  ~50 MB) — the scene is small and redraws only on input
- Drawn into a 32-bit DIB and presented with `UpdateLayeredWindow`. The
  frosted background is composited into that DIB directly: the captured
  screen behind the window is downscaled hard, box-blurred, and stretched
  back up with bilinear filtering (which finishes the blur), then the tint
  and content are drawn over it. No compositor effect is involved, so the
  frost cannot lag, drop out, or depend on a Windows version.
- Renderer released entirely while hidden
- Dark theme, blinking caret, selection highlight, result rows with icons
- Text editing in every field: caret movement by char and word, Home/End,
  Shift-selection, select all, Delete, cut/copy/paste, click to place the
  caret; long text scrolls to keep the caret in view
- Sectioned default list: `Suggestions` from launch history, then `Commands`
  holding every app, quicklink and apex command. The heading is omitted when
  there is no history to show.
- Scrolling list with a fixed viewport, so the default view can hold every
  installed entry without filling the screen. Rows outside it are culled.
- Mouse: hover to highlight, click to launch, wheel to scroll

**Curating the catalogue**
- `Hide from Apex` on any row, from a shell-level action appended to whatever
  the owning plugin offered, so one implementation covers every entry type
- `[sources]`: extra folders indexed alongside the Start menu, one level deep,
  for portable apps and loose scripts
- Management views for both, reachable as `Apex:` commands

**Search plugin** (the first plugin)
- Indexes the shell `AppsFolder`: desktop **and** UWP/Store apps, the same
  list the Start menu shows
- fzy-style fuzzy ranking: word-start, camelCase, and consecutive-run
  bonuses; gap, lead, and length penalties
- Real app icons via `IShellItemImageFactory`
- Launch through `shell:AppsFolder\<AppUserModelID>`

**Index helper**
- Enumeration and icon extraction run in a helper process (`apex --index`,
  the same exe) that writes `%LOCALAPPDATA%\apex\index.bin` and exits, so
  the shell's enumeration and imaging DLLs never load into the launcher
- The launcher reads the last index at startup — every app and icon is
  there before the first summon — then runs the helper in the background
  to pick up installs and removals; `Apex: Reload` runs it too
- Bitmaps are stored once and shared, so the file stays around 1 MB; a
  corrupt or truncated file is treated as absent, never trusted
- If the helper cannot be started at all, the launcher indexes in-process
  rather than showing nothing

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
- `[hotkey]` custom chord, `[plugins]` per-plugin toggles, `[appearance]`
  backdrop and animation
- Tray icon with Open / Reload / Open Config / Quit
- Clipboard cut, copy and paste in the query, prompts and forms
- Embedded app icon and version metadata

### Version history

| Version | Contents |
|---|---|
| `v0.4.0` (current) | Runs elevated so the hotkey works over admin windows; self-rendered acrylic backdrop; fixed window size; reliable summon (dedicated hook thread, guard window); alias-pill polish. |
| `v0.3.0` | Out-of-process app index, shared line editor, acrylic backdrop, summon animation. |
| `v0.2.2` | Hide entries, extra source folders, and management views for both. |
| `v0.2.1` | Sectioned and scrolling result list, mouse support. |
| `v0.2.0` | Frecency ranking, quicklinks, apex commands, tray icon, clipboard paste, alias pills, icon fallback. |
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
- [x] **Text editing** in the query — caret movement, word jumps,
      selection, cut/copy/paste, click to place the caret; the same editor
      backs the prompt and form fields.
- [x] **Icon cache on disk** — became the index helper: the whole index,
      not just the icons, is built by a helper process and read from disk,
      because measurement showed enumerating the AppsFolder costs more
      resident memory than extracting the icons does.
- [x] **Mouse support** — hover to highlight, click to launch, wheel to scroll
- [x] **Blur / acrylic backdrop** — self-rendered. First tried the DWM
      system backdrop (`DWMSBT_TRANSIENTWINDOW`); it lagged, dropped to
      solid on re-show, and proved too unreliable. Replaced with capturing
      the screen behind the window, blurring it, and baking it into the
      layered-window bitmap — frost on the first frame, every summon, any
      Windows version, no compositor dependency.
- [x] **Fade/scale animation** on summon — scale 96.5% → 100% and content
      opacity 60% → 100% over 110 ms, eased. Per-pixel, because a layered
      window's constant alpha below 255 makes the compositor drop the blur.
- [x] **Runs as administrator** — mandatory, via a `requireAdministrator`
      manifest, so the hotkey works over Task Manager and other elevated
      windows. Start-at-login uses a scheduled logon task with highest
      privileges (no UAC prompt at sign-in). Launched apps are handed to
      Explorer so they run unelevated (an `open_with` quicklink is the one
      documented exception).
      The earlier opt-in attempt (`e1866bf`, reverted `431200f`) failed
      because an elevated apex appeared but did not take foreground and the
      next keystroke dismissed it — the show-then-dismiss race since fixed by
      the post-summon guard window and the dedicated hook thread. Reused the
      reverted de-elevation launcher and task code.

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

- **The acrylic backdrop is a snapshot**, captured and blurred at summon
  time, not a live effect: content that moves behind an open apex is not
  re-blurred. Imperceptible for a briefly-shown launcher.
- **The index refreshes on start and on `Apex: Reload`**, not live: an app
  installed while apex is running appears after either.
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
