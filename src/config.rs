use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct ProfileConfig {
    pub name: String,
    /// Launcher command (e.g. `claude-work`); its `.cmd` is parsed for CLAUDE_CONFIG_DIR.
    #[serde(default)]
    pub command: Option<String>,
    /// Explicit config dir; takes priority over `command`. `~` is expanded.
    #[serde(default)]
    pub config_dir: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_refresh")]
    pub refresh_seconds: u64,
    /// Manager window theme: `system` (default), `light` or `dark`.
    #[serde(default = "default_theme")]
    pub theme: String,
    pub profiles: Vec<ProfileConfig>,
}

fn default_theme() -> String {
    "system".into()
}

fn default_refresh() -> u64 {
    900
}

impl Default for Config {
    fn default() -> Self {
        let p = |name: &str, command: Option<&str>, dir: Option<&str>| ProfileConfig {
            name: name.into(),
            command: command.map(Into::into),
            config_dir: dir.map(Into::into),
        };
        Config {
            refresh_seconds: default_refresh(),
            theme: default_theme(),
            profiles: vec![p("main", Some("claude"), None)],
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("claude-usage-widget").join("config.json")
}

/// Loads the config, writing a default one on first run.
pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => {
            let cfg = Config::default();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(text) = serde_json::to_string_pretty(&cfg) {
                let _ = std::fs::write(&path, text);
            }
            cfg
        }
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => home().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(path),
    }
}

fn dir_from_command(command: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for ext in ["cmd", "bat"] {
            let file = dir.join(format!("{command}.{ext}"));
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            for line in text.lines() {
                if let Some((_, rest)) = line.split_once("CLAUDE_CONFIG_DIR=") {
                    return Some(PathBuf::from(rest.trim().trim_end_matches('"')));
                }
            }
        }
    }
    None
}

pub fn resolve_dir(p: &ProfileConfig) -> PathBuf {
    if let Some(dir) = &p.config_dir {
        return expand(dir);
    }
    if let Some(dir) = p.command.as_deref().and_then(dir_from_command) {
        return dir;
    }
    home().join(".claude")
}

/// Trims fields, turns blank optional ones into `None` and checks the result is usable.
pub fn normalize(mut cfg: Config) -> Result<Config, String> {
    if cfg.refresh_seconds < 15 {
        return Err("refresh must be at least 15 seconds".into());
    }
    if !matches!(cfg.theme.as_str(), "system" | "light" | "dark") {
        return Err("theme must be system, light or dark".into());
    }
    let blank = |v: &mut Option<String>| {
        *v = v.take().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    };
    for (i, p) in cfg.profiles.iter_mut().enumerate() {
        p.name = p.name.trim().to_string();
        if p.name.is_empty() {
            return Err(format!("profile {} has no name", i + 1));
        }
        blank(&mut p.command);
        blank(&mut p.config_dir);
    }
    for (i, p) in cfg.profiles.iter().enumerate() {
        if cfg.profiles[..i].iter().any(|q| q.name == p.name) {
            return Err(format!("profile '{}' appears twice", p.name));
        }
    }
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, command: &str) -> ProfileConfig {
        ProfileConfig { name: name.into(), command: Some(command.into()), config_dir: Some("  ".into()) }
    }

    fn cfg(refresh: u64, profiles: Vec<ProfileConfig>) -> Config {
        Config { refresh_seconds: refresh, theme: default_theme(), profiles }
    }

    #[test]
    fn normalize_trims_and_blanks() {
        let out = normalize(cfg(900, vec![profile(" a ", " claude-a "), profile("b", "")])).unwrap();
        assert_eq!(out.profiles[0].name, "a");
        assert_eq!(out.profiles[0].command.as_deref(), Some("claude-a"));
        assert_eq!(out.profiles[0].config_dir, None);
        assert_eq!(out.profiles[1].command, None);
    }

    #[test]
    fn normalize_rejects_bad_config() {
        assert!(normalize(cfg(14, vec![])).is_err());
        assert!(normalize(cfg(900, vec![profile(" ", "x")])).is_err());
        assert!(normalize(cfg(900, vec![profile("a", "x"), profile("a", "y")])).is_err());
    }
}
