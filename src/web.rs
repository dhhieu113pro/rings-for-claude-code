//! Tiny localhost HTTP server hosting the MCP manager UI.
//!
//! Anything that can reach this port can write commands into Claude configs, so every request
//! must carry the Host of the loopback address (blocks DNS rebinding) and a random token that
//! only the opened page knows (blocks cross-site requests).
use crate::{config, mcp, profile};
use serde_json::{json, Value};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

const PAGE_PROFILES: &str = include_str!("profiles_ui.html");
const PAGE_MCP: &str = include_str!("mcp_ui.html");
const PAGE_SETTINGS: &str = include_str!("settings_ui.html");
const UI_CSS: &str = include_str!("ui_common.css");
const UI_JS: &str = include_str!("ui_common.js");
const MAX_BODY: usize = 1 << 20;

struct Handle {
    port: u16,
    token: String,
}

static SERVER: OnceLock<Option<Handle>> = OnceLock::new();

fn random_token() -> String {
    (0..2)
        .map(|_| format!("{:016x}", RandomState::new().build_hasher().finish()))
        .collect()
}

/// Starts the server on first use and returns the URL of a page (`/profiles`, `/mcp`, `/settings`).
pub fn url(page: &str) -> Option<String> {
    let handle = SERVER.get_or_init(|| {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        let token = random_token();
        let t = token.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, port, &t);
            }
        });
        Some(Handle { port, token })
    });
    handle.as_ref().map(|h| format!("http://127.0.0.1:{}{page}?t={}", h.port, h.token))
}

struct Request {
    method: String,
    path: String,
    host: String,
    token: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let (mut host, mut token, mut len) = (String::new(), String::new(), 0usize);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.to_ascii_lowercase().as_str() {
            "host" => host = v.to_string(),
            "x-token" => token = v.to_string(),
            "content-length" => len = v.parse().ok()?,
            _ => {}
        }
    }
    if len > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some(Request { method, path, host, token, body })
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn json_reply(stream: &mut TcpStream, result: Result<Value, String>) -> std::io::Result<()> {
    match result {
        Ok(v) => respond(stream, "200 OK", "application/json", &v.to_string()),
        Err(e) => respond(stream, "400 Bad Request", "application/json", &json!({ "error": e }).to_string()),
    }
}

fn handle(mut stream: TcpStream, port: u16, token: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let Some(req) = read_request(&mut stream) else {
        return respond(&mut stream, "400 Bad Request", "text/plain", "bad request");
    };
    if req.host != format!("127.0.0.1:{port}") {
        return respond(&mut stream, "403 Forbidden", "text/plain", "forbidden");
    }
    let (route, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
    if req.method == "GET" {
        // Shared assets hold no data, so they need no token.
        match route {
            "/ui.css" => return respond(&mut stream, "200 OK", "text/css; charset=utf-8", UI_CSS),
            "/ui.js" => return respond(&mut stream, "200 OK", "text/javascript; charset=utf-8", UI_JS),
            _ => {}
        }
        let page = match route {
            "/" | "/profiles" => Some(PAGE_PROFILES),
            "/mcp" => Some(PAGE_MCP),
            "/settings" => Some(PAGE_SETTINGS),
            _ => None,
        };
        if let Some(page) = page {
            if query.split('&').any(|p| p == format!("t={token}")) {
                return respond(&mut stream, "200 OK", "text/html; charset=utf-8", page);
            }
            return respond(&mut stream, "403 Forbidden", "text/plain", "forbidden");
        }
    }
    if req.token != token {
        return respond(&mut stream, "403 Forbidden", "text/plain", "forbidden");
    }
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let result = match (req.method.as_str(), route) {
        ("GET", "/api/state") => Ok(state()),
        ("POST", "/api/server") => save_server(&body),
        ("POST", "/api/delete") => delete_server(&body),
        ("POST", "/api/toggle") => toggle(&body),
        ("POST", "/api/import") => import(&body),
        ("POST", "/api/profile") => new_profile(&body),
        ("GET", "/api/config") => Ok(json!(config::load())),
        ("POST", "/api/config") => save_config(&body),
        _ => return respond(&mut stream, "404 Not Found", "text/plain", "not found"),
    };
    json_reply(&mut stream, result)
}

fn profile_dirs() -> Vec<(String, PathBuf)> {
    config::load()
        .profiles
        .iter()
        .map(|p| (p.name.clone(), config::resolve_dir(p)))
        .collect()
}

fn state() -> Value {
    let profiles: Vec<Value> = profile_dirs()
        .into_iter()
        .map(|(name, dir)| match mcp::enabled(&dir) {
            Ok(on) => json!({ "name": name, "dir": dir.display().to_string(), "enabled": on }),
            Err(e) => json!({ "name": name, "dir": dir.display().to_string(), "enabled": [], "error": e }),
        })
        .collect();
    json!({ "servers": mcp::list(), "profiles": profiles })
}

fn save_server(body: &Value) -> Result<Value, String> {
    let server: mcp::Server = serde_json::from_value(body["server"].clone()).map_err(|e| e.to_string())?;
    let dirs: Vec<PathBuf> = profile_dirs().into_iter().map(|(_, d)| d).collect();
    mcp::upsert(body["original"].as_str(), server, &dirs)?;
    Ok(json!({}))
}

fn delete_server(body: &Value) -> Result<Value, String> {
    let name = body["name"].as_str().ok_or("name required")?;
    let dirs: Vec<PathBuf> = profile_dirs().into_iter().map(|(_, d)| d).collect();
    mcp::delete(name, &dirs)?;
    Ok(json!({}))
}

fn toggle(body: &Value) -> Result<Value, String> {
    let profile = body["profile"].as_str().ok_or("profile required")?;
    let server = body["server"].as_str().ok_or("server required")?;
    let on = body["on"].as_bool().ok_or("on required")?;
    let (_, dir) = profile_dirs()
        .into_iter()
        .find(|(n, _)| n == profile)
        .ok_or("unknown profile")?;
    mcp::set_enabled(&dir, server, on)?;
    Ok(json!({}))
}

fn import(body: &Value) -> Result<Value, String> {
    let profile = body["profile"].as_str().ok_or("profile required")?;
    let (_, dir) = profile_dirs()
        .into_iter()
        .find(|(n, _)| n == profile)
        .ok_or("unknown profile")?;
    Ok(json!({ "added": mcp::import(&dir)? }))
}
fn new_profile(body: &Value) -> Result<Value, String> {
    let name = body["name"].as_str().ok_or("name required")?.trim();
    let servers: Vec<String> = body["servers"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let made = profile::create(name, &servers)?;
    if let Some(tx) = crate::REFRESH_TX.get() {
        let _ = tx.send(());
    }
    Ok(json!({
        "dir": made.dir.display().to_string(),
        "launcher": made.launcher.display().to_string(),
        "notes": made.notes,
    }))
}
fn save_config(body: &Value) -> Result<Value, String> {
    let cfg: config::Config = serde_json::from_value(body.clone()).map_err(|e| e.to_string())?;
    config::save(&config::normalize(cfg)?)?;
    if let Some(tx) = crate::REFRESH_TX.get() {
        let _ = tx.send(());
    }
    Ok(json!({}))
}

/// Parity with the native manager: the same user actions through the web handlers and through
/// `manager::*` must leave identical state on disk. Removed together with this file in Task 7.
#[cfg(test)]
mod parity_tests {
    use super::*;
    use claude_usage_widget::manager;

    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("parity-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for p in ["p1", "p2"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
            std::fs::write(root.join(p).join(".claude.json"), "{}").unwrap();
        }
        std::env::set_var("APPDATA", root.join("appdata"));
        let cfg = config::Config {
            refresh_seconds: 900,
            profiles: ["p1", "p2"]
                .iter()
                .map(|n| config::ProfileConfig {
                    name: n.to_string(),
                    command: None,
                    config_dir: Some(root.join(n).display().to_string()),
                })
                .collect(),
        };
        config::save(&cfg).unwrap();
        root
    }

    /// Everything a user could observe afterwards, with the temp root stripped out.
    fn observe(root: &PathBuf) -> String {
        let mut out = serde_json::to_string(&mcp::list()).unwrap();
        for p in ["p1", "p2"] {
            out += &std::fs::read_to_string(root.join(p).join(".claude.json")).unwrap();
        }
        out.replace(&root.display().to_string().replace('\\', "\\\\"), "<root>")
    }

    #[test]
    fn manager_matches_web_handlers() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = setup("web");
        save_server(&json!({"original": null, "server": {"name": "s", "type": "stdio", "command": "npx",
            "args": ["-y", "@x/y"], "env": {"K": "v"}, "url": "", "headers": {}}})).unwrap();
        save_server(&json!({"original": null, "server": {"name": "h", "type": "http", "command": "",
            "args": [], "env": {}, "url": "https://x.test/mcp", "headers": {"A": "b"}}})).unwrap();
        for (p, s, on) in [("p1", "s", true), ("p2", "h", true), ("p1", "h", true), ("p1", "h", false)] {
            toggle(&json!({"profile": p, "server": s, "on": on})).unwrap();
        }
        save_server(&json!({"original": "s", "server": {"name": "s2", "type": "stdio", "command": "node",
            "args": ["-y", "@x/y"], "env": {"K": "v"}, "url": "", "headers": {}}})).unwrap();
        delete_server(&json!({"name": "h"})).unwrap();
        let web_state = observe(&root);
        let _ = std::fs::remove_dir_all(&root);

        let root = setup("native");
        manager::save_server(None, manager::build_server("s", "stdio", "npx", "-y\n@x/y", "K=v", "", "").unwrap()).unwrap();
        manager::save_server(None, manager::build_server("h", "http", "", "", "", "https://x.test/mcp", "A=b").unwrap()).unwrap();
        for (p, s, on) in [("p1", "s", true), ("p2", "h", true), ("p1", "h", true), ("p1", "h", false)] {
            manager::toggle(p, s, on).unwrap();
        }
        manager::save_server(Some("s".into()), manager::build_server("s2", "stdio", "node", "-y\n@x/y", "K=v", "", "").unwrap()).unwrap();
        manager::delete_server("h").unwrap();
        let native_state = observe(&root);
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(web_state, native_state);
        assert!(native_state.contains("s2") && !native_state.contains("\"h\""));
    }

    #[test]
    fn manager_errors_match_web_handlers() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = setup("errs");
        let web = toggle(&json!({"profile": "nope", "server": "s", "on": true})).unwrap_err();
        assert_eq!(manager::toggle("nope", "s", true).unwrap_err(), web);
        let web = save_server(&json!({"original": null, "server": {"name": "", "type": "stdio", "command": "x",
            "args": [], "env": {}, "url": "", "headers": {}}})).unwrap_err();
        let native = manager::save_server(None, manager::build_server("", "stdio", "x", "", "", "", "").unwrap()).unwrap_err();
        assert_eq!(web, native);
        let _ = std::fs::remove_dir_all(&root);
    }
}
