use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const PORTS: &[&str] = &[
    "/dev/at_mdm0",
    "/dev/at_mdm1",
    "/dev/at_usb0",
    "/dev/smd7",
    "/dev/smd11",
];

/// Serialises every AT exchange process-wide. The modem node is a packet
/// device with a single receive queue shared by all opens, so two concurrent
/// `cat` readers split (or steal) each other's reply; HTTP workers, the AT
/// terminal, the policy threads and `busy_check` all go through here.
static AT_LOCK: Mutex<()> = Mutex::new(());

fn lock_port() -> MutexGuard<'static, ()> {
    AT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct AtPort {
    cached: Mutex<Option<String>>,
}

impl AtPort {
    pub fn new() -> Self {
        Self {
            cached: Mutex::new(None),
        }
    }

    pub fn detect(&self) -> Option<String> {
        if let Some(port) = self.cached() {
            return Some(port);
        }
        let _g = lock_port();
        self.probe()
    }

    fn cached(&self) -> Option<String> {
        self.cached.lock().unwrap().clone()
    }

    /// Probe the candidate ports with a bare `AT`. Caller must hold `AT_LOCK`.
    fn probe(&self) -> Option<String> {
        if let Some(port) = self.cached() {
            return Some(port);
        }

        for &port in PORTS {
            if !std::path::Path::new(port).exists() {
                continue;
            }
            let script = format!(
                "cat {p} & PID=$! ; sleep 0.3 ; echo -e 'AT\\r' > {p} ; sleep 1 ; kill $PID 2>/dev/null",
                p = port
            );
            let output = Command::new("sh").args(["-c", &script]).output().ok();
            if let Some(out) = output {
                let resp = String::from_utf8_lossy(&out.stdout);
                if resp.contains("OK") {
                    let mut cached = self.cached.lock().unwrap();
                    *cached = Some(port.to_string());
                    return Some(port.to_string());
                }
            }
        }
        None
    }
}

/// Send an AT command and return the raw response text.
///
/// Holds `AT_LOCK` for the whole exchange (port probe included) so no two
/// commands ever share the node's receive queue.
pub fn send(at_port: &AtPort, command: &str, timeout_secs: u64) -> Result<String, String> {
    crate::httpd::gate::debug_assert_may_block();
    let _g = lock_port();
    let port = at_port.probe().ok_or("no serial port found")?;
    let script = format!(
        "cat {p} & PID=$! ; sleep 0.3 ; echo -e '{cmd}\\r' > {p} ; sleep {t} ; kill $PID 2>/dev/null",
        p = port,
        cmd = command,
        t = timeout_secs
    );
    let output = Command::new("sh")
        .args(["-c", &script])
        .output()
        .map_err(|e| format!("failed to open port: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Like [`send`], but returns as soon as the modem's final result code arrives;
/// `timeout_secs` is only the upper bound. `send` always waits out its timeout,
/// which made every call action take seconds (`ATD` 5 s, `ATA`/`AT+CHUP` 3 s,
/// each `AT+CLCC` poll 2 s) and queued the next one behind it on `AT_LOCK`.
///
/// Only for commands whose whole reply ends with that code (call control,
/// `AT+CLCC`). Never for ones answered by a later unsolicited result, such as
/// USSD's `+CUSD`: those keep using `send`.
pub fn send_until_final(at_port: &AtPort, command: &str, timeout_secs: u64) -> Result<String, String> {
    crate::httpd::gate::debug_assert_may_block();
    let _g = lock_port();
    let port = at_port.probe().ok_or("no serial port found")?;
    exchange(Path::new(&port), command, Duration::from_secs(timeout_secs), OPEN_WAIT)
}

/// Longest wait for the reader to open the node before the command is written
/// (the fixed delay `send` uses).
const OPEN_WAIT: Duration = Duration::from_millis(300);

/// Whether `reply` holds the final result code for `command`.
/// `NO CARRIER`, `BUSY`, `NO ANSWER` and `NO DIALTONE` finish only `ATD`/`ATA`:
/// during anything else they are a call ending, not this command's reply.
pub(crate) fn is_final_reply(command: &str, reply: &str) -> bool {
    let upper = command.trim().to_ascii_uppercase();
    let call_setup = upper.starts_with("ATD") || upper == "ATA";
    reply.lines().map(str::trim).any(|line| {
        line == "OK"
            || line == "ERROR"
            || line.starts_with("+CME ERROR")
            || line.starts_with("+CMS ERROR")
            || (call_setup && matches!(line, "NO CARRIER" | "BUSY" | "NO ANSWER" | "NO DIALTONE"))
    })
}

/// The same device access as `send` (a `cat` reader, then the command written
/// to the node) without the fixed sleeps: the command goes out once the reader
/// holds the node open, and the reader is stopped at the final result code.
fn exchange(port: &Path, command: &str, timeout: Duration, open_wait: Duration) -> Result<String, String> {
    let mut reader = Command::new("cat")
        .arg(port)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to open port: {e}"))?;
    let mut stdout = reader.stdout.take().ok_or("failed to open port")?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let pump = std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    wait_until_open(reader.id(), port, open_wait);
    // `echo -e 'CMD\r'` wrote CMD, CR and a newline; keep the same bytes.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .open(port)
        .and_then(|mut node| node.write_all(format!("{command}\r\n").as_bytes()));

    let mut reply = Vec::new();
    if written.is_ok() {
        let deadline = Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(chunk) => {
                    reply.extend_from_slice(&chunk);
                    if is_final_reply(command, &String::from_utf8_lossy(&reply)) {
                        break;
                    }
                }
                Err(_) => break, // timed out, or the reader exited
            }
        }
    }
    let _ = reader.kill();
    let _ = reader.wait();
    let _ = pump.join();
    written.map_err(|e| format!("failed to write port: {e}"))?;
    Ok(String::from_utf8_lossy(&reply).into_owned())
}

/// Wait until process `pid` has `port` open (seen in `/proc/<pid>/fd`), plus a
/// moment to enter its first read; at most `max`. Without `/proc` this is the
/// plain `max` delay `send` uses.
fn wait_until_open(pid: u32, port: &Path, max: Duration) {
    let target: PathBuf = std::fs::canonicalize(port).unwrap_or_else(|_| port.to_path_buf());
    let fds = format!("/proc/{pid}/fd");
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        let open = std::fs::read_dir(&fds).is_ok_and(|entries| {
            entries.flatten().any(|e| std::fs::read_link(e.path()).is_ok_and(|t| t == target))
        });
        if open {
            std::thread::sleep(Duration::from_millis(20));
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests run in parallel and another test may be inside `send` for a
    /// few microseconds, so poll rather than demand the lock is free *now*.
    fn lock_is_free() -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if AT_LOCK.try_lock().is_ok() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// On a dev box none of `PORTS` exist, so `send` fails in `probe()` via
    /// the `?` early return; the lock guard must still be dropped so the next
    /// exchange (and a competing thread) can proceed.
    #[test]
    fn send_releases_lock_on_early_return() {
        let port = AtPort::new();
        assert!(send(&port, "AT", 1).is_err());
        assert!(lock_is_free(), "AT_LOCK still held after send");
        assert!(send(&port, "AT", 1).is_err());
        assert!(port.detect().is_none());
        assert!(lock_is_free(), "AT_LOCK still held after detect");
    }

    #[test]
    fn final_reply_codes() {
        let clcc = "AT+CLCC\r\r\n+CLCC: 1,1,4,0,0,\"10086\",129\r\n";
        assert!(!is_final_reply("AT+CLCC", clcc));
        assert!(is_final_reply("AT+CLCC", &format!("{clcc}\r\nOK\r\n")));
        assert!(is_final_reply("AT+CMUT=1", "\r\n+CME ERROR: 3\r\n"));
        assert!(is_final_reply("AT+CHUP", "\r\nERROR\r\n"));
        assert!(is_final_reply("ATA", "\r\nNO CARRIER\r\n"));
        assert!(is_final_reply("ATD10086;", "\r\nBUSY\r\n"));
        // A call ending while the status is read is not the status reply.
        assert!(!is_final_reply("AT+CLCC", "\r\nNO CARRIER\r\n"));
        assert!(!is_final_reply("AT+CLCC", "\r\nRING\r\n\r\n+CLIP: \"10086\",129\r\n"));
        assert!(!is_final_reply("AT+CLCC", "OKAY\r\n"));
    }

    /// A FIFO stands in for the modem node: the reader sees the command (as an
    /// echo) and then whatever the fake modem writes.
    #[cfg(unix)]
    fn fake_modem(name: &str, reply: &'static [u8], hold: Duration) -> (PathBuf, std::thread::JoinHandle<()>) {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("zte-at-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = dir.join("node");
        let path = std::ffi::CString::new(node.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let modem_node = node.clone();
        let modem = std::thread::spawn(move || {
            // Opens once the reader has; staying open keeps the reader from seeing EOF.
            let mut node = std::fs::OpenOptions::new().write(true).open(&modem_node).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            node.write_all(reply).unwrap();
            std::thread::sleep(hold);
        });
        (node, modem)
    }

    #[cfg(unix)]
    #[test]
    fn exchange_returns_at_the_final_code_not_the_timeout() {
        let (node, modem) = fake_modem("final", b"+CLCC: 1,1,4,0,0,\"10086\",129\r\n\r\nOK\r\n",
                                       Duration::from_secs(2));
        let started = Instant::now();
        let reply = exchange(&node, "AT+CLCC", Duration::from_secs(5), Duration::from_millis(100)).unwrap();
        let took = started.elapsed();
        assert!(reply.contains("AT+CLCC") && reply.contains("+CLCC: 1,1,4") && reply.contains("OK"), "{reply:?}");
        assert!(took < Duration::from_millis(1500), "took {took:?}");
        modem.join().unwrap();
        let _ = std::fs::remove_dir_all(node.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn exchange_gives_up_at_the_timeout() {
        let (node, modem) = fake_modem("timeout", b"\r\nRING\r\n", Duration::from_secs(2));
        let started = Instant::now();
        let reply = exchange(&node, "AT+CLCC", Duration::from_millis(800), Duration::from_millis(100)).unwrap();
        let took = started.elapsed();
        assert!(!reply.contains("OK"), "{reply:?}");
        assert!(took >= Duration::from_millis(800) && took < Duration::from_millis(1800), "took {took:?}");
        modem.join().unwrap();
        let _ = std::fs::remove_dir_all(node.parent().unwrap());
    }

    #[test]
    fn concurrent_senders_do_not_deadlock() {
        let port = std::sync::Arc::new(AtPort::new());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let port = std::sync::Arc::clone(&port);
                std::thread::spawn(move || send(&port, "AT", 1).is_err())
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap());
        }
        assert!(lock_is_free());
    }
}
