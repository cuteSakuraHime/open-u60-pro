//! Helpers shared by the features that own iptables chains, policy routes or
//! an extra uplink (Clash transparent proxy, Wi-Fi relay).
//!
//! Every feature keeps its rules in chains it created itself, so removing a
//! feature never touches fw3's or the firmware's own rules.

use std::io::Write;
use std::process::{Command, Stdio};

use ipnet::Ipv4Net;

pub const LAN_INTERFACE: &str = "br-lan";

pub fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success())
}

pub fn output(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("run {program}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{program} {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub fn run_required(program: &str, args: &[&str]) -> Result<(), String> {
    output(program, args).map(|_| ())
}

/// Run `program` with `input` on its standard input (`ipset restore`).
pub fn run_with_input(program: &str, args: &[&str], input: &[u8]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run {program}: {error}"))?;
    let written = child
        .stdin
        .take()
        .map_or(Ok(()), |mut stdin| stdin.write_all(input));
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for {program}: {error}"))?;
    if let Err(error) = written {
        return Err(format!("write to {program}: {error}"));
    }
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Create `chain` or empty it, so the caller can fill it from scratch.
pub fn fresh_chain(program: &str, table: &str, chain: &str) -> Result<(), String> {
    let action = if succeeds(program, &["-w", "2", "-t", table, "-S", chain]) {
        "-F"
    } else {
        "-N"
    };
    run_required(program, &["-w", "2", "-t", table, action, chain])
}

/// The `-D` arguments that undo each rule of `listing` (`iptables -S parent`)
/// that jumps to `chain`, whatever matches the rule was installed with.
pub fn jump_deletions(listing: &str, chain: &str) -> Vec<Vec<String>> {
    listing
        .lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let jumps = words
                .windows(2)
                .any(|pair| pair[0] == "-j" && pair[1] == chain);
            (words.first() == Some(&"-A") && jumps).then(|| {
                std::iter::once("-D")
                    .chain(words[1..].iter().copied())
                    .map(str::to_owned)
                    .collect()
            })
        })
        .collect()
}

pub fn jump_installed(program: &str, table: &str, parent: &str, chain: &str) -> bool {
    output(program, &["-w", "2", "-t", table, "-S", parent])
        .is_ok_and(|listing| !jump_deletions(&listing, chain).is_empty())
}

/// Remove every jump from `parent` to `chain`, then the chain itself. A chain
/// that does not exist is not an error.
pub fn drop_chain(program: &str, table: &str, parent: &str, chain: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Ok(listing) = output(program, &["-w", "2", "-t", table, "-S", parent]) {
        for deletion in jump_deletions(&listing, chain) {
            let mut args = vec!["-w", "2", "-t", table];
            args.extend(deletion.iter().map(String::as_str));
            if let Err(error) = run_required(program, &args) {
                errors.push(error);
            }
        }
    }
    if succeeds(program, &["-w", "2", "-t", table, "-S", chain]) {
        for action in ["-F", "-X"] {
            if let Err(error) = run_required(program, &["-w", "2", "-t", table, action, chain]) {
                errors.push(error);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// The first IPv4 network in `ip -4 -o addr show dev <interface>` output.
pub fn parse_interface_network(listing: &str) -> Option<Ipv4Net> {
    listing.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        words.find(|word| *word == "inet")?;
        words.next()?.parse().ok()
    })
}

pub fn interface_network(interface: &str) -> Result<Ipv4Net, String> {
    let listing = output("ip", &["-4", "-o", "addr", "show", "dev", interface])?;
    parse_interface_network(&listing).ok_or_else(|| format!("{interface} has no IPv4 address"))
}

pub fn lan_network() -> Result<Ipv4Net, String> {
    interface_network(LAN_INTERFACE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jump_deletions_follow_the_installed_rule() {
        let listing = "-P PREROUTING ACCEPT\n-A PREROUTING -i br-lan -p tcp -j OPENU60_CLASH\n-A PREROUTING -j OPENU60_CLASH_DNS\n-A PREROUTING -i br-lan -j zone_lan_prerouting\n";
        assert_eq!(
            jump_deletions(listing, "OPENU60_CLASH"),
            vec![vec![
                "-D",
                "PREROUTING",
                "-i",
                "br-lan",
                "-p",
                "tcp",
                "-j",
                "OPENU60_CLASH"
            ]]
        );
        assert!(jump_deletions(listing, "OPENU60_RELAY").is_empty());
    }

    #[test]
    fn interface_network_reads_the_first_address() {
        let listing = "12: br-lan    inet 192.168.0.1/24 brd 192.168.0.255 scope global br-lan\\       valid_lft forever preferred_lft forever\n";
        let network = parse_interface_network(listing).unwrap();
        assert_eq!(network.addr().to_string(), "192.168.0.1");
        assert_eq!(network.prefix_len(), 24);
        assert!(parse_interface_network("").is_none());
    }
}
