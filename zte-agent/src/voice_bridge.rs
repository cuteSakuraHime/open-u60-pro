//! Call-audio bridge between the modem and one LAN phone.
//!
//! The vendor tool `zte_route_hal_voice_test` (via `libztertp.so`) streams the
//! active call as PCMU RTP over UDP 48894, but the library hardcodes
//! `192.168.253.1` (router) and `192.168.253.100` (peer). The root filesystem is
//! read-only, so each start writes a patched copy to /tmp and loads it with
//! `LD_LIBRARY_PATH`. Firmware is never modified. Verified on a live call:
//! downlink 50 pkt/s PCMU from router:48894, uplink tone audible to the callee.

use std::net::Ipv4Addr;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const VENDOR_LIB: &str = "/usr/lib/libztertp.so";
const VENDOR_TOOL: &str = "/usr/bin/zte_route_hal_voice_test";
const WORK_DIR: &str = "/tmp/open-u60-voice";
const DEFAULT_ROUTER: &[u8] = b"192.168.253.1";
const DEFAULT_PEER: &[u8] = b"192.168.253.100";
/// Upper bound on one bridge session; the call-status poll stops it earlier.
const MAX_SECONDS: u32 = 4 * 3600;

#[derive(Default)]
pub struct VoiceBridge {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    child: Option<(Child, Ipv4Addr)>,
    /// How the last bridge ended on its own (not stopped by us), for `status`.
    last_exit: Option<String>,
}

impl Inner {
    /// Forget a bridge that has exited, keeping how it ended.
    fn reap(&mut self) {
        if let Some((child, _)) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    self.last_exit = Some(status.to_string());
                    self.child = None;
                }
                Err(e) => {
                    self.last_exit = Some(e.to_string());
                    self.child = None;
                }
            }
        }
    }
}

/// Stop the vendor tool: SIGINT (its own stop path, if it has one), then SIGTERM,
/// then SIGKILL. The grace periods give it a chance to close the modem's voice
/// session, which the next bridge must open again.
fn terminate(mut child: Child) {
    let pid = child.id() as libc::pid_t;
    for (signal, grace) in [(libc::SIGINT, 600u64), (libc::SIGTERM, 400)] {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        // The child is not reaped yet, so `pid` cannot have been reused.
        unsafe { libc::kill(pid, signal) };
        let deadline = Instant::now() + Duration::from_millis(grace);
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Replace one NUL-terminated default address in place, keeping the length.
fn replace_addr(lib: &mut [u8], old: &[u8], new: &str) -> Result<(), String> {
    if new.len() > old.len() {
        return Err("address longer than the vendor slot".into());
    }
    let mut needle = old.to_vec();
    needle.push(0);
    let hits: Vec<usize> = lib.windows(needle.len())
        .enumerate()
        .filter(|(i, w)| *w == needle.as_slice() && (*i == 0 || lib[*i - 1] == 0))
        .map(|(i, _)| i)
        .collect();
    let [at] = hits.as_slice() else {
        return Err(format!("unexpected vendor library layout ({} matches)", hits.len()));
    };
    let slot = &mut lib[*at..*at + old.len()];
    slot.fill(0);
    slot[..new.len()].copy_from_slice(new.as_bytes());
    Ok(())
}

pub(crate) fn patch_library(lib: &[u8], router: Ipv4Addr, peer: Ipv4Addr) -> Result<Vec<u8>, String> {
    let mut out = lib.to_vec();
    replace_addr(&mut out, DEFAULT_ROUTER, &router.to_string())?;
    replace_addr(&mut out, DEFAULT_PEER, &peer.to_string())?;
    Ok(out)
}

/// The phone must be a host on the router's own /24 LAN (not the router, not broadcast).
pub(crate) fn validate_peer(peer: Ipv4Addr, router: Ipv4Addr) -> Result<(), &'static str> {
    let (p, r) = (peer.octets(), router.octets());
    if p[..3] != r[..3] || peer == router || p[3] == 0 || p[3] == 255 {
        return Err("host must be a LAN client of this router");
    }
    Ok(())
}

fn router_lan_ip() -> Result<Ipv4Addr, String> {
    let out = Command::new("ip").args(["-4", "-o", "addr", "show", "br-lan"]).output()
        .map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .skip_while(|w| *w != "inet")
        .nth(1)
        .and_then(|cidr| cidr.split('/').next()?.parse().ok())
        .ok_or_else(|| "br-lan has no IPv4 address".into())
}

impl VoiceBridge {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stream the active call to `peer`. A bridge already streaming to `peer` is
    /// left alone unless `restart`: a second start (the app's status poll and its
    /// audio activation can both ask) used to kill and respawn it mid-call.
    pub fn start(&self, peer: Ipv4Addr, restart: bool) -> Result<(), String> {
        let router = router_lan_ip()?;
        validate_peer(peer, router)?;
        let original = std::fs::read(VENDOR_LIB).map_err(|e| format!("read vendor lib: {e}"))?;
        let patched = patch_library(&original, router, peer)?;
        let mut inner = self.lock();
        inner.reap();
        if !restart && inner.child.as_ref().is_some_and(|(_, running)| *running == peer) {
            return Ok(());
        }
        if let Some((old, _)) = inner.child.take() {
            terminate(old);
        }
        std::fs::create_dir_all(WORK_DIR).map_err(|e| e.to_string())?;
        // A new file renamed into place: a tool still mapping the old copy is untouched.
        let staged = format!("{WORK_DIR}/libztertp.so.new");
        std::fs::write(&staged, patched).map_err(|e| e.to_string())?;
        std::fs::rename(&staged, format!("{WORK_DIR}/libztertp.so")).map_err(|e| e.to_string())?;
        let child = Command::new(VENDOR_TOOL)
            .env("LD_LIBRARY_PATH", WORK_DIR)
            .args(["-i", "11C05000", "-d", "2,2147483664", "-p", "dummy", "-r", "dummy", "-t", "0",
                   "-l", &MAX_SECONDS.to_string()])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start bridge: {e}"))?;
        inner.child = Some((child, peer));
        inner.last_exit = None;
        Ok(())
    }

    /// Idempotent; safe to call when nothing runs. Returns at once: the tool is
    /// signalled and reaped on its own thread, so a hang-up never waits for it.
    pub fn stop(&self) {
        let taken = self.lock().child.take();
        if let Some((child, _)) = taken {
            std::thread::spawn(move || terminate(child));
        }
    }

    pub fn status(&self) -> Value {
        let mut inner = self.lock();
        inner.reap();
        match (&inner.child, &inner.last_exit) {
            (Some((_, peer)), _) => json!({"running": true, "host": peer.to_string(), "port": 48894}),
            (None, Some(exit)) => json!({"running": false, "exited": exit}),
            (None, None) => json!({"running": false}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_lib() -> Vec<u8> {
        let mut lib = b"\x7fELF....\0".to_vec();
        lib.extend_from_slice(b"192.168.253.1\0");
        lib.extend_from_slice(b"pad\0");
        lib.extend_from_slice(b"192.168.253.100\0tail");
        lib
    }

    #[test]
    fn patches_both_addresses_without_changing_size() {
        let lib = fake_lib();
        let out = patch_library(&lib, Ipv4Addr::new(192, 168, 0, 1), Ipv4Addr::new(192, 168, 0, 165)).unwrap();
        assert_eq!(out.len(), lib.len());
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("192.168.0.1\0\0\0"));
        assert!(text.contains("192.168.0.165\0\0\0"));
        assert!(!text.contains("192.168.253"));
        assert!(text.ends_with("tail"));
    }

    #[test]
    fn router_default_is_not_matched_inside_peer_default() {
        // "192.168.253.1" is a prefix of "192.168.253.100"; only exact NUL-bounded strings count.
        let lib = fake_lib();
        let out = patch_library(&lib, Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2)).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("10.0.0.2\0"));
    }

    #[test]
    fn rejects_unexpected_layout() {
        assert!(patch_library(b"nothing here", Ipv4Addr::LOCALHOST, Ipv4Addr::LOCALHOST).is_err());
        let mut twice = fake_lib();
        twice.extend_from_slice(b"\0192.168.253.1\0");
        assert!(patch_library(&twice, Ipv4Addr::new(192, 168, 0, 1), Ipv4Addr::new(192, 168, 0, 9)).is_err());
    }

    #[test]
    fn terminate_stops_a_tool_that_honours_sigint_at_once() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let started = Instant::now();
        terminate(child);
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    }

    #[test]
    fn terminate_escalates_to_sigkill() {
        // Ignores SIGINT and SIGTERM; only SIGKILL ends it.
        let child = Command::new("sh").args(["-c", "trap '' INT TERM; while :; do sleep 0.2; done"])
            .spawn().unwrap();
        std::thread::sleep(Duration::from_millis(100)); // let the traps install
        let started = Instant::now();
        terminate(child);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(1000) && took < Duration::from_millis(2500), "{took:?}");
    }

    #[test]
    fn exited_bridge_reports_how_it_ended() {
        let bridge = VoiceBridge::default();
        let child = Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        bridge.lock().child = Some((child, Ipv4Addr::new(192, 168, 0, 9)));
        std::thread::sleep(Duration::from_millis(200));
        let status = bridge.status();
        assert_eq!(status["running"], false);
        assert!(status["exited"].as_str().unwrap().contains('3'), "{status}");
        bridge.stop(); // nothing left to stop
    }

    #[test]
    fn peer_must_be_lan_client() {
        let r = Ipv4Addr::new(192, 168, 0, 1);
        assert!(validate_peer(Ipv4Addr::new(192, 168, 0, 165), r).is_ok());
        for bad in [r, Ipv4Addr::new(192, 168, 0, 0), Ipv4Addr::new(192, 168, 0, 255),
                    Ipv4Addr::new(192, 168, 1, 5), Ipv4Addr::new(8, 8, 8, 8)] {
            assert!(validate_peer(bad, r).is_err(), "{bad}");
        }
    }
}
