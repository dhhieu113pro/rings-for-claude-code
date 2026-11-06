# Native manager window (replace web UI) - design

## Intent
The widget's manager (Profiles, MCP servers, Settings) currently opens as HTML pages in the browser via a localhost server (`src/web.rs` + 5 HTML/CSS/JS assets). Replace it with one native WinUI 3 window built with `windows-reactor`, so the app is fully native: the D2D rings widget stays as it is, and the manager becomes a Fluent window with a left sidebar.

## Goals
- One native window with a left sidebar (NavigationView-style, following the reactor `navigation` sample pattern): **Profiles · MCP servers · Settings**.
- Tray menu items that used to open a browser page now open this window on the matching page. Tray and widget behaviour otherwise unchanged.
- All current manager functionality is preserved, driven by direct Rust calls instead of HTTP:

  - **Profiles:** card list (name, resolver note `folder: …` / `command: …` / `default (~/.claude)`, resolved dir, MCP tags, per-profile error line). Buttons: Edit (name, launcher command, config folder text + native folder picker), Remove (confirm; only hides in widget), Add existing…, New profile (name + checkbox list of the shared MCP servers -> `profile::create`, shows created dir/launcher/notes multiline).
  - **MCP servers:** card list (name, type tag `stdio`/`http`, summary = command+args or URL, a checkbox per profile to toggle it on/off in that profile's `.claude.json` via `mcp::set_enabled`; disabled when the profile has an error). Add/Edit dialog: name; type switch showing stdio fields (command, args one per line, name/value env rows) or http fields (URL, name/value header rows); Edit keeps the `extra` map untouched. Import: profile dropdown + button -> `mcp::import`; Delete: confirm -> `mcp::delete` across all profiles.
  - **Settings:** refresh interval (seconds, min 15) -> `config::save(normalize(..))`, triggers a widget refresh like `save_config` does today (`REFRESH_TX`).
- Read-modify-write semantics are kept everywhere: before saving profiles or settings, reload the config fresh first (same pattern `web.rs::saveProfiles` / `save_config` use), so changes made through the other page survive.
- Errors surface inline on the page/dialog (like the old `banner`/`.warn` fields), not as crashes.

## Non-goals
- No change to the widget's look, rendering, polling, or tray icon drawing.
- No new config file format or storage location (`%APPDATA%\claude-usage-widget\*.json` unchanged).
- No self-contained runtime packaging work in this change (default: use the installed Windows App Runtime; `windows-reactor-setup` self-contained is a follow-up).
- No reimplementation of the update checker.

## Anti-goals
- The localhost server, its port/token security model, and all HTML/CSS/JS must not survive alongside the native UI; this is a replacement, not a second manager.
- A failed save must never corrupt `config.json`, `mcp.json` or a profile's `.claude.json` (upsert/delete semantics of `mcp.rs` already guard this; the native layer must not bypass it).

## Constraints
- `windows-reactor` 0.100 is a **preview** crate and requires **rustc >= 1.95** (MSRV); `windows-pickers` for the folder picker. Verify the exact crate versions' MSRV when building; pin what the windows-rs samples of that era use.
- Windows App Runtime must be installed on the dev/test machine (Windows 10+).
- The existing app loop is hand-written Win32 (widget + tray in one thread). Reactor's `App` owns its own loop/thread model — run the manager as a reactor app on its own thread so the widget loop is untouched; verify reactor supports launching `App` on a non-main thread from an already-running process (sample reference: `notifyicon`, `secondary-window`, `navigation`). If a shared loop is required instead, that refactor is part of this project and the plan must spell it out.
- **Spike result (2026-11-05, windows-reactor 0.100, rustc 1.99):** `App::run_component` opens a WinUI window fine on a plain secondary thread with no explicit bootstrap, but it is **one-shot per process**: after the window closes and `run_component` returns, a second call in the same process segfaults. So the in-process "restart the manager thread on each tray click" model is rejected. See "Process model" below.
- Message pump/coexistence: window classes, DPI awareness and `windows_subsystem` from the existing `main.rs` must remain valid; the reactor window must not inherit the widget's layered/click-through styles.
- Builds happen on Windows (MSVC); Rust code can be authored and syntax-checked elsewhere but not linked or run.

## Builder check - answered
- **Umbrella crate untouched:** `windows = "0.62"` (widget, D2D) stays as is. `windows-reactor`/`windows-pickers` ride the focused crates alongside it; cargo resolves both version families. Never pass cross-stack foreign types (HSTRING/HWND of the other family) between the widget and manager threads — interop is OS primitives only (existing `REFRESH_TX` channel, tray deltas). Builder call, follow-up if a conflict shows up.
- **Unpackaged init:** follow the reactor `framework_dependent` sample for using the installed runtime; use `windows-reactor-setup` for the bootstrap/stage step if the sample does. Builder verifies at build time.
- **Manager init failure** (runtime missing, WinUI error): one-time message box, then stay tray-only; widget and all other tray items keep working. Manager call. (User may veto.)
- **ARM64:** releases ship `win-arm64`; Windows App Runtime provides ARM64 installers for the versions these crates target. Builder verifies installer availability at build.
- **Non-Windows compile/test:** windows-rs crates normally compile off-Windows (bindings only); if `windows-reactor` does not, gate `mod manager` behind `#[cfg(windows)]`. Verify with `cargo check` on the dev machine where possible.

## Approaches
- **A. windows-reactor / WinUI 3 (chosen):** declarative Fluent UI, native look matching Windows 11, Elm-style components map cleanly onto the three pages and their dialogs. Costs: new dep on Windows App Runtime, rustc >= 1.95, preview API churn, learning curve.
- B. Plain Win32 controls on the existing hand-written loop: no new runtime and no MSRV bump, but every textbox/checkbox/dialog is manual code and the result looks dated. Rejected: most code for the worst look.
- C. egui (immediate mode): fastest to write, looks like a dev tool, another foreign UI stack in an already windows-rs project. Rejected.

## Theme
WinUI follows the system light/dark theme automatically. The web UI's per-page manual theme switcher (localStorage `theme`) has no equivalent need; it is dropped.

## Process model (decided after the spike)
The manager is a **separate executable**, `claude-usage-manager.exe`, shipped next to `claude-usage-widget.exe` in the same zip.
- **Crate layout:** one package, a small `src/lib.rs` exposing the shared `config`, `mcp`, `profile` modules and the manager data layer; two binaries: `src/main.rs` (widget, unchanged Win32/D2D) and `src/bin/claude-usage-manager.rs` (reactor UI). The widget exe does not link reactor.
- **Launch:** the tray runs `claude-usage-manager.exe --page profiles|mcp|settings`, located next to `current_exe()`. Missing exe or spawn failure: one message box, tray stays working, retryable.
- **Single instance:** manager takes a named mutex. A second launch writes the requested page into a named event/file handoff, signals the running instance (which navigates) and brings its window to the front, then exits. Window closed = process exits; next click starts fresh (config re-read).
- **Manager -> widget refresh:** after a save the manager signals a named event (`Local\ClaudeUsageWidgetRefresh`); the widget has a small thread that waits on it and sends on its existing `REFRESH_TX`. No port, no token, no HTTP.
- **Widget -> manager usage data (rings in the manager):** the widget writes its latest per-profile readings to `%APPDATA%\claude-usage-widget\usage-cache.json` after every poll; the manager reads it on open. No reading or unreadable cache = empty track, never a made-up percent.
- **Updater:** `update.rs` must now replace both executables (the zip contains both; the swap script copies both). A zip missing the manager exe is treated as invalid.
- **Crash isolation:** a WinUI failure can only take down the manager process, never the widget.

## Decided structure
- New `src/manager.rs` (component root + three page components + dialogs), reusing `config.rs`, `mcp.rs`, `profile.rs` directly. The data-shaping helpers from `web.rs` (`profile_dirs`, `state`, error strings) move there in Rust form.
- Delete: `src/web.rs`, `profiles_ui.html`, `mcp_ui.html`, `settings_ui.html`, `ui_common.css`, `ui_common.js`, `mod web` and all browser-open code in `main.rs`.
- Tray entries keep their labels; clicking opens the window (creating it on first use) on the given page; window close hides/destroys standardly, reopening shows a fresh view (config is re-read each open, as the web pages reloaded on every load).
- `README.md`: "The tray menu opens a local manager in the browser" section becomes "the tray menu opens the native manager window"; drop the token/port notes; drop theme-switcher mentions.

## Left to the builder
- Exact reactor component/layout code, dialog layout, spacing, and wording; match wording from the HTML pages.
- Folder picker: prefer the `windows-pickers` crate; if it drags in the runtime setup separately, fall back to a `Win32_UI_Shell` folder dialog (the crate is already a dependency).
- Cargo edits: add `windows-reactor` (and `windows-pickers` if used) with versions matching the repo samples; keep `edition`/`profile.release` as-is.
- The text-area-per-line args parsing and kv-row add/remove interactions.
