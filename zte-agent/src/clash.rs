//! Mihomo (Clash.Meta) process and configuration management.
//!
//! The core lives on the persistent data partition. OpenU60 owns its process,
//! validates every imported YAML with Mihomo itself, and keeps the controller
//! on loopback. LAN clients opt in through the mixed HTTP/SOCKS port; this
//! first release deliberately does not install transparent-proxy firewall
//! rules, so a bad proxy configuration cannot take the hotspot offline.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DIR: &str = "/data/open-u60/clash";
const BIN: &str = "/data/open-u60/clash/mihomo";
const CONFIG: &str = "/data/open-u60/clash/config.yaml";
const CONFIG_NEW: &str = "/data/open-u60/clash/config.yaml.new";
const STATE: &str = "/data/open-u60/clash/state.json";
const STATE_NEW: &str = "/data/open-u60/clash/state.json.new";
const LOG: &str = "/tmp/open-u60-clash.log";
const MAX_CONFIG: usize = 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
struct SavedState {
    enabled: bool,
}

struct Runtime {
    desired: bool,
    child: Option<Child>,
    last_error: Option<String>,
}

pub struct ClashManager {
    runtime: Mutex<Runtime>,
}

impl ClashManager {
    pub fn new() -> Self {
        let saved = fs::read(STATE)
            .ok()
            .and_then(|data| serde_json::from_slice::<SavedState>(&data).ok())
            .unwrap_or_default();
        Self {
            runtime: Mutex::new(Runtime {
                desired: saved.enabled,
                child: None,
                last_error: None,
            }),
        }
    }

    pub fn auto_start(&self) {
        let enabled = self.runtime.lock().unwrap_or_else(|e| e.into_inner()).desired;
        if enabled {
            if let Err(error) = self.start_process() {
                let mut state = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
                state.last_error = Some(error.clone());
                eprintln!("[clash] startup failed: {error}");
            }
        }
    }

    /// Mihomo resolves names through the system resolver. On this firmware the
    /// agent starts before the cellular DNS servers are installed, so restart
    /// an enabled process after WAN reconnect to refresh its resolver state.
    pub fn start_wan_watch(self: &Arc<Self>, events: mpsc::Receiver<Value>) {
        let manager = Arc::clone(self);
        crate::system::spawn_named("clash-wan", move || {
            for event in events {
                if manager.enabled() && wan_connected(&event) {
                    if let Err(error) = manager.start_process() {
                        let mut state = manager.runtime.lock().unwrap_or_else(|e| e.into_inner());
                        state.last_error = Some(error.clone());
                        eprintln!("[clash] WAN reconnect restart failed: {error}");
                    }
                }
            }
        });
    }

    fn enabled(&self) -> bool {
        self.runtime.lock().unwrap_or_else(|e| e.into_inner()).desired
    }

    fn installed() -> bool {
        Path::new(BIN).is_file()
    }

    fn configured() -> bool {
        Path::new(CONFIG).is_file()
    }

    fn persist_enabled(enabled: bool) -> Result<(), String> {
        fs::create_dir_all(DIR).map_err(|e| format!("create Clash directory: {e}"))?;
        let data = serde_json::to_vec(&SavedState { enabled }).map_err(|e| e.to_string())?;
        atomic_write(STATE_NEW, STATE, &data, 0o600)
    }

    fn validate_config(path: &str) -> Result<(), String> {
        if !Self::installed() {
            return Err("Mihomo is not installed".into());
        }
        let output = Command::new(BIN)
            .args(["-t", "-d", DIR, "-f", path])
            .output()
            .map_err(|e| format!("run Mihomo config check: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        Err(format!("invalid Mihomo configuration: {}", truncate(detail, 800)))
    }

    fn start_process(&self) -> Result<(), String> {
        if !Self::installed() {
            return Err("Mihomo is not installed".into());
        }
        if !Self::configured() {
            return Err("import a Clash configuration before enabling it".into());
        }
        Self::validate_config(CONFIG)?;
        {
            let mut state = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
            stop_child(state.child.take());
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(LOG)
            .map_err(|e| format!("open Clash log: {e}"))?;
        let err = log.try_clone().map_err(|e| format!("clone Clash log: {e}"))?;
        let child = Command::new(BIN)
            .args(["-d", DIR, "-f", CONFIG])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .spawn()
            .map_err(|e| format!("start Mihomo: {e}"))?;
        let mut state = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        state.child = Some(child);
        state.last_error = None;
        Ok(())
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        if enabled {
            self.start_process()?;
            if let Err(error) = Self::persist_enabled(true) {
                let mut state = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
                stop_child(state.child.take());
                return Err(error);
            }
        } else {
            let mut state = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
            stop_child(state.child.take());
            drop(state);
            Self::persist_enabled(false)?;
        }
        let mut state = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        state.desired = enabled;
        state.last_error = None;
        Ok(())
    }

    fn replace_config(&self, text: &str, enable: bool) -> Result<(), String> {
        if text.is_empty() || text.len() > MAX_CONFIG {
            return Err(format!("configuration must be 1-{MAX_CONFIG} bytes"));
        }
        validate_lan_policy(text)?;
        fs::create_dir_all(DIR).map_err(|e| format!("create Clash directory: {e}"))?;
        atomic_write(CONFIG_NEW, CONFIG_NEW, text.as_bytes(), 0o600)?;
        if let Err(error) = Self::validate_config(CONFIG_NEW) {
            let _ = fs::remove_file(CONFIG_NEW);
            return Err(error);
        }
        fs::rename(CONFIG_NEW, CONFIG).map_err(|e| format!("commit Clash configuration: {e}"))?;
        self.set_enabled(enable)
    }

    fn snapshot(&self) -> Value {
        let mut state = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        let process = state.child.as_mut().map(|child| child.try_wait());
        let running = match process {
            Some(Ok(None)) => true,
            Some(Ok(Some(status))) => {
                state.last_error = Some(format!("Mihomo exited with {status}"));
                false
            }
            Some(Err(error)) => {
                state.last_error = Some(format!("inspect Mihomo process: {error}"));
                false
            }
            None => false,
        };
        if !running {
            state.child = None;
        }
        let version = if Self::installed() {
            Command::new(BIN).arg("-v").output().ok().and_then(|o| {
                let text = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                (!text.is_empty()).then_some(text)
            })
        } else {
            None
        };
        json!({
            "installed": Self::installed(),
            "configured": Self::configured(),
            "enabled": state.desired,
            "running": running,
            "version": version,
            "mode": "explicit_proxy",
            "mixed_port": 7890,
            "proxy_address": "192.168.0.1:7890",
            "controller": "127.0.0.1:9091",
            "log": LOG,
            "last_error": state.last_error,
        })
    }
}

impl Drop for ClashManager {
    fn drop(&mut self) {
        if let Ok(state) = self.runtime.get_mut() {
            stop_child(state.child.take());
        }
    }
}

fn stop_child(child: Option<Child>) {
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn atomic_write(temp: &str, target: &str, data: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(mode)
        .open(temp)
        .map_err(|e| format!("open {temp}: {e}"))?;
    file.set_permissions(fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    file.write_all(data).and_then(|_| file.sync_all()).map_err(|e| format!("write {temp}: {e}"))?;
    if temp != target {
        fs::rename(temp, target).map_err(|e| format!("rename {temp}: {e}"))?;
    }
    Ok(())
}

/// Keep the first release explicit and recoverable: clients opt in to one
/// known LAN port, and the powerful controller may never be exposed to Wi-Fi.
fn validate_lan_policy(text: &str) -> Result<(), String> {
    let mut mixed = None;
    let mut allow_lan = None;
    let mut controller = None;
    for raw in text.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') || raw.trim_start().starts_with('#') {
            continue;
        }
        let line = raw.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(['\'', '"']);
        match key.trim() {
            "mixed-port" => mixed = value.parse::<u16>().ok(),
            "allow-lan" => allow_lan = Some(value.eq_ignore_ascii_case("true")),
            "external-controller" => controller = Some(value.to_owned()),
            _ => {}
        }
    }
    if mixed != Some(7890) {
        return Err("the top-level setting mixed-port: 7890 is required".into());
    }
    if allow_lan != Some(true) {
        return Err("the top-level setting allow-lan: true is required".into());
    }
    if let Some(address) = controller {
        if !address.starts_with("127.0.0.1:") && !address.starts_with("localhost:") {
            return Err("external-controller must listen on 127.0.0.1 only".into());
        }
    }
    Ok(())
}

fn wan_connected(event: &Value) -> bool {
    let value = event
        .get("status")
        .or_else(|| event.get("connect_status"))
        .or_else(|| event.get("data").and_then(|data| data.get("status")));
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_i64().is_some_and(|value| value != 0),
        Some(Value::String(value)) => matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "up" | "connected" | "connect"
        ),
        _ => false,
    }
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[derive(Deserialize)]
struct ImportRequest {
    config: String,
    #[serde(default)]
    enabled: bool,
}

pub fn status(manager: &ClashManager) -> (u16, Value) {
    (200, json!({"ok": true, "data": manager.snapshot()}))
}

pub fn import(manager: &ClashManager, body: &[u8]) -> (u16, Value) {
    let request: ImportRequest = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return (400, json!({"ok": false, "error": "config and enabled are required"})),
    };
    match manager.replace_config(&request.config, request.enabled) {
        Ok(()) => status(manager),
        Err(error) => (400, json!({"ok": false, "error": error})),
    }
}

pub fn enable(manager: &ClashManager) -> (u16, Value) {
    match manager.set_enabled(true) {
        Ok(()) => status(manager),
        Err(error) => (503, json!({"ok": false, "error": error})),
    }
}

pub fn disable(manager: &ClashManager) -> (u16, Value) {
    match manager.set_enabled(false) {
        Ok(()) => status(manager),
        Err(error) => (503, json!({"ok": false, "error": error})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_utf8_safe() {
        assert_eq!(truncate("中abc", 2), "中a");
    }

    #[test]
    fn import_request_defaults_disabled() {
        let request: ImportRequest = serde_json::from_slice(br#"{"config":"mixed-port: 7890"}"#).unwrap();
        assert!(!request.enabled);
    }

    #[test]
    fn lan_policy_requires_safe_known_ports() {
        assert!(validate_lan_policy("mixed-port: 7890\nallow-lan: true\n").is_ok());
        assert!(validate_lan_policy("mixed-port: 7891\nallow-lan: true\n").is_err());
        assert!(validate_lan_policy("mixed-port: 7890\nallow-lan: false\n").is_err());
        assert!(validate_lan_policy("mixed-port: 7890\nallow-lan: true\nexternal-controller: 0.0.0.0:9090\n").is_err());
        assert!(validate_lan_policy("mixed-port: 7890\nallow-lan: true\nexternal-controller: 127.0.0.1:9091\n").is_ok());
    }

    #[test]
    fn wan_event_only_accepts_connected_states() {
        assert!(wan_connected(&json!({"status": "connected"})));
        assert!(wan_connected(&json!({"data": {"status": 1}})));
        assert!(!wan_connected(&json!({"status": "disconnected"})));
        assert!(!wan_connected(&json!({})));
    }
}
