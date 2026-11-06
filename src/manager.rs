//! Data layer for the manager window: the operations the UI calls, with no UI code in here.
//!
//! The manager runs as its own process, so it reaches the widget through OS primitives only:
//! a named event to ask the widget to refresh, and `usage-cache.json`, which the widget writes
//! after each poll so the manager can show the same readings.
use crate::{config, mcp, profile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};

/// Named event the widget waits on; the manager signals it after changing anything.
pub fn refresh_event_name() -> PCWSTR {
    if cfg!(test) {
        w!("Local\\ClaudeUsageWidgetRefreshTest")
    } else {
        w!("Local\\ClaudeUsageWidgetRefresh")
    }
}

/// Asks the running widget (if any) to re-read its config and poll again.
pub fn refresh_widget() {
    unsafe {
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, refresh_event_name()) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
        }
    }
}

/// Named event the widget waits on to show itself (raise the window and flash it briefly).
pub fn show_event_name() -> PCWSTR {
    if cfg!(test) {
        w!("Local\\ClaudeUsageWidgetShowTest")
    } else {
        w!("Local\\ClaudeUsageWidgetShow")
    }
}

/// Asks the running widget to show itself. Returns false when no widget is listening, so the
/// caller can start one.
pub fn show_widget() -> bool {
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, show_event_name()) {
            Ok(h) => {
                let _ = SetEvent(h);
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Manager window theme. `System` follows Windows' app mode and is the default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    pub fn arg(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }
    /// Unknown or missing values fall back to `System`.
    pub fn parse(s: &str) -> Theme {
        [Theme::Light, Theme::Dark].into_iter().find(|t| t.arg() == s).unwrap_or(Theme::System)
    }
    /// Whether the window should be dark, given what Windows currently prefers.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Theme::System => system_dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }
}

/// Saves the theme choice (fresh config, validated like every other save).
pub fn save_theme(theme: Theme) -> Result<(), String> {
    let mut cfg = config::load();
    cfg.theme = theme.arg().to_string();
    config::save(&config::normalize(cfg)?)?;
    Ok(())
}

/// Ring colour by percent used: red above 90%, orange above 60%, otherwise green.
/// The widget and the manager both use this, so the thresholds live in one place.
pub fn level_rgb(used: f32) -> (f32, f32, f32) {
    if used > 90.0 {
        (0.92, 0.33, 0.33)
    } else if used > 60.0 {
        (0.98, 0.58, 0.16)
    } else {
        (0.30, 0.78, 0.45)
    }
}

/// Manager pages; `arg()` is what the tray passes as `--page`. `Home` is the dashboard and the
/// page shown when none is asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Home,
    Profiles,
    Mcp,
    Settings,
}

impl Page {
    pub const ALL: [Page; 4] = [Page::Home, Page::Profiles, Page::Mcp, Page::Settings];

    pub fn label(self) -> &'static str {
        match self {
            Page::Home => "Home",
            Page::Profiles => "Profiles",
            Page::Mcp => "MCP servers",
            Page::Settings => "Settings",
        }
    }
    pub fn arg(self) -> &'static str {
        match self {
            Page::Home => "home",
            Page::Profiles => "profiles",
            Page::Mcp => "mcp",
            Page::Settings => "settings",
        }
    }
    pub fn parse(s: &str) -> Option<Page> {
        Page::ALL.into_iter().find(|p| p.arg() == s)
    }
    /// A missing or unknown page means the dashboard.
    pub fn parse_or_home(s: Option<&str>) -> Page {
        s.and_then(Page::parse).unwrap_or(Page::Home)
    }
}

/// A calendar date: year, month (1-12), day.
pub type Ymd = (i32, u32, u32);

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil((y, m, d): Ymd) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Turns the widget's reset text ("Nov 12") into the next such date on or after `today`
/// (a past month/day means next year) and the number of days until it.
pub fn upcoming_reset(text: &str, today: Ymd) -> Option<(Ymd, i64)> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = text.split_whitespace();
    let month = parts.next()?;
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month))? as u32 + 1;
    let day: u32 = parts.next()?.trim_end_matches(',').parse().ok()?;
    if parts.next().is_some() || day == 0 || day > 31 {
        return None;
    }
    let now = days_from_civil(today);
    let mut date = (today.0, month, day);
    if days_from_civil(date) < now {
        date.0 += 1;
    }
    Some((date, days_from_civil(date) - now))
}

/// Each account's share (0..=1) of the combined weekly usage. Accounts without a reading get 0;
/// when nobody has used anything every share is 0 rather than a made-up even split.
pub fn usage_shares(weekly: &[Option<f32>]) -> Vec<f32> {
    let total: f32 = weekly.iter().flatten().map(|u| u.max(0.0)).sum();
    weekly
        .iter()
        .map(|u| if total > 0.0 { u.unwrap_or(0.0).max(0.0) / total } else { 0.0 })
        .collect()
}

/// One profile's latest reading, as written by the widget.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct UsageEntry {
    pub name: String,
    pub session_used: Option<f32>,
    pub weekly_used: Option<f32>,
    pub weekly_reset: Option<String>,
    pub error: Option<String>,
}

pub fn usage_cache_path() -> PathBuf {
    config::config_path().with_file_name("usage-cache.json")
}

/// Written by the widget after each poll (temp file + rename, so a reader never sees half a file).
pub fn write_usage(entries: &[UsageEntry]) -> Result<(), String> {
    let path = usage_cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string(entries).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Missing or unreadable cache means "no readings": rings stay empty, nothing is invented.
pub fn read_usage() -> Vec<UsageEntry> {
    std::fs::read_to_string(usage_cache_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub struct ProfileCard {
    pub name: String,
    pub dir: String,
    /// Shared MCP servers switched on in this profile's `.claude.json`.
    pub mcps: Vec<String>,
    /// The profile's `.claude.json` could not be read.
    pub error: Option<String>,
    pub session_used: Option<f32>,
    pub weekly_used: Option<f32>,
    pub weekly_reset: Option<String>,
}

pub struct ManagerState {
    pub servers: Vec<mcp::Server>,
    pub profiles: Vec<ProfileCard>,
}

fn profile_dirs() -> Vec<(String, PathBuf)> {
    config::load()
        .profiles
        .iter()
        .map(|p| (p.name.clone(), config::resolve_dir(p)))
        .collect()
}

fn dir_of(profile: &str) -> Result<PathBuf, String> {
    profile_dirs()
        .into_iter()
        .find(|(n, _)| n == profile)
        .map(|(_, d)| d)
        .ok_or_else(|| "unknown profile".to_string())
}

/// Everything the pages show, read fresh from disk.
pub fn snapshot() -> ManagerState {
    let usage = read_usage();
    let profiles = profile_dirs()
        .into_iter()
        .map(|(name, dir)| {
            let (mcps, error) = match mcp::enabled(&dir) {
                Ok(on) => (on, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            let reading = usage.iter().find(|u| u.name == name);
            ProfileCard {
                dir: dir.display().to_string(),
                mcps,
                error,
                session_used: reading.and_then(|u| u.session_used),
                weekly_used: reading.and_then(|u| u.weekly_used),
                weekly_reset: reading.and_then(|u| u.weekly_reset.clone()),
                name,
            }
        })
        .collect();
    ManagerState { servers: mcp::list(), profiles }
}

/// Creates a server, or edits `original`. Editing keeps the entry's `extra` keys (such as
/// `tools`), which the form cannot show.
pub fn save_server(original: Option<String>, mut server: mcp::Server) -> Result<(), String> {
    if let Some(orig) = &original {
        if let Some(old) = mcp::list().into_iter().find(|s| &s.name == orig) {
            server.extra = old.extra;
        }
    }
    let dirs: Vec<PathBuf> = profile_dirs().into_iter().map(|(_, d)| d).collect();
    mcp::upsert(original.as_deref(), server, &dirs)
}

pub fn delete_server(name: &str) -> Result<(), String> {
    let dirs: Vec<PathBuf> = profile_dirs().into_iter().map(|(_, d)| d).collect();
    mcp::delete(name, &dirs)
}

pub fn toggle(profile: &str, server: &str, on: bool) -> Result<(), String> {
    mcp::set_enabled(&dir_of(profile)?, server, on)
}

pub fn import_from(profile: &str) -> Result<Vec<String>, String> {
    mcp::import(&dir_of(profile)?)
}

pub fn new_profile(name: &str, servers: &[String]) -> Result<profile::Created, String> {
    let made = profile::create(name.trim(), servers)?;
    refresh_widget();
    Ok(made)
}

/// Replaces the profile called `original`. The config is reloaded first, so changes made since
/// the page was opened survive.
/// "Add existing…": shows an existing config folder in the widget without creating anything.
pub fn add_profile(name: &str, command: Option<String>, config_dir: Option<String>) -> Result<(), String> {
    let mut cfg = config::load();
    cfg.profiles.push(config::ProfileConfig { name: name.to_string(), command, config_dir });
    config::save(&config::normalize(cfg)?)?;
    refresh_widget();
    Ok(())
}

pub fn edit_profile(
    original: &str,
    name: &str,
    command: Option<String>,
    config_dir: Option<String>,
) -> Result<(), String> {
    let mut cfg = config::load();
    let slot = cfg
        .profiles
        .iter_mut()
        .find(|p| p.name == original)
        .ok_or_else(|| "unknown profile".to_string())?;
    *slot = config::ProfileConfig { name: name.to_string(), command, config_dir };
    config::save(&config::normalize(cfg)?)?;
    refresh_widget();
    Ok(())
}

/// Hides a profile in the widget. Never touches the profile's files.
pub fn remove_profile(name: &str) -> Result<(), String> {
    let mut cfg = config::load();
    let before = cfg.profiles.len();
    cfg.profiles.retain(|p| p.name != name);
    if cfg.profiles.len() == before {
        return Err("unknown profile".into());
    }
    config::save(&config::normalize(cfg)?)?;
    refresh_widget();
    Ok(())
}

pub fn save_settings(seconds: u64) -> Result<(), String> {
    let mut cfg = config::load();
    cfg.refresh_seconds = seconds;
    config::save(&config::normalize(cfg)?)?;
    refresh_widget();
    Ok(())
}

/// Splits on line breaks, trims, drops blanks.
pub fn parse_lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// One `key=value` per line. An empty value is allowed; a line without `=` is an error.
pub fn parse_kv(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for line in parse_lines(text) {
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("'{line}' is not name=value"))?;
        out.insert(k.trim().to_string(), v.trim().to_string());
    }
    Ok(out)
}

pub fn build_server(
    name: &str,
    kind: &str,
    command: &str,
    args_text: &str,
    env_text: &str,
    url: &str,
    headers_text: &str,
) -> Result<mcp::Server, String> {
    let stdio = kind == "stdio";
    Ok(mcp::Server {
        name: name.trim().to_string(),
        kind: kind.to_string(),
        command: if stdio { command.trim().to_string() } else { String::new() },
        args: if stdio { parse_lines(args_text) } else { Vec::new() },
        env: if stdio { parse_kv(env_text)? } else { BTreeMap::new() },
        url: if stdio { String::new() } else { url.trim().to_string() },
        headers: if stdio { BTreeMap::new() } else { parse_kv(headers_text)? },
        extra: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    struct Env {
        root: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Env {
        fn new(tag: &str) -> Env {
            let guard = mcp::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let root = std::env::temp_dir().join(format!("manager-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            std::env::set_var("APPDATA", root.join("appdata"));
            Env { root, _guard: guard }
        }

        fn dir(&self, name: &str) -> PathBuf {
            let d = self.root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn config(&self, refresh: u64, profiles: &[(&str, Option<PathBuf>)]) {
            let cfg = config::Config {
                refresh_seconds: refresh,
                theme: "system".into(),
                profiles: profiles
                    .iter()
                    .map(|(n, d)| config::ProfileConfig {
                        name: n.to_string(),
                        command: None,
                        config_dir: d.as_ref().map(|d| d.display().to_string()),
                    })
                    .collect(),
            };
            config::save(&cfg).unwrap();
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn stdio(name: &str) -> mcp::Server {
        build_server(name, "stdio", "npx", "-y", "", "", "").unwrap()
    }

    #[test]
    fn edit_profile_round_trip() {
        let env = Env::new("edit");
        env.config(900, &[("a", None), ("b", None)]);
        edit_profile("a", "a2", None, Some("D:\\claude-a".into())).unwrap();
        let cfg = config::load();
        assert_eq!(cfg.profiles[0].name, "a2");
        assert_eq!(cfg.profiles[0].config_dir.as_deref(), Some("D:\\claude-a"));
        assert_eq!(cfg.profiles[1].name, "b");
        let e = edit_profile("a2", "b", None, None).unwrap_err();
        assert!(e.contains("appears twice"), "{e}");
    }

    #[test]
    fn remove_profile_keeps_files_and_defaults() {
        let env = Env::new("remove");
        let dir = env.dir("pa");
        std::fs::write(dir.join(".claude.json"), "{}").unwrap();
        env.config(900, &[("a", Some(dir.clone())), ("b", None)]);
        remove_profile("a").unwrap();
        assert_eq!(config::load().profiles.len(), 1);
        assert!(dir.join(".claude.json").is_file());
        assert_eq!(remove_profile("nope").unwrap_err(), "unknown profile");
    }

    #[test]
    fn add_profile_appends_and_validates() {
        let env = Env::new("addprofile");
        env.config(900, &[("a", None)]);
        add_profile("b", None, Some("D:\\claude-b".into())).unwrap();
        let cfg = config::load();
        assert_eq!(cfg.profiles.len(), 2);
        assert_eq!(cfg.profiles[1].config_dir.as_deref(), Some("D:\\claude-b"));
        let e = add_profile("a", None, None).unwrap_err();
        assert!(e.contains("appears twice"), "{e}");
        assert_eq!(config::load().profiles.len(), 2, "nothing written on error");
    }

    #[test]
    fn edit_unknown_profile_errors() {
        let env = Env::new("edit-unknown");
        env.config(900, &[("a", None)]);
        assert_eq!(edit_profile("zzz", "x", None, None).unwrap_err(), "unknown profile");
    }

    #[test]
    fn save_settings_signals_widget_and_validates() {
        let env = Env::new("settings");
        env.config(900, &[("a", None)]);
        let event = unsafe { CreateEventW(None, false, false, refresh_event_name()).unwrap() };
        save_settings(17).unwrap();
        let text = std::fs::read_to_string(config::config_path()).unwrap();
        assert!(text.contains("\"refresh_seconds\": 17"), "{text}");
        assert_eq!(unsafe { WaitForSingleObject(event, 1000) }, WAIT_OBJECT_0);
        let e = save_settings(5).unwrap_err();
        assert!(e.contains("15"), "{e}");
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        unsafe {
            let _ = CloseHandle(event);
        }
    }

    #[test]
    fn show_widget_reports_whether_a_widget_listens() {
        let _env = Env::new("showwidget");
        assert!(!show_widget(), "no widget running: caller must start one");
        let event = unsafe { CreateEventW(None, false, false, show_event_name()).unwrap() };
        assert!(show_widget());
        assert_eq!(unsafe { WaitForSingleObject(event, 1000) }, WAIT_OBJECT_0);
        unsafe {
            let _ = CloseHandle(event);
        }
        assert!(!show_widget(), "event gone again");
    }

    #[test]
    fn toggle_import_delete_fresh_state() {
        let env = Env::new("toggle");
        let dir = env.dir("pa");
        std::fs::write(dir.join(".claude.json"), "{}").unwrap();
        env.config(900, &[("a", Some(dir.clone()))]);
        save_server(None, stdio("s")).unwrap();
        let snap = snapshot();
        assert_eq!(snap.servers.len(), 1);
        assert!(snap.profiles[0].mcps.is_empty());
        toggle("a", "s", true).unwrap();
        assert_eq!(snapshot().profiles[0].mcps, vec!["s".to_string()]);
        assert_eq!(toggle("zzz", "s", true).unwrap_err(), "unknown profile");
        delete_server("s").unwrap();
        let after = snapshot();
        assert!(after.servers.is_empty());
        assert!(after.profiles[0].mcps.is_empty());
    }

    #[test]
    fn edit_preserves_extra_keys() {
        let env = Env::new("extra");
        env.config(900, &[]);
        let mut s = stdio("Bravo - mssql-mcp");
        s.extra.insert("tools".into(), serde_json::json!(["a", "b"]));
        save_server(None, s).unwrap();
        let edited = build_server("Bravo - mssql-mcp", "stdio", "dotnet", "x", "K=v", "", "").unwrap();
        save_server(Some("Bravo - mssql-mcp".into()), edited).unwrap();
        let list = mcp::list();
        assert_eq!(list[0].command, "dotnet");
        assert!(list[0].extra.contains_key("tools"));
    }

    #[test]
    fn parse_lines_and_kv() {
        assert_eq!(parse_lines("a\r\nb\n\nc "), vec!["a", "b", "c"]);
        let kv = parse_kv("a=1\nb=").unwrap();
        assert_eq!(kv["a"], "1");
        assert_eq!(kv["b"], "");
        assert!(parse_kv("a=1\nbad").is_err());
        assert_eq!(parse_kv("a=1=2").unwrap()["a"], "1=2");
    }

    #[test]
    fn snapshot_carries_widget_usage() {
        let env = Env::new("usage");
        env.config(900, &[("a", None), ("b", None)]);
        assert!(snapshot().profiles[0].session_used.is_none(), "no cache = no reading");
        write_usage(&[UsageEntry {
            name: "a".into(),
            session_used: Some(42.0),
            weekly_used: Some(10.0),
            weekly_reset: Some("Nov 9".into()),
            error: None,
        }])
        .unwrap();
        let snap = snapshot();
        assert_eq!(snap.profiles[0].session_used, Some(42.0));
        assert_eq!(snap.profiles[0].weekly_reset.as_deref(), Some("Nov 9"));
        assert_eq!(snap.profiles[1].session_used, None);
        std::fs::write(usage_cache_path(), "not json").unwrap();
        assert!(snapshot().profiles[0].session_used.is_none(), "corrupt cache = no reading");
    }

    #[test]
    fn new_profile_reports_errors_inline() {
        let _env = Env::new("newprofile");
        let e = new_profile("../evil", &[]).err().unwrap();
        assert!(e.contains("name may only use"), "{e}");
    }

    #[test]
    fn theme_defaults_to_system_and_round_trips() {
        let env = Env::new("theme");
        env.config(900, &[("a", None)]);
        assert_eq!(Theme::parse(&config::load().theme), Theme::System);
        for t in [Theme::Dark, Theme::Light, Theme::System] {
            save_theme(t).unwrap();
            assert_eq!(Theme::parse(&config::load().theme), t);
        }
        assert_eq!(config::load().profiles.len(), 1, "profiles untouched");
        // An old config file without the key reads as system; a bad value is rejected on save.
        let path = config::config_path();
        std::fs::write(&path, r#"{"refresh_seconds":900,"profiles":[{"name":"a"}]}"#).unwrap();
        assert_eq!(config::load().theme, "system");
        let mut bad = config::load();
        bad.theme = "purple".into();
        assert!(config::normalize(bad).err().unwrap().contains("theme"));
        assert_eq!(Theme::parse("purple"), Theme::System);
        assert!(Theme::System.is_dark(true) && !Theme::System.is_dark(false));
        assert!(Theme::Dark.is_dark(false) && !Theme::Light.is_dark(true));
    }

    #[test]
    fn level_thresholds() {
        assert_eq!(level_rgb(0.0), level_rgb(60.0));
        assert_ne!(level_rgb(60.0), level_rgb(60.1));
        assert_eq!(level_rgb(60.1), level_rgb(90.0));
        assert_ne!(level_rgb(90.0), level_rgb(90.1));
    }

    #[test]
    fn page_args_round_trip() {
        for p in Page::ALL {
            assert_eq!(Page::parse(p.arg()), Some(p));
        }
        assert_eq!(Page::parse("nope"), None);
    }

    #[test]
    fn no_page_means_the_dashboard() {
        assert_eq!(Page::parse_or_home(None), Page::Home);
        assert_eq!(Page::parse_or_home(Some("bogus")), Page::Home);
        assert_eq!(Page::parse_or_home(Some("mcp")), Page::Mcp);
    }

    #[test]
    fn reset_dates_roll_into_next_year_and_count_days() {
        let today = (2026, 11, 6);
        assert_eq!(upcoming_reset("Nov 12", today), Some(((2026, 11, 12), 6)));
        assert_eq!(upcoming_reset("Nov 6", today), Some(((2026, 11, 6), 0)), "today counts");
        assert_eq!(upcoming_reset("nov 7", today), Some(((2026, 11, 7), 1)), "case-insensitive");
        // A date already past this year means the next one: Jan 2 is 57 days after Nov 6.
        assert_eq!(upcoming_reset("Jan 2", today), Some(((2027, 1, 2), 57)));
        // Leap day arithmetic: Feb 28 -> Mar 1 spans the 29th in 2028.
        assert_eq!(upcoming_reset("Mar 1", (2028, 2, 28)), Some(((2028, 3, 1), 2)));
        for bad in ["", "Nov", "Foo 3", "Nov 0", "Nov 32", "Nov 3 4", "12 Nov"] {
            assert_eq!(upcoming_reset(bad, today), None, "{bad:?}");
        }
    }

    #[test]
    fn usage_shares_are_real_proportions() {
        let s = usage_shares(&[Some(30.0), Some(10.0), None, Some(60.0)]);
        assert!((s[0] - 0.3).abs() < 1e-6 && (s[1] - 0.1).abs() < 1e-6 && s[2] == 0.0 && (s[3] - 0.6).abs() < 1e-6);
        assert!((s.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert_eq!(usage_shares(&[Some(0.0), None]), vec![0.0, 0.0], "no usage: no invented split");
        assert!(usage_shares(&[]).is_empty());
    }
}
