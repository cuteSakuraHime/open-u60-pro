//! Mihomo (Clash.Meta) process, configuration and transparent proxy.
//!
//! The core lives on the persistent data partition. OpenU60 owns its process:
//! every imported YAML is turned into a runtime copy with router-owned
//! listeners ([`crate::clash_config`]) and checked by Mihomo itself before it
//! replaces the running one. The controller listens on loopback behind a
//! secret that changes at every start; the console reaches it only through
//! this module.
//!
//! LAN clients can always opt in through the mixed HTTP/SOCKS port. The
//! transparent proxy ([`crate::clash_proxy`]) is installed only after Mihomo
//! answers on that port and is removed the moment the process is stopped or
//! exits, so a bad proxy configuration cannot keep the hotspot offline.
//!
//! Threads: `clash-run` starts one Mihomo process, copies its output into a
//! bounded in-memory log and blocks until it exits (the process dies with
//! that thread, so it never outlives the agent). `clash-guard` exists only
//! while transparent rules are installed and re-arms them once a minute.
//! Nothing runs while Clash is disabled.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::clash_config::{self, RuntimeOptions, Summary, CONTROLLER, MIXED_PORT, TUN_DEVICE};
use crate::clash_proxy::{self, Plan};
use crate::net_rules;

const DIR: &str = "/data/open-u60/clash";
const BIN: &str = "/data/open-u60/clash/mihomo";
const CONFIG: &str = "/data/open-u60/clash/config.yaml";
const CONFIG_NEW: &str = "/data/open-u60/clash/config.yaml.new";
const RUNTIME: &str = "/data/open-u60/clash/runtime.yaml";
const RUNTIME_NEW: &str = "/data/open-u60/clash/runtime.yaml.new";
const STATE: &str = "/data/open-u60/clash/state.json";
const STATE_NEW: &str = "/data/open-u60/clash/state.json.new";
const CHINA_LIST: &str = "/data/open-u60/clash/china_ip_list.txt";
const CHINA_LIST_NEW: &str = "/data/open-u60/clash/china_ip_list.txt.new";
const CHINA_LIST_URLS: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/17mon/china_ip_list@master/china_ip_list.txt",
    "https://raw.githubusercontent.com/17mon/china_ip_list/master/china_ip_list.txt",
];
const DELAY_TEST_URL: &str = "http%3A%2F%2Fwww.gstatic.com%2Fgenerate_204";

const MAX_CONFIG: usize = 1024 * 1024;
/// Body limit of `PUT /api/vpn/clash`: the configuration as one JSON string.
pub const IMPORT_BODY_MAX: usize = MAX_CONFIG + 256 * 1024;
const MAX_URL: usize = 2048;
const MAX_NAME: usize = 256;
const MAX_GROUPS: usize = 64;
const MAX_MEMBERS: usize = 256;
const LOG_CAPACITY: usize = 64 * 1024;
const LOG_LINE_MAX: usize = 1024;

const READY_TIMEOUT: Duration = Duration::from_secs(45);
const TUN_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(4);
const GUARD_INTERVAL: Duration = Duration::from_secs(60);
/// An unexpected exit is followed by one restart only after this much uptime.
const RESTART_MIN_UPTIME: Duration = Duration::from_secs(60);

const LOG_LEVELS: &[&str] = &["silent", "error", "warning", "info", "debug"];

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    /// Redirect LAN clients' TCP to Mihomo.
    pub transparent: bool,
    /// Also route their UDP into Mihomo (through its TUN device).
    pub udp: bool,
    /// Answer their DNS queries with Mihomo's resolver.
    pub dns_hijack: bool,
    /// Leave mainland China destinations on the direct path.
    pub bypass_china: bool,
    pub log_level: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            transparent: false,
            udp: true,
            dns_hijack: true,
            bypass_china: false,
            log_level: "warning".into(),
        }
    }
}

impl Settings {
    fn patched(&self, patch: &Value) -> Result<Self, String> {
        let Some(patch) = patch.as_object() else {
            return Err("expected a JSON object".into());
        };
        let mut next = self.clone();
        for (key, value) in patch {
            let flag = || {
                value
                    .as_bool()
                    .ok_or_else(|| format!("{key} must be a boolean"))
            };
            match key.as_str() {
                "transparent" => next.transparent = flag()?,
                "udp" => next.udp = flag()?,
                "dns_hijack" => next.dns_hijack = flag()?,
                "bypass_china" => next.bypass_china = flag()?,
                "log_level" => {
                    let level = value.as_str().unwrap_or_default();
                    if !LOG_LEVELS.contains(&level) {
                        return Err(format!(
                            "log_level must be one of {}",
                            LOG_LEVELS.join(", ")
                        ));
                    }
                    next.log_level = level.to_owned();
                }
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        Ok(next)
    }

    /// Loaded from disk: an unknown level falls back instead of failing.
    fn sanitized(mut self) -> Self {
        if !LOG_LEVELS.contains(&self.log_level.as_str()) {
            self.log_level = Self::default().log_level;
        }
        self
    }

    fn dns_intercepted(&self) -> bool {
        self.transparent && self.dns_hijack
    }

    fn udp_intercepted(&self) -> bool {
        self.transparent && self.udp
    }

    /// The mainland China bypass matches destination addresses, so client
    /// DNS has to hand out real ones for it to see anything.
    fn real_addresses(&self) -> bool {
        self.dns_intercepted() && self.bypass_china
    }

    fn plan(&self) -> Plan {
        Plan {
            udp: self.udp,
            dns_hijack: self.dns_hijack,
            bypass_china: self.bypass_china,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Subscription {
    /// Often carries the account token: stored 0600, never returned.
    url: String,
    updated_at: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct SavedState {
    enabled: bool,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    subscription: Option<Subscription>,
}

struct Process {
    pid: u32,
    started: Instant,
    started_at: u64,
}

struct Runtime {
    saved: SavedState,
    summary: Option<Summary>,
    process: Option<Process>,
    /// Bumped by every start and stop; a `clash-run` thread whose generation
    /// is stale knows its process was stopped on purpose.
    generation: u64,
    secret: String,
    rules: Option<Plan>,
    /// Dropping it ends the `clash-guard` thread.
    guard: Option<mpsc::Sender<()>>,
    china_entries: Option<usize>,
    version: Option<String>,
    last_error: Option<String>,
}

#[derive(Default)]
struct LogBuffer {
    lines: VecDeque<String>,
    bytes: usize,
}

impl LogBuffer {
    fn push(&mut self, line: &str) {
        let line = truncate(line.trim_end(), LOG_LINE_MAX);
        if line.is_empty() {
            return;
        }
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.bytes > LOG_CAPACITY {
            match self.lines.pop_front() {
                Some(old) => self.bytes -= old.len() + 1,
                None => break,
            }
        }
    }

    fn tail(&self, count: usize) -> String {
        let skip = self.lines.len().saturating_sub(count);
        self.lines
            .iter()
            .skip(skip)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
    }
}

pub struct ClashManager {
    /// Serialises everything that starts, stops or reconfigures Mihomo.
    /// Status reads take only `runtime`, so they never wait behind it.
    ops: Mutex<()>,
    runtime: Mutex<Runtime>,
    log: Mutex<LogBuffer>,
}

impl ClashManager {
    pub fn new() -> Self {
        let mut saved = fs::read(STATE)
            .ok()
            .and_then(|data| serde_json::from_slice::<SavedState>(&data).ok())
            .unwrap_or_default();
        saved.settings = saved.settings.sanitized();
        let summary = fs::read_to_string(CONFIG)
            .ok()
            .map(|text| clash_config::summarize(&text));
        Self {
            ops: Mutex::new(()),
            runtime: Mutex::new(Runtime {
                saved,
                summary,
                process: None,
                generation: 0,
                secret: String::new(),
                rules: None,
                guard: None,
                china_entries: None,
                version: None,
                last_error: None,
            }),
            log: Mutex::new(LogBuffer::default()),
        }
    }

    /// Clear rules a previous agent left behind, start Mihomo if it is
    /// enabled, then restart it after WAN reconnects: Mihomo resolves names
    /// through the system resolver, and on this firmware the agent starts
    /// before the uplink's DNS servers are installed.
    pub fn start(self: &Arc<Self>, wan_events: mpsc::Receiver<Value>) {
        let manager = Arc::clone(self);
        crate::system::spawn_named("clash-wan", move || {
            {
                let _ops = manager.ops();
                clash_proxy::remove();
                if manager.lock().saved.enabled {
                    manager.start_or_report("startup");
                }
            }
            while let Ok(event) = wan_events.recv() {
                // A reconnect arrives as a burst of events: one restart for all of them.
                let mut connected = wan_connected(&event);
                while let Ok(queued) = wan_events.try_recv() {
                    connected |= wan_connected(&queued);
                }
                if !connected {
                    continue;
                }
                let _ops = manager.ops();
                if manager.lock().saved.enabled {
                    manager.start_or_report("WAN reconnect");
                }
            }
        });
    }

    fn ops(&self) -> MutexGuard<'_, ()> {
        self.ops.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> MutexGuard<'_, Runtime> {
        self.runtime.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self) -> MutexGuard<'_, LogBuffer> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn installed() -> bool {
        Path::new(BIN).is_file()
    }

    fn configured() -> bool {
        Path::new(CONFIG).is_file()
    }

    fn start_or_report(self: &Arc<Self>, reason: &str) {
        if let Err(error) = self.start_process() {
            eprintln!("[clash] {reason} start failed: {error}");
            self.lock().last_error = Some(error);
        }
    }

    fn validate_config(path: &str) -> Result<(), String> {
        let output = Command::new(BIN)
            .args(["-t", "-d", DIR, "-f", path])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("run Mihomo config check: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        // Mihomo prints the reason last, after its start-up lines.
        let reason = detail.lines().rev().take(3).collect::<Vec<_>>();
        let reason = reason.into_iter().rev().collect::<Vec<_>>().join(" ");
        Err(format!(
            "invalid Mihomo configuration: {}",
            truncate(&reason, 800)
        ))
    }

    /// Compose the runtime copy of `user` and let Mihomo check it.
    fn build_runtime(user: &str, settings: &Settings, secret: &str) -> Result<(), String> {
        if !Self::installed() {
            return Err("Mihomo is not installed".into());
        }
        let lan_address = net_rules::lan_network()
            .map(|network| network.addr())
            .unwrap_or(Ipv4Addr::new(192, 168, 0, 1));
        let text = clash_config::compose(
            user,
            &RuntimeOptions {
                secret,
                log_level: &settings.log_level,
                dns_hijack: settings.dns_intercepted(),
                real_addresses: settings.real_addresses(),
                tun: settings.udp_intercepted(),
                lan_address,
            },
        )?;
        fs::create_dir_all(DIR).map_err(|e| format!("create Clash directory: {e}"))?;
        atomic_write(RUNTIME_NEW, RUNTIME_NEW, text.as_bytes(), 0o600)?;
        if let Err(error) = Self::validate_config(RUNTIME_NEW) {
            let _ = fs::remove_file(RUNTIME_NEW);
            return Err(error);
        }
        Ok(())
    }

    /// (Re)start Mihomo with the saved configuration and settings. The
    /// running process is replaced only after the new runtime copy passed
    /// Mihomo's own check. Caller holds `ops`.
    fn start_process(self: &Arc<Self>) -> Result<(), String> {
        if !Self::configured() {
            return Err("import a Clash configuration before enabling it".into());
        }
        let settings = self.lock().saved.settings.clone();
        let secret = new_secret()?;
        let user =
            fs::read_to_string(CONFIG).map_err(|e| format!("read Clash configuration: {e}"))?;
        Self::build_runtime(&user, &settings, &secret)?;
        self.stop_process();
        kill_strays();
        fs::rename(RUNTIME_NEW, RUNTIME)
            .map_err(|e| format!("commit runtime configuration: {e}"))?;

        let generation = {
            let mut state = self.lock();
            state.generation += 1;
            state.secret = secret;
            state.last_error = None;
            state.generation
        };
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let manager = Arc::clone(self);
        crate::system::try_spawn_named("clash-run", move || {
            manager.run_child(generation, spawned_tx)
        })
        .map_err(|e| format!("start Mihomo thread: {e}"))?;
        match spawned_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err("Mihomo did not start".into()),
        }
        if let Err(error) = self.wait_ready(generation) {
            self.stop_process();
            return Err(error);
        }
        if settings.transparent {
            if let Err(error) = self.install_rules(&settings, generation) {
                // Mihomo keeps serving the explicit proxy port.
                eprintln!("[clash] transparent proxy not installed: {error}");
                self.lock().last_error = Some(format!("transparent proxy is not active: {error}"));
            }
        }
        Ok(())
    }

    /// Body of the `clash-run` thread: one Mihomo process from start to exit.
    fn run_child(self: Arc<Self>, generation: u64, spawned: mpsc::Sender<Result<(), String>>) {
        let (mut child, output) = match spawn_mihomo() {
            Ok(pair) => pair,
            Err(error) => {
                let _ = spawned.send(Err(error));
                return;
            }
        };
        let pid = child.id();
        {
            let mut state = self.lock();
            if state.generation != generation {
                drop(state);
                let _ = child.kill();
                let _ = child.wait();
                let _ = spawned.send(Err("superseded by a newer start".into()));
                return;
            }
            state.process = Some(Process {
                pid,
                started: Instant::now(),
                started_at: unix_now(),
            });
        }
        self.log()
            .push(&format!("--- Mihomo started (pid {pid}) ---"));
        let _ = spawned.send(Ok(()));
        drop(spawned);

        for line in BufReader::new(output).split(b'\n').map_while(Result::ok) {
            self.log().push(&String::from_utf8_lossy(&line));
        }
        wait_for_exit(pid);

        // The pid is forgotten under the lock before the process is reaped,
        // so `stop_process` can never signal a recycled pid.
        let (unexpected, uptime, status) = {
            let mut state = self.lock();
            let uptime = state
                .process
                .as_ref()
                .filter(|process| process.pid == pid)
                .map(|process| process.started.elapsed());
            if uptime.is_some() {
                state.process = None;
            }
            let status = child
                .wait()
                .map_or_else(|error| error.to_string(), |status| status.to_string());
            (
                state.generation == generation,
                uptime.unwrap_or_default(),
                status,
            )
        };
        self.log()
            .push(&format!("--- Mihomo exited ({status}) ---"));
        if unexpected {
            self.after_unexpected_exit(generation, uptime, &status);
        }
    }

    fn after_unexpected_exit(self: &Arc<Self>, generation: u64, uptime: Duration, status: &str) {
        let _ops = self.ops();
        {
            let mut state = self.lock();
            if state.generation != generation {
                return;
            }
            state.last_error = Some(format!("Mihomo stopped unexpectedly ({status})"));
        }
        self.remove_rules();
        eprintln!("[clash] Mihomo exited ({status}); LAN traffic is direct again");
        if uptime >= RESTART_MIN_UPTIME && self.lock().saved.enabled {
            self.start_or_report("automatic");
        }
    }

    fn wait_ready(&self, generation: u64) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, MIXED_PORT));
        loop {
            let alive = {
                let state = self.lock();
                state.generation == generation && state.process.is_some()
            };
            if !alive {
                let tail = self.log().tail(4);
                return Err(format!(
                    "Mihomo exited during start-up. {}",
                    truncate(&tail, 600)
                ));
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("Mihomo did not open its proxy port in time".into());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Remove the transparent rules, then stop the process. Caller holds `ops`.
    fn stop_process(&self) {
        self.remove_rules();
        let running = {
            let mut state = self.lock();
            state.generation += 1;
            match state.process.as_ref() {
                Some(process) => {
                    signal(process.pid, libc::SIGTERM);
                    true
                }
                None => false,
            }
        };
        if !running {
            return;
        }
        let gone = |wait: Duration| {
            let deadline = Instant::now() + wait;
            loop {
                if self.lock().process.is_none() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        if !gone(STOP_TIMEOUT) {
            if let Some(process) = self.lock().process.as_ref() {
                signal(process.pid, libc::SIGKILL);
            }
            gone(Duration::from_secs(2));
        }
    }

    fn install_rules(self: &Arc<Self>, settings: &Settings, generation: u64) -> Result<(), String> {
        let plan = settings.plan();
        let china = if plan.bypass_china {
            let list = load_china_list();
            if list.is_empty() {
                return Err("the mainland China address list has not been downloaded".into());
            }
            list
        } else {
            Vec::new()
        };
        if plan.udp && !wait_for_tun() {
            return Err("Mihomo did not create its TUN device".into());
        }
        clash_proxy::install(plan, &china)?;
        let (stop_tx, stop_rx) = mpsc::channel();
        let manager = Arc::clone(self);
        let spawned = crate::system::try_spawn_named("clash-guard", move || {
            manager.guard_loop(generation, plan, stop_rx)
        });
        if let Err(error) = spawned {
            clash_proxy::remove();
            return Err(format!("start rule keepalive: {error}"));
        }
        let mut state = self.lock();
        state.rules = Some(plan);
        state.guard = Some(stop_tx);
        if plan.bypass_china {
            state.china_entries = Some(china.len());
        }
        Ok(())
    }

    fn remove_rules(&self) {
        let installed = {
            let mut state = self.lock();
            state.guard = None;
            state.rules.take().is_some()
        };
        if installed {
            clash_proxy::remove();
        }
    }

    /// Body of the `clash-guard` thread: keep the rules armed, and put them
    /// back after the firmware reloads its firewall.
    fn guard_loop(self: Arc<Self>, generation: u64, plan: Plan, stop: mpsc::Receiver<()>) {
        while let Err(mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(GUARD_INTERVAL) {
            let Ok(_ops) = self.ops.try_lock() else {
                // A change is in progress and will settle the rules itself.
                let _ = clash_proxy::refresh_guard();
                continue;
            };
            {
                let state = self.lock();
                if state.generation != generation || state.rules != Some(plan) {
                    return;
                }
            }
            if clash_proxy::intact(plan) {
                if let Err(error) = clash_proxy::refresh_guard() {
                    eprintln!("[clash] rule keepalive failed: {error}");
                }
                continue;
            }
            eprintln!("[clash] firewall was reloaded; reinstalling transparent proxy rules");
            let settings = self.lock().saved.settings.clone();
            self.remove_rules();
            if let Err(error) = self.install_rules(&settings, generation) {
                self.lock().last_error = Some(format!("transparent proxy is not active: {error}"));
            }
            return;
        }
    }

    fn persist(saved: &SavedState) -> Result<(), String> {
        fs::create_dir_all(DIR).map_err(|e| format!("create Clash directory: {e}"))?;
        let data = serde_json::to_vec(saved).map_err(|e| e.to_string())?;
        atomic_write(STATE_NEW, STATE, &data, 0o600)
    }

    /// Change and persist the saved state; memory changes only if the write worked.
    fn save(&self, change: impl FnOnce(&mut SavedState)) -> Result<(), String> {
        let mut next = self.lock().saved.clone();
        change(&mut next);
        Self::persist(&next)?;
        self.lock().saved = next;
        Ok(())
    }

    fn set_enabled(self: &Arc<Self>, enabled: bool) -> Result<(), String> {
        if enabled {
            self.start_process()?;
        } else {
            self.stop_process();
        }
        if let Err(error) = self.save(|saved| saved.enabled = enabled) {
            if enabled {
                self.stop_process();
            }
            return Err(error);
        }
        if !enabled {
            self.lock().last_error = None;
        }
        Ok(())
    }

    /// Check `text` with Mihomo and make it the saved configuration.
    fn commit_config(&self, text: &str) -> Result<(), String> {
        if text.is_empty() || text.len() > MAX_CONFIG {
            return Err(format!("configuration must be 1-{MAX_CONFIG} bytes"));
        }
        let settings = self.lock().saved.settings.clone();
        Self::build_runtime(text, &settings, "check-only")?;
        let _ = fs::remove_file(RUNTIME_NEW);
        atomic_write(CONFIG_NEW, CONFIG, text.as_bytes(), 0o600)?;
        self.lock().summary = Some(clash_config::summarize(text));
        Ok(())
    }

    fn update_settings(self: &Arc<Self>, patch: &Value) -> Result<(), String> {
        let current = self.lock().saved.settings.clone();
        let next = current.patched(patch)?;
        if next == current {
            return Ok(());
        }
        if next.transparent && next.bypass_china && load_china_list().is_empty() {
            let entries = download_china_list().map_err(|error| {
                format!("could not download the mainland China address list: {error}")
            })?;
            self.lock().china_entries = Some(entries);
        }
        self.save(|saved| saved.settings = next.clone())?;
        let (running, generation) = {
            let state = self.lock();
            (state.process.is_some(), state.generation)
        };
        if !running {
            return Ok(());
        }
        let restart = current.dns_intercepted() != next.dns_intercepted()
            || current.udp_intercepted() != next.udp_intercepted()
            || current.real_addresses() != next.real_addresses()
            || current.log_level != next.log_level;
        let applied = if restart {
            self.start_process()
        } else {
            self.remove_rules();
            if next.transparent {
                self.install_rules(&next, generation)
            } else {
                Ok(())
            }
        };
        if let Err(error) = applied {
            // Back to the settings that worked, and to a running proxy.
            let _ = self.save(|saved| saved.settings = current);
            self.start_or_report("rollback");
            return Err(error);
        }
        Ok(())
    }

    fn subscribe(self: &Arc<Self>, url: Option<&str>, enabled: Option<bool>) -> Result<(), String> {
        let url = match url {
            Some(url) => validate_url(url)?.to_owned(),
            None => self
                .lock()
                .saved
                .subscription
                .as_ref()
                .map(|subscription| subscription.url.clone())
                .ok_or("no subscription URL is saved")?,
        };
        let text = fetch_text(&url, MAX_CONFIG as u64, "clash.meta")?;
        self.commit_config(&text)?;
        self.save(|saved| {
            saved.subscription = Some(Subscription {
                url,
                updated_at: unix_now(),
            })
        })?;
        let enabled = enabled.unwrap_or_else(|| self.lock().saved.enabled);
        self.set_enabled(enabled)
    }

    fn version(&self) -> Option<String> {
        if let Some(version) = self.lock().version.clone() {
            return Some(version);
        }
        if !Self::installed() {
            return None;
        }
        let output = Command::new(BIN)
            .arg("-v")
            .stdin(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let version = truncate(text.lines().next()?.trim(), 160);
        if version.is_empty() {
            return None;
        }
        self.lock().version = Some(version.clone());
        Some(version)
    }

    fn snapshot(&self) -> Value {
        let version = self.version();
        let lan_address = net_rules::lan_network()
            .map(|network| network.addr())
            .unwrap_or(Ipv4Addr::new(192, 168, 0, 1));
        let china_updated = modified_at(CHINA_LIST);
        if china_updated.is_some() && self.lock().china_entries.is_none() {
            let entries = load_china_list().len();
            self.lock().china_entries = Some(entries);
        }
        let state = self.lock();
        let settings = &state.saved.settings;
        let china_entries = china_updated.and(state.china_entries);
        json!({
            "installed": Self::installed(),
            "configured": Self::configured(),
            "enabled": state.saved.enabled,
            "running": state.process.is_some(),
            "started_at": state.process.as_ref().map(|process| process.started_at),
            // Durations are measured on the router: its wall clock and the
            // browser's need not agree, so the page never subtracts the two.
            "uptime": state.process.as_ref().map(|process| process.started.elapsed().as_secs()),
            "now": unix_now(),
            "version": version,
            "mode": if state.rules.is_some() { "transparent" } else { "explicit_proxy" },
            "transparent_active": state.rules.is_some(),
            "mixed_port": MIXED_PORT,
            "proxy_address": format!("{lan_address}:{MIXED_PORT}"),
            "settings": settings,
            "summary": state.summary,
            "subscription": state.saved.subscription.as_ref().map(|subscription| json!({
                "host": url_host(&subscription.url),
                "updated_at": subscription.updated_at,
            })),
            "china_list": {"entries": china_entries, "updated_at": china_updated},
            "last_error": state.last_error,
        })
    }

    /// The controller's address and this start's secret, while Mihomo runs.
    fn controller(&self) -> Result<(ureq::Agent, String), String> {
        let secret = {
            let state = self.lock();
            if state.process.is_none() {
                return Err("Mihomo is not running".into());
            }
            state.secret.clone()
        };
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .proxy(None)
            .build()
            .into();
        Ok((agent, format!("Bearer {secret}")))
    }

    fn controller_get(&self, path: &str) -> Result<Value, String> {
        let (agent, authorization) = self.controller()?;
        agent
            .get(format!("http://{CONTROLLER}{path}"))
            .header("Authorization", authorization)
            .call()
            .map_err(|error| controller_error(&error))?
            .into_body()
            .read_json()
            .map_err(|error| format!("read Mihomo reply: {error}"))
    }

    fn controller_put(&self, path: &str, body: &Value) -> Result<(), String> {
        let (agent, authorization) = self.controller()?;
        agent
            .put(format!("http://{CONTROLLER}{path}"))
            .header("Authorization", authorization)
            .send_json(body)
            .map(|_| ())
            .map_err(|error| controller_error(&error))
    }
}

fn controller_error(error: &ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(404) => "Mihomo does not know that proxy or group".into(),
        ureq::Error::StatusCode(code) => format!("Mihomo refused the request (HTTP {code})"),
        other => format!("Mihomo controller unavailable: {other}"),
    }
}

/// Mihomo with both output streams on one pipe. Must be called on the thread
/// that then waits for the process: the kernel stops the child when that
/// thread ends ([`crate::event_bus::die_with_parent`]).
fn spawn_mihomo() -> Result<(Child, std::io::PipeReader), String> {
    let (reader, writer) = std::io::pipe().map_err(|e| format!("create Mihomo log pipe: {e}"))?;
    let errors = writer
        .try_clone()
        .map_err(|e| format!("clone Mihomo log pipe: {e}"))?;
    let mut command = Command::new(BIN);
    command
        .args(["-d", DIR, "-f", RUNTIME])
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(errors);
    crate::event_bus::die_with_parent(&mut command);
    let child = command.spawn().map_err(|e| format!("start Mihomo: {e}"))?;
    // The command still holds the pipe's write ends; without this the reader
    // would never see the end of the child's output.
    drop(command);
    Ok((child, reader))
}

/// Block until `pid` has exited, without reaping it.
fn wait_for_exit(pid: u32) {
    loop {
        // SAFETY: an all-zero siginfo_t is a valid value for an out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` outlives the call; `pid` is this thread's own child.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// Mihomo creates its TUN device shortly after its listeners.
fn wait_for_tun() -> bool {
    let device = format!("/sys/class/net/{TUN_DEVICE}");
    let deadline = Instant::now() + TUN_TIMEOUT;
    loop {
        if Path::new(&device).exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: kill(2) on a child that has not been reaped (see `run_child`).
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// Mihomo processes this agent does not own: left by an earlier agent that
/// was killed, and still holding the proxy and controller ports.
fn stray_pids() -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe == Path::new(BIN))
        })
        .collect()
}

fn kill_strays() {
    let strays = stray_pids();
    if strays.is_empty() {
        return;
    }
    eprintln!(
        "[clash] stopping {} Mihomo process(es) left by an earlier agent",
        strays.len()
    );
    for pid in strays {
        signal(pid, libc::SIGTERM);
    }
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if stray_pids().is_empty() {
            return;
        }
    }
    for pid in stray_pids() {
        signal(pid, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(300));
}

fn new_secret() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| "no entropy for the controller secret".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn modified_at(path: &str) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn load_china_list() -> Vec<Ipv4Net> {
    fs::read_to_string(CHINA_LIST)
        .map(|text| clash_proxy::parse_networks(&text))
        .unwrap_or_default()
}

fn download_china_list() -> Result<usize, String> {
    let mut last_error = String::from("no source configured");
    for url in CHINA_LIST_URLS {
        match fetch_text(url, 4 * 1024 * 1024, "OpenU60") {
            Ok(text) => {
                let entries = clash_proxy::parse_networks(&text).len();
                if entries < 1000 {
                    last_error = "the downloaded list is incomplete".into();
                    continue;
                }
                fs::create_dir_all(DIR).map_err(|e| format!("create Clash directory: {e}"))?;
                atomic_write(CHINA_LIST_NEW, CHINA_LIST, text.as_bytes(), 0o644)?;
                return Ok(entries);
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// Download a text file. Errors never repeat the URL: a subscription URL
/// carries the account token.
fn fetch_text(url: &str, limit: u64, user_agent: &str) -> Result<String, String> {
    if cfg!(test) {
        return Err("no downloads in tests".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let response = agent
        .get(url)
        .header("User-Agent", user_agent)
        .call()
        .map_err(|error| match error {
            ureq::Error::StatusCode(code) => format!("the server answered HTTP {code}"),
            ureq::Error::BadUri(_) => "the URL is not valid".to_string(),
            other => format!(
                "download failed: {}",
                other.to_string().replace(url, "the URL")
            ),
        })?;
    response
        .into_body()
        .with_config()
        .limit(limit)
        .read_to_string()
        .map_err(|_| "the download is not text or is too large".to_string())
}

fn validate_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("the subscription URL must start with http:// or https://")?;
    if rest.is_empty()
        || url.len() > MAX_URL
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("the subscription URL is not valid".into());
    }
    Ok(url)
}

/// The host of a URL, for display: no credentials, path or query.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    truncate(host, 120)
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Policy groups from the controller's `/proxies` reply, in the order of the
/// configuration (the built-in GLOBAL group lists them that way).
fn shape_groups(reply: &Value) -> Vec<Value> {
    let Some(proxies) = reply["proxies"].as_object() else {
        return Vec::new();
    };
    let is_group = |name: &str| {
        name != "GLOBAL"
            && proxies
                .get(name)
                .is_some_and(|proxy| proxy["all"].is_array() && proxy["hidden"] != true)
    };
    let mut names: Vec<&str> = proxies
        .get("GLOBAL")
        .and_then(|global| global["all"].as_array())
        .map(|all| all.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    names.retain(|name| is_group(name));
    for name in proxies.keys() {
        if is_group(name) && !names.contains(&name.as_str()) {
            names.push(name.as_str());
        }
    }
    names
        .into_iter()
        .take(MAX_GROUPS)
        .map(|name| {
            let group = &proxies[name];
            let members: Vec<Value> = group["all"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(MAX_MEMBERS)
                .map(|member| {
                    let proxy = proxies.get(member);
                    let delay = proxy.and_then(|proxy| {
                        proxy["history"].as_array()?.last()?.get("delay")?.as_u64()
                    });
                    json!({
                        "name": member,
                        "type": proxy.map_or(Value::Null, |proxy| proxy["type"].clone()),
                        "delay": delay,
                    })
                })
                .collect();
            json!({
                "name": name,
                "type": group["type"],
                "now": group["now"],
                "selectable": group["type"] == "Selector",
                "members": members,
            })
        })
        .collect()
}

fn atomic_write(temp: &str, target: &str, data: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(mode)
        .open(temp)
        .map_err(|e| format!("open {temp}: {e}"))?;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| e.to_string())?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("write {temp}: {e}"))?;
    if temp != target {
        fs::rename(temp, target).map_err(|e| format!("rename {temp}: {e}"))?;
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

fn refused(status: u16, error: impl Into<String>) -> (u16, Value) {
    (status, json!({"ok": false, "error": error.into()}))
}

fn name_field<'a>(body: &'a Value, key: &str) -> Result<&'a str, String> {
    body[key]
        .as_str()
        .filter(|name| !name.is_empty() && name.len() <= MAX_NAME)
        .ok_or_else(|| format!("{key} is required"))
}

#[derive(Deserialize)]
struct ImportRequest {
    config: String,
    #[serde(default)]
    enabled: bool,
}

#[derive(Deserialize)]
struct SubscriptionRequest {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

pub fn status(manager: &ClashManager) -> (u16, Value) {
    (200, json!({"ok": true, "data": manager.snapshot()}))
}

pub fn import(manager: &Arc<ClashManager>, body: &[u8]) -> (u16, Value) {
    let Ok(request) = serde_json::from_slice::<ImportRequest>(body) else {
        return refused(400, "config and enabled are required");
    };
    let _ops = manager.ops();
    if let Err(error) = manager.commit_config(&request.config) {
        return refused(400, error);
    }
    match manager.set_enabled(request.enabled) {
        Ok(()) => status(manager),
        Err(error) => refused(
            503,
            format!("configuration saved, but Mihomo did not start: {error}"),
        ),
    }
}

pub fn enable(manager: &Arc<ClashManager>) -> (u16, Value) {
    let _ops = manager.ops();
    match manager.set_enabled(true) {
        Ok(()) => status(manager),
        Err(error) => refused(503, error),
    }
}

pub fn disable(manager: &Arc<ClashManager>) -> (u16, Value) {
    let _ops = manager.ops();
    match manager.set_enabled(false) {
        Ok(()) => status(manager),
        Err(error) => refused(503, error),
    }
}

pub fn restart(manager: &Arc<ClashManager>) -> (u16, Value) {
    let _ops = manager.ops();
    if !manager.lock().saved.enabled {
        return refused(409, "Clash is disabled");
    }
    match manager.start_process() {
        Ok(()) => status(manager),
        Err(error) => {
            manager.lock().last_error = Some(error.clone());
            refused(503, error)
        }
    }
}

pub fn settings(manager: &Arc<ClashManager>, body: &[u8]) -> (u16, Value) {
    let Ok(patch) = serde_json::from_slice::<Value>(body) else {
        return refused(400, "invalid JSON");
    };
    let _ops = manager.ops();
    match manager.update_settings(&patch) {
        Ok(()) => status(manager),
        Err(error) => refused(400, error),
    }
}

pub fn subscribe(manager: &Arc<ClashManager>, body: &[u8]) -> (u16, Value) {
    let Ok(request) = serde_json::from_slice::<SubscriptionRequest>(body) else {
        return refused(400, "invalid JSON");
    };
    let _ops = manager.ops();
    match manager.subscribe(request.url.as_deref(), request.enabled) {
        Ok(()) => status(manager),
        Err(error) => refused(400, error),
    }
}

pub fn forget_subscription(manager: &ClashManager) -> (u16, Value) {
    let _ops = manager.ops();
    match manager.save(|saved| saved.subscription = None) {
        Ok(()) => status(manager),
        Err(error) => refused(503, error),
    }
}

pub fn update_china_list(manager: &Arc<ClashManager>) -> (u16, Value) {
    let _ops = manager.ops();
    let entries = match download_china_list() {
        Ok(entries) => entries,
        Err(error) => return refused(503, error),
    };
    let (generation, settings, reinstall) = {
        let mut state = manager.lock();
        state.china_entries = Some(entries);
        let reinstall = state.rules.is_some_and(|plan| plan.bypass_china);
        (state.generation, state.saved.settings.clone(), reinstall)
    };
    if reinstall {
        manager.remove_rules();
        if let Err(error) = manager.install_rules(&settings, generation) {
            manager.lock().last_error = Some(format!("transparent proxy is not active: {error}"));
            return refused(503, error);
        }
    }
    status(manager)
}

pub fn log(manager: &ClashManager) -> (u16, Value) {
    let text = manager.log().tail(usize::MAX);
    (200, json!({"ok": true, "data": {"text": text}}))
}

pub fn clear_log(manager: &ClashManager) -> (u16, Value) {
    manager.log().clear();
    (200, json!({"ok": true, "data": {"text": ""}}))
}

pub fn proxies(manager: &ClashManager) -> (u16, Value) {
    match manager.controller_get("/proxies") {
        Ok(reply) => (
            200,
            json!({"ok": true, "data": {"groups": shape_groups(&reply)}}),
        ),
        Err(error) => refused(409, error),
    }
}

pub fn select_proxy(manager: &ClashManager, body: &[u8]) -> (u16, Value) {
    let body: Value = serde_json::from_slice(body).unwrap_or_default();
    let (group, proxy) = match (name_field(&body, "group"), name_field(&body, "proxy")) {
        (Ok(group), Ok(proxy)) => (group, proxy),
        (Err(error), _) | (_, Err(error)) => return refused(400, error),
    };
    let path = format!("/proxies/{}", percent_encode(group));
    match manager.controller_put(&path, &json!({"name": proxy})) {
        Ok(()) => proxies(manager),
        Err(error) => refused(409, error),
    }
}

pub fn test_group(manager: &ClashManager, body: &[u8]) -> (u16, Value) {
    let body: Value = serde_json::from_slice(body).unwrap_or_default();
    let group = match name_field(&body, "group") {
        Ok(group) => group,
        Err(error) => return refused(400, error),
    };
    let path = format!(
        "/group/{}/delay?url={DELAY_TEST_URL}&timeout=5000",
        percent_encode(group)
    );
    match manager.controller_get(&path) {
        Ok(_) => proxies(manager),
        Err(error) => refused(409, error),
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
        let request: ImportRequest =
            serde_json::from_slice(br#"{"config":"mixed-port: 7890"}"#).unwrap();
        assert!(!request.enabled);
    }

    #[test]
    fn state_saved_by_the_first_release_still_loads() {
        let saved: SavedState = serde_json::from_slice(br#"{"enabled":true}"#).unwrap();
        assert!(saved.enabled);
        assert_eq!(saved.settings, Settings::default());
        assert!(!saved.settings.transparent);
        assert!(saved.subscription.is_none());

        let odd: SavedState = serde_json::from_slice(
            br#"{"enabled":false,"settings":{"transparent":true,"log_level":"loud"}}"#,
        )
        .unwrap();
        let settings = odd.settings.sanitized();
        assert!(settings.transparent && settings.udp && settings.dns_hijack);
        assert_eq!(settings.log_level, "warning");
    }

    #[test]
    fn settings_patch_is_strict() {
        let base = Settings::default();
        let next = base
            .patched(&json!({"transparent": true, "udp": false, "log_level": "info"}))
            .unwrap();
        assert!(next.transparent && !next.udp && next.dns_hijack);
        assert_eq!(next.log_level, "info");
        assert!(next.dns_intercepted());
        assert!(!base.dns_intercepted());
        assert_eq!(
            next.plan(),
            Plan {
                udp: false,
                dns_hijack: true,
                bypass_china: false
            }
        );
        assert!(base.patched(&json!({"transparent": "yes"})).is_err());
        assert!(base.patched(&json!({"log_level": "trace"})).is_err());
        assert!(base.patched(&json!({"tun": true})).is_err());
        assert!(base.patched(&json!([])).is_err());
    }

    #[test]
    fn subscription_urls_are_checked_and_shown_without_secrets() {
        assert!(validate_url(" https://example.com/sub?token=abc ").is_ok());
        assert!(validate_url("ftp://example.com/sub").is_err());
        assert!(validate_url("https://").is_err());
        assert!(validate_url("https://example.com/a b").is_err());
        assert_eq!(
            url_host("https://account@sub.example.com:8443/api?token=abc"),
            "sub.example.com:8443"
        );
        assert_eq!(url_host("http://example.com"), "example.com");
    }

    #[test]
    fn group_names_are_path_safe() {
        assert_eq!(percent_encode("Auto-1_a.b~"), "Auto-1_a.b~");
        assert_eq!(percent_encode("节点 A/B"), "%E8%8A%82%E7%82%B9%20A%2FB");
    }

    #[test]
    fn log_keeps_only_the_newest_lines() {
        let mut log = LogBuffer::default();
        log.push("   ");
        assert_eq!(log.tail(10), "");
        let line = "x".repeat(LOG_LINE_MAX + 50);
        for _ in 0..100 {
            log.push(&line);
        }
        log.push("last\r\n");
        assert!(log.bytes <= LOG_CAPACITY);
        assert!(log.lines.len() < 100);
        assert_eq!(log.lines.front().unwrap().len(), LOG_LINE_MAX);
        assert_eq!(log.tail(1), "last");
        log.clear();
        assert_eq!((log.bytes, log.lines.len()), (0, 0));
    }

    #[test]
    fn groups_follow_the_configuration_order() {
        let reply = json!({"proxies": {
            "GLOBAL": {"type": "Selector", "now": "DIRECT", "all": ["DIRECT", "Zeta", "node-a", "Alpha"]},
            "Alpha": {"type": "URLTest", "now": "node-a", "all": ["node-a", "node-b"]},
            "Zeta": {"type": "Selector", "now": "Alpha", "all": ["Alpha", "DIRECT"]},
            "Hidden": {"type": "Selector", "now": "DIRECT", "all": ["DIRECT"], "hidden": true},
            "Late": {"type": "Fallback", "now": "node-b", "all": ["node-b"]},
            "DIRECT": {"type": "Direct", "history": []},
            "node-a": {"type": "Shadowsocks", "history": [{"delay": 310}, {"delay": 120}]},
            "node-b": {"type": "Trojan", "history": [{"delay": 0}]}
        }});
        let groups = shape_groups(&reply);
        let names: Vec<&str> = groups
            .iter()
            .map(|group| group["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Zeta", "Alpha", "Late"]);
        assert_eq!(groups[0]["selectable"], true);
        assert_eq!(groups[1]["selectable"], false);
        assert_eq!(groups[1]["now"], "node-a");
        assert_eq!(
            groups[1]["members"],
            json!([
                {"name": "node-a", "type": "Shadowsocks", "delay": 120},
                {"name": "node-b", "type": "Trojan", "delay": 0}
            ])
        );
        assert_eq!(
            groups[0]["members"][1],
            json!({"name": "DIRECT", "type": "Direct", "delay": null})
        );
        assert!(shape_groups(&json!({})).is_empty());
    }

    #[test]
    fn wan_event_only_accepts_connected_states() {
        assert!(wan_connected(&json!({"status": "connected"})));
        assert!(wan_connected(&json!({"data": {"status": 1}})));
        assert!(!wan_connected(&json!({"status": "disconnected"})));
        assert!(!wan_connected(&json!({})));
    }

    #[test]
    fn a_fresh_manager_reports_without_touching_the_device() {
        let manager = ClashManager::new();
        let (code, body) = status(&manager);
        assert_eq!(code, 200);
        assert_eq!(body["data"]["running"], false);
        assert_eq!(body["data"]["mode"], "explicit_proxy");
        assert_eq!(body["data"]["settings"]["transparent"], false);
        assert!(body["data"].get("secret").is_none());
        assert_eq!(proxies(&manager).0, 409);
        assert_eq!(select_proxy(&manager, b"{}").0, 400);
    }
}
