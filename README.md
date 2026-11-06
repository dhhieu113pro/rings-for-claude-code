# Usage Rings for Claude Code

A small, transparent Windows desktop widget that shows Claude Code usage for several profiles at once.

> Unofficial. Not affiliated with, endorsed by or sponsored by Anthropic. "Claude" is a trademark of Anthropic.

Built in Rust with [windows-rs](https://github.com/microsoft/windows-rs) (Direct2D + DirectWrite).

Each profile is drawn as a double ring:

- **Inner ring / centre number** – 5-hour session usage (% used)
- **Outer ring** – 7-day weekly usage (% used), shown as a small pill on the ring
- **Text under the name** – the local date the weekly window resets, e.g. `Resets Oct 29`

Ring colour is based on percent used, per ring:

| Used | Colour |
|---|---|
| up to 60% | green |
| above 60% up to 90% | orange |
| above 90% | red |

## Features

- Any number of profiles, laid out in a compact grid of 2 rows
- Transparent, always-on-top and click-through: mouse clicks go to the apps underneath
- Tray icon menu: **Refresh now**, **Unlock to move** (drag the widget), **Dashboard** (opens the manager), **Run at Windows startup**, **Quit**. Only one widget and one manager run at a time; starting a second widget just flashes the rings of the running one
- Per-monitor DPI aware; starts bottom-right of the work area
- Draws its own tray icon at startup (dark disc with an orange and green double ring); if it is hidden, drag it out of the tray overflow
- Refreshes every 15 minutes by default; the old numbers stay visible while a refresh runs
- Config is reloaded on every refresh, so adding or removing profiles needs no restart
- Manage MCP servers once and switch them on or off per profile (see [MCP servers](#mcp-servers))

## How it works

Every profile is a Claude Code config directory (`CLAUDE_CONFIG_DIR`). For each one the widget runs
`claude -p "/usage"` (hidden window, 60 s timeout) with that variable set and parses the text:

```
Current session: 0% used · resets Oct 27, 2:20pm (Asia/Bangkok)
Current week (all models): 93% used · resets Oct 28, 9pm (Asia/Bangkok)
```

Claude Code handles authentication and token refresh itself, so the widget never touches
`.credentials.json`. `claude.exe` must be on `PATH`.

> The output format of `/usage` is not a stable interface; if a Claude Code update changes it,
> the profile shows `bad output` and `parse()` in `src/usage.rs` needs adjusting.

## Configuration

The tray menu's **Dashboard** opens the native manager window (`claude-usage-manager.exe`, next to the widget), centred on the screen, with four pages: **Home** (dashboard), **Profiles**, **MCP servers** and **Settings**. Starting the manager again while it is open just brings that window forward. The title bar has a **Show rings** button that raises the rings widget (and starts it if it is not running).

- **Home** shows the number of accounts, the highest weekly use, the next weekly reset, a donut of each account's share of the combined weekly usage, and the upcoming reset dates (plus any account whose usage could not be read).
- **Profiles** lists the profiles shown in the widget with their folder, launcher command and enabled MCP servers. Edit changes the name, launcher command or config folder; Remove only hides a profile in the widget (its files are kept); *Add existing…* shows an existing config folder; *New profile* creates one (see below).
- **Settings** sets the manager theme (System, Light or Dark; System follows the Windows app mode and is the default), the refresh interval, and checks for updates. The interval is saved when you leave the page or close the window. The menu button in the title bar collapses the sidebar to icons.

Saving applies immediately and triggers a refresh.

The settings are stored in `%APPDATA%\claude-usage-widget\config.json`, created on first run (you can still edit it by hand):

```json
{
  "refresh_seconds": 900,
  "theme": "system",
  "profiles": [
    { "name": "main", "command": "claude" }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | Label shown under the rings |
| `command` | Launcher command. Its `.cmd`/`.bat` on `PATH` is parsed for `CLAUDE_CONFIG_DIR`; without one, `~/.claude` is used |
| `config_dir` | Explicit config directory (`~` expanded). Takes priority over `command` |
| `theme` | Manager window theme: `system` (default), `light` or `dark` |
| `refresh_seconds` | Poll interval in seconds, default 900 (15 min), minimum 15. "Refresh now" and restarting the app each make an immediate request, so avoid spamming them |

Profile launchers look like this (`claude-work.cmd`):

```bat
set "CLAUDE_CONFIG_DIR=%USERPROFILE%\claude-profiles\claude-work"
claude.exe %*
```

## MCP servers

Tray menu → **MCP servers** (or the *MCP servers* page of the manager) lets you add, edit and delete MCP servers (`stdio` with args and env vars, or `http` with headers).

- The shared list is stored in `%APPDATA%\claude-usage-widget\mcp.json`.
- Each profile has a checkbox per server. It is **on** when the server is present in `mcpServers` of `<profile dir>\.claude.json`; toggling adds or removes the entry and leaves the rest of the file untouched.
- Editing or renaming a server updates it in every profile that has it on; deleting removes it from all of them.
- **Import** copies the servers already in a profile's `.claude.json` into the shared list (names already in the list are skipped). Keys the UI does not edit, such as `tools`, are kept; entries with a type other than `stdio`/`http` are skipped.
- **New profile** (on the Profiles page) does what `new-claude-profile.bat` does: it creates `%USERPROFILE%\claude-profiles\<name>` with a default `settings.json`, writes the launcher `%USERPROFILE%\.local\bin\<name>.cmd` (sets `CLAUDE_CONFIG_DIR`, runs `claude.exe`), adds the profile to the widget, and turns on the shared MCP servers you tick. Existing folders, `settings.json` and launchers are kept. `claude.exe` is looked up in `CLAUDE_EXE`, `%USERPROFILE%\.local\bin`, then `PATH`; `CLAUDE_PROFILE_ROOT` and `CLAUDE_BIN_DIR` override the folders. Servers that run through `dnx` are plain `stdio` servers with command `dnx`.
- Env values are stored in plain text in both files, as Claude Code does itself. Restart running Claude sessions to pick up changes.

## Build and run

Requires the Rust MSVC toolchain. The manager window is a WinUI 3 app, so running it needs the [Windows App Runtime](https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/) (the widget itself works without it).

```powershell
cargo build --release
.\target\release\claude-usage-widget.exe
```

Quit the widget from the tray before rebuilding, otherwise the exe is locked.

## Start with Windows

Tray menu → **Run at Windows startup** (checked when enabled). It adds or removes a `ClaudeUsageWidget` entry under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, pointing at the current exe path.

## Updating

Manager → **Settings → Check for updates**. If a newer GitHub release exists, you are asked to confirm; the app then downloads the zip for your architecture, verifies it against the release's `.sha256` file, closes, replaces both exes (widget and manager) and starts the new version. If the download or checksum fails, nothing is changed. It uses the `curl`, `tar` and PowerShell that ship with Windows 10/11.

## Troubleshooting

| Label | Meaning |
|---|---|
| `no claude` | `claude.exe` could not be started (not on `PATH`) |
| `token expired` | The output mentions an expired login; run `/login` on that profile |
| `bad output` | `claude -p "/usage"` printed something the parser does not recognise (logged out, or format changed) |
| `timeout` | No result within 60 s |

## Project layout

- `src/main.rs` – widget: window, layered rendering, tray menu, polling thread
- `src/bin/claude-usage-manager.rs` – native manager window (Profiles, MCP servers, Settings pages)
- `src/manager.rs` – the manager's data layer (shared by the UI and its tests), widget hand-off via named event and `usage-cache.json`
- `src/lib.rs` – modules shared by both executables
- `src/config.rs` – config file, profile directory resolution
- `src/update.rs` – check for updates: GitHub release lookup, SHA-256 verify, replace helper
- `src/usage.rs` – runs `claude -p "/usage"` and parses it
- `src/profile.rs` – creates a profile (folder, launcher, widget entry)
- `src/mcp.rs` – central MCP list, sync into profile `.claude.json`
