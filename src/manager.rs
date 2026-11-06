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

/// Manager pages; `arg()` is what the tray passes as `--page`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Profiles,
    Mcp,
    Settings,
}

impl Page {
    pub fn label(self) -> &'static str {
        match self {
            Page::Profiles => "Profiles",
            Page::Mcp => "MCP servers",
            Page::Settings => "Settings",
        }
    }
    pub fn arg(self) -> &'static str {
        match self {
            Page::Profiles => "profiles",
            Page::Mcp => "mcp",
            Page::Settings => "settings",
        }
    }
    pub fn parse(s: &str) -> Option<Page> {
        [Page::Profiles, Page::Mcp, Page::Settings].into_iter().find(|p| p.arg() == s)
    }
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
    fn page_args_round_trip() {
        for p in [Page::Profiles, Page::Mcp, Page::Settings] {
            assert_eq!(Page::parse(p.arg()), Some(p));
        }
        assert_eq!(Page::parse("nope"), None);
    }
}
