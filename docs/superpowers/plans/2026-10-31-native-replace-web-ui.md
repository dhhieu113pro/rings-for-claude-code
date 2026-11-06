# Native Manager Window (replace web UI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the browser-hosted manager (`src/web.rs` + 5 HTML/CSS/JS assets) with one native WinUI 3 window built with `windows-reactor`, driven by direct Rust calls.

**Architecture:** New `src/manager.rs` holds a pure data layer (the Rust ports of `web.rs`'s API handlers, unit-tested) plus the reactor components (root window with a `SplitView` sidebar and the three pages). The widget's hand-written Win32 loop is untouched. **Process model (decided after the spike: `App::run_component` is one-shot per process):** the manager is a separate binary `claude-usage-manager.exe` (`src/bin/claude-usage-manager.rs`), launched by the tray with `--page <p>`; shared modules (`config`, `mcp`, `profile`, manager data layer) live in a new `src/lib.rs`. Widget -> manager data goes through `%APPDATA%\claude-usage-widget\usage-cache.json`; manager -> widget refresh goes through the named event `Local\ClaudeUsageWidgetRefresh`; a second launch navigates the running instance via a named mutex + event/file handoff. Wherever Tasks 1-6 below say `crate::REFRESH_TX`, `STATE`, `spawn_manager`, `MANAGER` or "own thread", read it as superseded by this model; Task 3 is rewritten accordingly. The updater (`update.rs`) must ship both executables (new Task 3b). UI code is verified by building on Windows/CI; UI behaviour is verified manually on the developer's Windows machine.

**Tech Stack:** Rust 2021, `windows-reactor = "0.100"`, `windows-canvas = "0.100"` (reactor feature, Task 4 signature rings), existing `windows = 0.62` (widget, D2D, folder picker), `serde_json` (already). No other new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-31-native-replace-web-ui-design.md` · UI/UX: `docs/superpowers/specs/2026-10-31-native-manager-ui-ux.md`

## Global Constraints

- `windows-reactor = "0.100"` is a preview crate, MSRV rustc >= 1.95; verify the exact MSRV when building and match the era of the reactor samples.
- Do not touch `windows = "0.62"` or `windows-numerics = "0.3"` (widget, D2D). Cargo keeps reactor's `0.100`-family crates alongside them.
- Windows App Runtime (framework-dependent) must be installed on the dev/test machine; Windows 10+.
- Tests run on the pipeline only: `windows-latest` (x64) and `windows-11-arm` (arm64). No local arm64 or non-Windows testing exists; `cargo check` off-Windows currently fails in the `windows-future` dependency (verified: `IMarshal`/`marshaler` not found in `windows_core::imp`) and is out of scope.
- Cross-stack interop between widget and manager threads is OS primitives only (the existing `REFRESH_TX` channel, std channels, thread posts) — never reactor or `0.62` crate types across threads.
- Every save reloads config fresh first (read-modify-write), like `web.rs::saveProfiles` / `save_config` do today.
- Match `main.rs` style: inline Win32 where needed, sparse comments, `let _ =` for ignored results, plain imperative commit messages ("Add …").
- UI wording matches the HTML pages being deleted (field labels, button names, error prefixes).

## Review Focus

- Tray click while the manager window is already open: must bring the window to front and navigate to the clicked page, never a second window or a second WinUI message pump (Task 3, manual-verify step 3).
- Manager init failure (Windows App Runtime missing, WinUI bootstrap error): one-time message box, widget and all other tray items keep working, tray manager items stay retryable (Task 3).
- Window closed then reopened: fresh re-read of config, no stale component state; manager thread exits when `run_component` returns and a later tray click starts a new one (Task 3, manual-verify steps 2 and 3).
- A profile whose `.claude.json` is unreadable: shown as an error card, its MCP checkboxes disabled, actions on it surface the error inline without touching any file (Task 1 test + Task 5 wiring).
- Corrupt/incomplete user input (empty name, bad refresh seconds, blank folder line): surfaces as inline error text from the same message `normalize`/`validate` produce, and nothing is written (Task 1 tests, Tasks 4–6 wiring).
- ARM64: every pipeline test task runs the matrix on `windows-latest` **and** `windows-11-arm`; a Release build on `aarch64-pc-windows-msvc` (pipeline) proves the arm64 ship path (Task 2, and Task 3+ tasks' build checks).
- A ring shows a real reading or an empty track — never a made-up percent; a profile with no reading still renders its full card with muted rings (Task 1 test + Task 4 wiring).

---

### Task 1: Pure data layer (`src/manager.rs`)

**Files:**
- Create: `src/lib.rs` (`pub mod config; pub mod mcp; pub mod profile; pub mod manager;`), `src/manager.rs`
- Modify: `src/main.rs` (drop `mod config/mcp/profile`, use them from the lib crate `claude_usage_widget`; `mod web` stays until Task 7), `Cargo.toml` (explicit lib + bins)
- Refresh/usage changes vs. the text below: `refresh_widget()` signals the named event `Local\ClaudeUsageWidgetRefresh` instead of `REFRESH_TX`; `snapshot()` reads usage from `usage-cache.json` (written by the widget after each poll) instead of `main.rs::STATE`; tests inject a temp `APPDATA` and write the cache file.

**Interfaces:**
- Consumes: `config::{load, save, normalize, resolve_dir, Config, ProfileConfig}`, `mcp::{list, import, enabled, set_enabled, upsert, delete, Server}`, `profile::create` (all existing, unmodified).
- Produces (used by Tasks 3–6 by exact name):
  - `pub struct ProfileCard { pub name: String, pub dir: String, pub mcps: Vec<String>, pub error: Option<String>, pub session_used: Option<f32>, pub weekly_used: Option<f32>, pub weekly_reset: Option<String> }` (dir from `config::resolve_dir`; the three usage fields come from the widget's live `STATE` mutex in `main.rs`, matched by profile name — `None` when the widget has no reading)
  - `pub struct ManagerState { pub servers: Vec<mcp::Server>, pub profiles: Vec<ProfileCard> }`
  - `pub fn snapshot() -> ManagerState` (port of `web.rs::profile_dirs` + `state`, fresh read each call)
  - `pub fn save_server(original: Option<String>, server: mcp::Server) -> Result<(), String>` (fresh dirs, `mcp::upsert`; when editing, carries over the existing entry's `extra` map untouched — `build_server` cannot recover keys like `tools`)
  - `pub fn delete_server(name: &str) -> Result<(), String>` (fresh dirs, `mcp::delete`)
  - `pub fn toggle(profile: &str, server: &str, on: bool) -> Result<(), String>` (fresh dirs, `mcp::set_enabled`, unknown profile → `Err("unknown profile")`)
  - `pub fn import_from(profile: &str) -> Result<Vec<String>, String>` (fresh dirs, `mcp::import`)
  - `pub fn new_profile(name: &str, servers: &[String]) -> Result<profile::Created, String>` (trims name, `profile::create`, then `refresh_widget()`)
  - `pub fn edit_profile(original: &str, name: &str, command: Option<String>, config_dir: Option<String>) -> Result<(), String>` — replaces the entry whose `name == original` in a freshly loaded config, then `config::save(&config::normalize(cfg)?)` and `refresh_widget()`; unknown profile → `Err("unknown profile")`
  - `pub fn remove_profile(name: &str) -> Result<(), String>` — deletes the entry from a fresh config, saves it via `normalize`, and calls `refresh_widget()` (never touches profile files); unknown → `Err("unknown profile")`
  - `pub fn save_settings(seconds: u64) -> Result<(), String>` — fresh config, set `refresh_seconds`, `normalize`, `save`, `refresh_widget()` (port of `web.rs::save_config`)
  - `pub fn refresh_widget()` — `if let Some(tx) = crate::REFRESH_TX.get() { let _ = tx.send(()); }`
  - `pub fn parse_lines(text: &str) -> Vec<String>` — split on `\n`/`\r\n`, trim, drop blanks (args/env keys/header rows)
  - `pub fn parse_kv(text: &str) -> Result<BTreeMap<String, String>, String>` — one `key=value` per line, both trimmed; an empty value is allowed (`Name=`); a line without `=` → `Err`
  - `pub fn build_server(name: &str, kind: &str, command: &str, args_text: &str, env_text: &str, url: &str, headers_text: &str) -> mcp::Server` — fills matching fields (`args = parse_lines(args_text)`, `env = parse_kv(env_text)?`, url/headers defaulted empty)
- [ ] **Step 1: Write failing tests** in `#[cfg(test)] mod tests` at the bottom of `src/manager.rs`, using the `mcp::TEST_LOCK` + temp-dir env pattern (`Env::new(tag)` idiom copied from `src/mcp.rs:264`):
  - `edit_profile_round_trip`: env with `APPDATA`; config with two profiles; `edit_profile("a", "a2", None, Some("D:\\claude-a"))` → reloaded config has profile `a2` with `config_dir` set, first entry preserved, and `normalize` used (renaming to an existing name → `Err` containing "appears twice").
  - `remove_profile_keeps_files_and_defaults`: config with two profiles; `remove_profile("a")` → config has one; the profile's directory file written in the test still exists; `remove_profile("nope")` → `Err("unknown profile")`.
  - `edit_unknown_profile_errors`: `Err("unknown profile")`.
  - `save_settings_wires_refresh_and_validates`: config on disk; send through `crate::REFRESH_TX` by installing one via `std::sync::mpsc::channel` + `OnceLock` set in the test (same global `main.rs` uses); `save_settings(17)` → persisted JSON contains `"refresh_seconds": 17`; `save_settings(5)` → `Err` mentioning "15"; a message arrives on the test receiver after the successful save.
  - `toggle_import_delete_fresh_state`: profile dir with a `.claude.json` written, plus `save_server(None, build_server("s","stdio","npx","-y","","",""))` → `snapshot()` shows the server and its enabled state; `toggle` on it flips `.claude.json`; `delete_server("s")` removes it everywhere.
  - `edit_preserves_extra_keys`: import a server with `extra` keys (use the `Bravo - mssql-mcp` JSON from `src/mcp.rs:455`), then `save_server(Some("Bravo - mssql-mcp"), build_server("Bravo - mssql-mcp", "stdio", "dotnet", "x", "K=v", "", ""))` → `mcp::list()` still returns the `tools` extra.
  - `parse_lines_and_kv`: `parse_lines("a\r\nb\n\nc ")` == ["a","b","c"]; `parse_kv("a=1\nb=")` ok with `b=>""`; `parse_kv("a=1\nbad")` → `Err`; no quoting support: `"a=1=2"` → key `a`, value `1=2`.
  - `snapshot_carries_widget_usage`: with a `main.rs::STATE` entry for profile `a` (session 42.0, no error) and no entry for profile `b`, `snapshot()`'s card for `a` has `session_used == Some(42.0)` and `b`'s has `None`; ring colours always come from `main.rs::level_color`, the manager never re-derives thresholds.
  - `new_profile_reports_errors_inline`: env configured like `src/profile.rs` tests; `new_profile("../evil", &[])` → `Err` mentioning name rules.
- [ ] **Step 2: Run to verify failure:** `cargo test manager::` → compile error / FAIL (functions undefined). Run on Windows or in CI (this repo cannot compile off-Windows — see Global Constraints).
- [ ] **Step 3: Implement the functions above.** No HTTP, no folder picker, no UI here. Ports of `web.rs:170-246` minus the JSON layer; `edit_profile`/`remove_profile` are new/config-only. Refresh via `refresh_widget()` exactly where `web.rs` sends `REFRESH_TX` today.
- [ ] **Step 4: Run to verify pass:** `cargo test manager::` → all pass.
- [ ] **Step 5: Commit and push**

```bash
git add src/manager.rs src/main.rs docs/superpowers
git commit -m "Add manager data layer: profile/server/settings operations"
git push
```

---

### Task 2: Pipeline test workflow (x64 + ARM64)

**Files:**
- Create: `.github/workflows/test.yml`

**Interfaces:**
- Produces: a `Test` workflow that every later build task can rely on as its green gate.

- [ ] **Step 1: Create the workflow.** Matrix: `{ arch: x64, runs-on: windows-latest }`, `{ arch: win-arm64, runs-on: windows-11-arm }`. Steps: checkout, `dtolnay/rust-toolchain@stable`, cache keyed on arch, `cargo test --locked`, `cargo build --release --locked`. No secrets. If the repo cannot use the `windows-11-arm` hosted runner, fall back to cross-building `aarch64-pc-windows-msvc` on `windows-latest` (tests on that leg run x64 only) and record the limitation in the README commit.
- [ ] **Step 2: Verify:** push; both matrix legs go green (this is the ARM64 Windows test — pipeline only, per the spec's builder decision).
- [ ] **Step 3: Commit and push**

```bash
git add .github/workflows/test.yml
git commit -m "Add test pipeline for x64 and ARM64"
git push
```

---

### Task 3: Manager executable shell, tray launch, single instance

**Files:**
- Modify: `Cargo.toml` (`windows-reactor = "0.100"`; add `[[bin]]` entries: `claude-usage-widget` -> `src/main.rs`, `claude-usage-manager` -> `src/bin/claude-usage-manager.rs`; `windows` features `Win32_System_Threading`, `Win32_System_Com` as needed)
- Create: `src/bin/claude-usage-manager.rs` (reactor components, `ManagerRoot`, `Page`)
- Modify: `src/main.rs` (replace `open_page` and the `ID_PROFILES/ID_MCP/ID_CONFIG` arms with `launch_manager(page)`; add the refresh-event listener thread; write `usage-cache.json` after each poll)

**Interfaces:**
- `enum Page { Profiles, Mcp, Settings }`, `Page::label()` = "Profiles" / "MCP servers" / "Settings", `Page::arg()` = `profiles|mcp|settings`, `Page::parse(&str)`; shared via the lib so the widget uses the same `arg()`.
- Widget side: `fn launch_manager(page: Page)` spawns `<dir of current_exe>\claude-usage-manager.exe --page <arg>`; spawn failure or missing exe -> `MessageBoxW` "Cannot start the manager (claude-usage-manager.exe not found next to the widget)." and stay tray-only, retryable.
- Widget side: refresh listener thread waits on the named event `Local\ClaudeUsageWidgetRefresh` and does `REFRESH_TX.send(())`.
- Widget side: after every successful/failed poll, write `usage-cache.json` (per-profile `name`, `session_used`, `weekly_used`, `weekly_reset`, `error`) atomically (write temp + rename).
- Manager side: `main()` parses `--page`; takes named mutex `Local\ClaudeUsageManager`; if already held, writes the page to `manager-nav.txt` (APPDATA dir), sets the named event `Local\ClaudeUsageManagerNavigate`, focuses the existing window (`FindWindowW` by title) and exits 0. Otherwise runs `App::run_component::<ManagerRoot>(ManagerInput { first })` once; the page-request pump is `context.spawn_background` waiting on that event, reading `manager-nav.txt`, yielding `ManagerMsg::Widget(page)`. `run_component` returning means the process exits.
- Window title "Usage Rings for Claude Code – Manager". View: `SplitView` sidebar (Profiles, MCP servers, Settings) + page content, per the UI/UX spec.
- Init failure inside the manager process (runtime missing, WinUI error): `MessageBoxW` "Cannot open the manager window. The Windows App Runtime may be missing." and exit non-zero; the widget is unaffected.

- [ ] **Step 1: Cargo + lib split** (done in Task 1 for the lib; here add the reactor dep and bins).
- [ ] **Step 2: Write the shell** per the interfaces above; tray arms call `launch_manager(Page::…)`; delete `open_page`.
- [ ] **Step 3: Verify (Windows):** build green; each tray item opens the manager on that page; a second tray click while open switches page and focuses the same window (one process only); closing then clicking again starts a fresh process; killing/renaming the manager exe -> one message box, widget and other tray items unaffected; saving in the manager triggers a widget refresh within a second.
- [ ] **Step 4: Commit and push** (`git add Cargo.toml Cargo.lock src docs`; "Add native manager executable shell and tray launch").

---

### Task 3b: Updater ships both executables

**Files:**
- Modify: `src/update.rs`, `.github/workflows/release.yml`, `.github/workflows/test.yml` (if present from Task 2)

- [ ] **Step 1:** `release.yml` Package step copies both `claude-usage-widget.exe` and `claude-usage-manager.exe` into the zip folder.
- [ ] **Step 2:** `update.rs` extraction validates both exes exist in the package (error text names the missing one) and the swap script replaces both next to `current_exe()`. Keep the existing verify-then-swap behaviour; add a test for the "missing manager exe" case alongside the existing zip/sha tests.
- [ ] **Step 3:** `cargo test update::` green; commit "Ship and update the manager executable with the widget".

---

### Task 4: Profiles page

**Files:**
- Modify: `src/manager.rs` (`ProfilesPage` component + edit dialog + folder picker)

**Interfaces:**
- Consumes: Task 1 (`snapshot`, `new_profile`, `edit_profile`, `remove_profile`, `refresh_widget`), Task 3 (`ManagerMsg::Profiles`, `Page`), reactor sample `navigation` patterns (`View::component`, `context.callback`), the [UI/UX spec](../specs/2026-10-31-native-manager-ui-ux.md#screen-1--profiles).
- Native folder picker helper: `fn pick_folder() -> Option<String>` in `manager.rs` — `IFileOpenDialog` (`windows 0.62`, `Win32_UI_Shell` + `Win32_System_Com`): `CoCreateInstance` the dialog, `FOS_PICKFOLDERS`, return the first selected path; `None` on cancel/error. Runs on the manager (XAML/STA) thread; an already-initialized COM result (`RPC_E_CHANGED_MODE`) counts as success.
- Signature helper: `fn level_color(used: f32) -> (f32, f32, f32)` — proxy over `main.rs`'s existing `level_color` (single source of truth; publicise it or re-use it, never duplicate thresholds).
- Produces: `ProfilesMsg { Edit(String), Remove(String), PickedFolder(Option<String>), Commit { original: Option<String>, name: String, command: String, config_dir: String }, Create { name: String, servers: Vec<String> } }` — every data-changing page action goes through `ManagerMsg::Profiles`.

- [ ] **Step 1: Render the profiles list and buttons.** Per the UI/UX spec's Screen 1: a command bar ("Add existing…" secondary, "+ New profile" primary), profile cards with two `RingMeter` mini meters (session/week, empty track when `None`, reset date caption under the week ring when known), name/source/dir lines, MCP tag chips (or muted "no MCP servers"), "Edit"/"Remove" text buttons (Remove red), inline error line on the card. Four states: loading (skeleton rows), empty ("No profiles yet." + one "New profile" button), error (read-failure lines), full.
- [ ] **Step 2: Add the `RingMeter` component** using `windows-canvas = "0.100"` (feature `reactor`, host `canvas_invalidated` + `Invalidator`), track + arc from 12 o'clock with `level_color(clamp(used, 0..100))`, main.rs's exact geometry. If the canvas host fights the page layout or recovery, fall back to two thin rounded bars with the same ramp and record the decision in the design doc's Builder-check section.
- [ ] **Step 3: Wire actions** to Task 1 functions; every `Err` shows inline in the dialog/page area (the exact strings `config::normalize` / `profile::create` / `mcp::validate` produce — no `.expect`); Remove and profile-Create use `ContentDialog` confirmations per the spec; on success call `refresh_widget()` and re-`snapshot()`.
- [ ] **Step 4: Build and manual verify** (Windows) against the spec's Screen 1: New profile → created dir/launcher/notes printed multiline; a profile with no reading renders empty tracks (never a fake percent); Edit keeps other profiles untouched (pipeline `cargo test manager::` still green, including `snapshot_carries_widget_usage`); Remove confirms and leaves files intact; folder picker opens a standard dialog; focus visuals (Tab) reach every control and are visible.

```bash
git add src/manager.rs
git commit -m "Add native profiles manager page"
git push
```

---

### Task 5: MCP servers page and dialogs

**Files:**
- Modify: `src/manager.rs` (`McpPage` component + add/edit dialog + import/delete)

**Interfaces:**
- Consumes: Task 1 (`snapshot`, `save_server`, `delete_server`, `toggle`, `import_from`, `build_server`, `parse_lines`, `parse_kv`), Task 3 (`ManagerMsg::Mcp`), reactor `TextBox` + `ElementRef` pattern from the `navigation` sample.
- Produces: `McpMsg { Toggle { profile: String, server: String, on: bool }, Upsert { original: Option<String>, server: mcp::Server }, Delete(String), Import(String) }` — server objects always built via Task 1's `build_server` so parsing/`extra` handling stays single-sourced.

- [ ] **Step 1: Render the matrix of servers × profiles.** Per the UI/UX spec's Screen 2: a command bar ("from <profile>" dropdown + "Import" secondary, "+ Add server" primary; per-card "Edit"/"Delete" with Delete red), and one card per server from `snapshot().servers` (name, kind tag "stdio"/"http", summary = `command (+args joined " ")` or url, clipped with the full value in a tooltip), under it one checkbox per profile: checked iff the profile has no `error` and `p.mcps.contains(&s.name)`; disabled when the profile `error` is `Some`, with the error as the checkbox's tooltip; profile read errors render as warning cards below the list, as the old page does. Checkbox click → `McpMsg::Toggle` → Task 1's `toggle`. Four states like Screen 1: loading (skeleton rows), empty ("No MCP servers yet." + the one "+ Add server" action), error (warning cards), full.
- [ ] **Step 2: Add/Edit dialog.** Common fields: name, kind toggle (stdio/http) switching displayed field sets; stdio fields = command `TextBox`, args `TextBox` (re-parsed via `parse_lines`), "Name/Value" rows with remove buttons and a "+" (env map); http fields = url, header rows same way; footer Save/Cancel. Edit passes `original`; builds via Task 1's `build_server` (so `extra` survives, per Task 1's `save_server`); rows pre-filled with the exact strings (args joined with `\n`, kv rows joined `\n`).
- [ ] **Step 3: Import + Delete.** Profile dropdown listing `snapshot().profiles` names (defaults to the first), Import button → `import_from` → inline "Imported: a, b" (empty list → inline "Nothing to import"); Delete button per server → confirm dialog ("Delete "x" and remove it from every profile?" per the spec) → `delete_server`.
- [ ] **Step 4: Build and manual verify** on Windows: toggling persists into that profile's `.claude.json`; error-profile checkboxes are disabled; renamed server moves entries in enabled profiles (`rename` integration remains covered by pipeline tests); import adds and reports names; deleting removes everywhere.
- [ ] **Step 5: Commit and push**

```bash
git add src/manager.rs
git commit -m "Add native MCP servers manager page"
git push
```

---

### Task 6: Settings page

**Files:**
- Modify: `src/manager.rs` (`SettingsPage` component)

**Interfaces:**
- Consumes: Task 1 (`save_settings`), `config::load`.
- Produces: none (leaf task).

- [ ] **Step 1: Render and wire.** One numeric field "Refresh interval (seconds)" prefilled from `config::load().refresh_seconds` (fresh read like the old page's `GET /api/config`), Save button → parse the text as `u64` (parse failure → inline "Enter a number of seconds."), then `save_settings(n)` — its `Err` ("refresh must be at least 15 seconds" when < 15, otherwise `normalize`'s) shows inline. Success → brief inline "Saved." and the widget visibly refreshes (`REFRESH_TX`).
- [ ] **Step 2: Build, manual verify** on Windows: value < 15 shows inline error and persists nothing; 900 is saved and the rings re-fetch within a second.
- [ ] **Step 3: Commit and push**

```bash
git add src/manager.rs
git commit -m "Add native settings page"
git push
```

---

### Task 7: Delete the web UI, final README

**Files:**
- Delete: `src/web.rs`, `src/profiles_ui.html`, `src/mcp_ui.html`, `src/settings_ui.html`, `src/ui_common.css`, `src/ui_common.js`
- Modify: `src/main.rs` (remove `mod web;` and any `web::` references)
- Modify: `README.md`

**Interfaces:**
- Consumes: Tasks 3–6 (all tray paths already native). Nothing later consumes `web.rs`.

- [ ] **Step 1: Delete the six files** and their references: `mod web;` (`main.rs:8`) gone; `grep -rn 'web::' src/` → empty (`open_page` was already replaced in Task 3).
- [ ] **Step 2: README.** Replace the "tray menu opens a local manager in the browser" section with "the tray menu opens the native manager window"; drop the localhost port/token notes and the manual theme-switcher notes; add one Requirements line: "Windows App Runtime (WinUI 3) — see https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/" plus the note that the manager needs it once installed (widget itself keeps working without it).
- [ ] **Step 3: Verify:** `cargo build --release --locked` green (Windows), pipeline (Task 2) green on both `windows-latest` and `windows-11-arm` legs, and Release workflow untouched (all arm64 packaging already built there and still valid; no source changes beyond deletions).
- [ ] **Step 4: Commit and push**

```bash
git add -A
git commit -m "Replace browser manager with the native window"
git push
```

Then open the PR for review when the whole plan lands in one pushable state (squash merge per repo convention at the maintainer's discretion).
