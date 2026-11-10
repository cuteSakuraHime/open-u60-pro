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
use std::sync::Mutex;

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
    child: Mutex<Option<(Child, Ipv4Addr)>>,
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
    pub fn start(&self, peer: Ipv4Addr) -> Result<(), String> {
        let router = router_lan_ip()?;
        validate_peer(peer, router)?;
        let original = std::fs::read(VENDOR_LIB).map_err(|e| format!("read vendor lib: {e}"))?;
        let patched = patch_library(&original, router, peer)?;
        std::fs::create_dir_all(WORK_DIR).map_err(|e| e.to_string())?;
        std::fs::write(format!("{WORK_DIR}/libztertp.so"), patched).map_err(|e| e.to_string())?;
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((mut old, _)) = guard.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        let child = Command::new(VENDOR_TOOL)
            .env("LD_LIBRARY_PATH", WORK_DIR)
            .args(["-i", "11C05000", "-d", "2,2147483664", "-p", "dummy", "-r", "dummy", "-t", "0",
                   "-l", &MAX_SECONDS.to_string()])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start bridge: {e}"))?;
        *guard = Some((child, peer));
        Ok(())
    }

    /// Idempotent; safe to call when nothing runs.
    pub fn stop(&self) {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((mut child, _)) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn status(&self) -> Value {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((child, _)) = guard.as_mut() {
            if !matches!(child.try_wait(), Ok(None)) {
                *guard = None;
            }
        }
        match guard.as_ref() {
            Some((_, peer)) => json!({"running": true, "host": peer.to_string(), "port": 48894}),
            None => json!({"running": false}),
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
    fn peer_must_be_lan_client() {
        let r = Ipv4Addr::new(192, 168, 0, 1);
        assert!(validate_peer(Ipv4Addr::new(192, 168, 0, 165), r).is_ok());
        for bad in [r, Ipv4Addr::new(192, 168, 0, 0), Ipv4Addr::new(192, 168, 0, 255),
                    Ipv4Addr::new(192, 168, 1, 5), Ipv4Addr::new(8, 8, 8, 8)] {
            assert!(validate_peer(bad, r).is_err(), "{bad}");
        }
    }
}
