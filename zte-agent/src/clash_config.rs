//! The Mihomo configuration the router actually runs.
//!
//! The imported YAML is kept as the user wrote it. What Mihomo reads is a
//! runtime copy in which the settings the router must own are replaced:
//! listeners, the controller, its secret and the dashboard it serves,
//! logging, and anything that would let Mihomo rewrite the router's own
//! routes or firewall (TUN, its iptables helper). Subscription files
//! routinely set those to values that clash with the agent (a controller on
//! port 9090, for one), so they are dropped rather than rejected.
//!
//! No YAML parser is linked. Top-level keys start in column 0, so the file is
//! handled as a list of top-level blocks; Mihomo itself validates the result
//! before it is used.

use std::net::Ipv4Addr;

pub const MIXED_PORT: u16 = 7890;
pub const REDIR_PORT: u16 = 7892;
/// The TUN device that carries LAN clients' UDP while it is intercepted.
pub const TUN_DEVICE: &str = "ouclash0";
pub const DNS_PORT: u16 = 1053;
pub const CONTROLLER_PORT: u16 = 9091;
pub const CONTROLLER: &str = "127.0.0.1:9091";

/// Used when the imported file has no `ntp` section. Mihomo then measures
/// its own offset from real time and leaves the system clock alone. This
/// firmware keeps local time in a clock it labels UTC, so the clock is hours
/// away from real time, and a Shadowsocks 2022 server refuses a handshake
/// whose timestamp is more than 30 seconds off.
const DEFAULT_NTP: &str = "ntp:\n  enable: true\n  write-to-system: false\n  server: ntp.aliyun.com\n  port: 123\n  interval: 30\n";

/// Top-level keys the runtime copy never takes from the imported file.
const OWNED: &[&str] = &[
    "port",
    "socks-port",
    "mixed-port",
    "redir-port",
    "tproxy-port",
    "allow-lan",
    "bind-address",
    "external-controller",
    "external-controller-tls",
    "external-controller-unix",
    "external-controller-pipe",
    "external-controller-cors",
    "external-doh-server",
    "external-ui",
    "external-ui-name",
    "external-ui-url",
    "secret",
    "log-level",
    "tun",
    "iptables",
    "ebpf",
];

/// Keys that mark a file as a Clash configuration at all.
const RECOGNISED: &[&str] = &[
    "proxies",
    "proxy-providers",
    "proxy-groups",
    "rules",
    "rule-providers",
    "mixed-port",
    "port",
    "mode",
];

pub struct RuntimeOptions<'a> {
    /// Guards the loopback controller; unused while a dashboard is served.
    pub secret: &'a str,
    pub dashboard: Option<Dashboard<'a>>,
    pub log_level: &'a str,
    pub dns_hijack: bool,
    /// Make the built-in resolver answer with real addresses, so that
    /// destination address lists in the firewall can match.
    pub real_addresses: bool,
    /// Create the TUN device that LAN clients' UDP is routed into.
    pub tun: bool,
    pub lan_address: Ipv4Addr,
}

/// A dashboard that Mihomo serves itself. The controller then listens on the
/// LAN address instead of loopback and asks for no secret, so anything that
/// can reach that address controls the core.
#[derive(Clone, Copy)]
pub struct Dashboard<'a> {
    pub address: Ipv4Addr,
    /// The directory with the dashboard's `index.html`.
    pub directory: &'a str,
}

/// Where the controller listens. Browsers are told to let only the
/// dashboard's own pages read the API: Mihomo's default allows every site.
fn controller(options: &RuntimeOptions) -> String {
    match options.dashboard {
        Some(Dashboard { address, directory }) => format!(
            "external-controller: {address}:{CONTROLLER_PORT}\nsecret: ''\nexternal-ui: '{directory}'\nexternal-controller-cors:\n  allow-origins:\n    - http://{address}:{CONTROLLER_PORT}\n  allow-private-network: false\n"
        ),
        None => format!(
            "external-controller: {CONTROLLER}\nsecret: '{}'\n",
            options.secret
        ),
    }
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Summary {
    pub proxies: usize,
    pub proxy_providers: usize,
    pub proxy_groups: usize,
    pub rules: usize,
    pub has_dns: bool,
}

struct Block<'a> {
    key: Option<&'a str>,
    lines: Vec<&'a str>,
}

/// The key of a line that opens a top-level block, if it is one.
fn top_level_key(line: &str) -> Option<&str> {
    let first = line.chars().next()?;
    if first.is_whitespace() || matches!(first, '#' | '-' | '[' | '{' | '.') {
        return None;
    }
    let (key, rest) = line.split_once(':')?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let key = key.trim().trim_matches(['\'', '"']);
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    plain.then_some(key)
}

fn blocks(text: &str) -> Vec<Block<'_>> {
    let mut blocks: Vec<Block<'_>> = Vec::new();
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match (top_level_key(line), blocks.last_mut()) {
            (None, Some(block)) => block.lines.push(line),
            (key, _) => blocks.push(Block {
                key,
                lines: vec![line],
            }),
        }
    }
    blocks
}

fn without_comment(text: &str) -> &str {
    let text = text.trim();
    if text.starts_with('#') {
        return "";
    }
    text.split(" #").next().unwrap_or("").trim()
}

fn is_content(line: &str) -> bool {
    let trimmed = line.trim_start();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// The `dns` block with its listener fixed to the port the router redirects
/// to (and switched on when DNS is intercepted). `None`: the block is empty.
fn rewrite_dns(lines: &[&str], hijack: bool) -> Result<Option<Vec<String>>, String> {
    let head = lines[0];
    let inline = head
        .split_once(':')
        .map_or("", |(_, rest)| without_comment(rest));
    let children: Vec<&str> = lines[1..]
        .iter()
        .copied()
        .filter(|l| is_content(l))
        .collect();
    let indent = children.first().map_or("", |line| indent_of(line));
    if !inline.is_empty() || (indent.is_empty() && !children.is_empty()) {
        return if hijack {
            Err("write the dns section as an indented block to use DNS interception".into())
        } else {
            Ok(Some(lines.iter().map(|line| (*line).to_owned()).collect()))
        };
    }
    if children.is_empty() {
        return Ok(None);
    }
    let mut out = vec![head.to_owned(), format!("{indent}listen: :{DNS_PORT}")];
    if hijack {
        out.push(format!("{indent}enable: true"));
    }
    for line in &lines[1..] {
        let replaced = is_content(line)
            && indent_of(line) == indent
            && line.trim_start().split_once(':').is_some_and(|(key, _)| {
                let key = key.trim().trim_matches(['\'', '"']);
                key == "listen" || (hijack && key == "enable")
            });
        if !replaced {
            out.push((*line).to_owned());
        }
    }
    Ok(Some(out))
}

/// Used when DNS is intercepted and the imported file has no `dns` section.
/// Names are resolved through the router's own resolver so the DNS & Privacy
/// settings still apply. Normally every name except local ones gets a fake
/// address; with `real_addresses` clients get the real one and Mihomo maps
/// it back to the name, which is what lets the mainland China bypass see
/// where a connection is really going.
fn default_dns(lan_address: Ipv4Addr, real_addresses: bool) -> String {
    if real_addresses {
        return format!(
            "dns:\n  enable: true\n  listen: :{DNS_PORT}\n  ipv6: false\n  enhanced-mode: redir-host\n  nameserver:\n    - {lan_address}\n"
        );
    }
    format!(
        "dns:\n  enable: true\n  listen: :{DNS_PORT}\n  ipv6: false\n  enhanced-mode: fake-ip\n  fake-ip-range: 198.18.0.1/16\n  fake-ip-filter:\n    - '*.lan'\n    - '*.local'\n    - '+.home.arpa'\n    - '+.msftconnecttest.com'\n    - '+.msftncsi.com'\n    - '+.pool.ntp.org'\n    - 'time.*.com'\n  nameserver:\n    - {lan_address}\n"
    )
}

/// The runtime configuration for `user`.
pub fn compose(user: &str, options: &RuntimeOptions) -> Result<String, String> {
    let text = user.strip_prefix('\u{feff}').unwrap_or(user);
    let mut out = String::with_capacity(text.len() + 1024);
    let mut recognised = false;
    let mut has_dns = false;
    let mut has_ntp = false;
    for block in blocks(text) {
        recognised |= block.key.is_some_and(|key| RECOGNISED.contains(&key));
        has_ntp |= block.key == Some("ntp");
        match block.key {
            Some(key) if OWNED.contains(&key) => {}
            Some("dns") => {
                if let Some(lines) = rewrite_dns(&block.lines, options.dns_hijack)? {
                    has_dns = true;
                    for line in lines {
                        out.push_str(&line);
                        out.push('\n');
                    }
                }
            }
            _ => {
                for line in block.lines {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    if !recognised {
        return Err("this is not a Clash configuration (no proxies, proxy-groups or rules)".into());
    }
    if options.dns_hijack && !has_dns {
        out.push_str(&default_dns(options.lan_address, options.real_addresses));
    }
    if !has_ntp {
        out.push_str(DEFAULT_NTP);
    }
    if options.tun {
        // A plain device: the agent installs the one route and the rules that
        // use it, so Mihomo must not touch the router's routing or firewall.
        out.push_str(&format!(
            "tun:\n  enable: true\n  device: {TUN_DEVICE}\n  stack: gvisor\n  auto-route: false\n  auto-redirect: false\n  auto-detect-interface: false\n  strict-route: false\n  dns-hijack: []\n  mtu: 1500\n"
        ));
    }
    out.push_str(&format!(
        "# Managed by OpenU60: listeners, controller and logging.\nmixed-port: {MIXED_PORT}\nredir-port: {REDIR_PORT}\nallow-lan: true\nbind-address: '*'\n{}log-level: {}\n",
        controller(options),
        options.log_level
    ));
    Ok(out)
}

/// Entries of a block sequence: the `- ` lines at the sequence's own indent.
fn sequence_items(lines: &[&str]) -> usize {
    let items: Vec<&str> = lines[1..]
        .iter()
        .copied()
        .filter(|l| is_content(l))
        .collect();
    let Some(indent) = items.first().map(|line| indent_of(line)) else {
        return 0;
    };
    items
        .iter()
        .filter(|line| indent_of(line) == indent && line.trim_start().starts_with('-'))
        .count()
}

/// Keys of a block mapping: the lines at the mapping's own indent.
fn mapping_keys(lines: &[&str]) -> usize {
    let items: Vec<&str> = lines[1..]
        .iter()
        .copied()
        .filter(|l| is_content(l))
        .collect();
    let Some(indent) = items.first().map(|line| indent_of(line)) else {
        return 0;
    };
    items
        .iter()
        .filter(|line| indent_of(line) == indent)
        .count()
}

/// What the imported file contains, without returning any of it: the file
/// holds proxy credentials and is never sent back to a browser.
pub fn summarize(user: &str) -> Summary {
    let mut summary = Summary::default();
    for block in blocks(user.strip_prefix('\u{feff}').unwrap_or(user)) {
        match block.key {
            Some("proxies") => summary.proxies = sequence_items(&block.lines),
            Some("proxy-groups") => summary.proxy_groups = sequence_items(&block.lines),
            Some("rules") => summary.rules = sequence_items(&block.lines),
            Some("proxy-providers") => summary.proxy_providers = mapping_keys(&block.lines),
            Some("dns") => summary.has_dns = block.lines[1..].iter().any(|l| is_content(l)),
            _ => {}
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 1);

    fn options(dns_hijack: bool) -> RuntimeOptions<'static> {
        RuntimeOptions {
            secret: "controller-fixture",
            dashboard: None,
            log_level: "warning",
            dns_hijack,
            real_addresses: false,
            tun: false,
            lan_address: LAN,
        }
    }

    const SUBSCRIPTION: &str = "port: 7890\r\nsocks-port: 7891\r\nallow-lan: false\r\nmode: rule\r\nlog-level: info\r\nexternal-controller: 0.0.0.0:9090\r\nsecret: \"from-the-provider\"\r\ntun:\r\n  enable: true\r\n  auto-route: true\r\ndns:\r\n  enable: false\r\n  listen: 0.0.0.0:53\r\n  nameserver:\r\n    - 223.5.5.5\r\n  fallback-filter:\r\n    geoip: true\r\n    listen: kept-because-it-is-nested\r\nproxies:\r\n  - {name: one, type: ss, server: a.example, port: 443, cipher: aes-128-gcm, password: p}\r\n  - name: two\r\n    type: trojan\r\n    server: b.example\r\n    port: 443\r\n    password: q\r\nproxy-groups:\r\n- name: Auto\r\n  type: select\r\n  proxies: [one, two]\r\nrules:\r\n- DOMAIN-SUFFIX,example.com,Auto\r\n- MATCH,DIRECT\r\n";

    fn top_level(text: &str, key: &str) -> Vec<String> {
        text.lines()
            .filter(|line| top_level_key(line) == Some(key))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn router_owned_settings_replace_the_imported_ones() {
        let runtime = compose(SUBSCRIPTION, &options(false)).unwrap();
        assert!(!runtime.contains('\r'));
        for key in ["port", "socks-port", "tun"] {
            assert!(top_level(&runtime, key).is_empty(), "{key} must be dropped");
        }
        assert!(!runtime.contains("auto-route"));
        assert!(!runtime.contains("from-the-provider"));
        assert_eq!(top_level(&runtime, "mixed-port"), ["mixed-port: 7890"]);
        assert_eq!(top_level(&runtime, "allow-lan"), ["allow-lan: true"]);
        assert_eq!(
            top_level(&runtime, "external-controller"),
            ["external-controller: 127.0.0.1:9091"]
        );
        assert_eq!(
            top_level(&runtime, "secret"),
            ["secret: 'controller-fixture'"]
        );
        assert_eq!(top_level(&runtime, "log-level"), ["log-level: warning"]);
        assert_eq!(top_level(&runtime, "mode"), ["mode: rule"]);
        assert!(runtime.contains("- DOMAIN-SUFFIX,example.com,Auto\n- MATCH,DIRECT\n"));
        assert!(runtime.contains("    password: q\n"));
    }

    #[test]
    fn mihomo_keeps_its_own_clock_unless_the_file_sets_one() {
        let runtime = compose(SUBSCRIPTION, &options(false)).unwrap();
        assert_eq!(top_level(&runtime, "ntp").len(), 1);
        // The default only corrects Mihomo: the router's clock is the firmware's.
        assert!(runtime.contains("ntp:\n  enable: true\n  write-to-system: false\n"));

        let own = format!("{SUBSCRIPTION}ntp:\n  enable: false\n");
        let kept = compose(&own, &options(false)).unwrap();
        assert_eq!(top_level(&kept, "ntp").len(), 1);
        assert!(kept.contains("ntp:\n  enable: false\n"));
        assert!(!kept.contains("write-to-system"));
    }

    #[test]
    fn a_dashboard_moves_the_controller_to_the_lan_address_without_a_secret() {
        let imported = format!(
            "{SUBSCRIPTION}external-ui: /etc\nexternal-ui-url: https://example.com/ui.zip\nexternal-controller-cors:\n  allow-origins:\n    - '*'\n"
        );
        let closed = compose(&imported, &options(false)).unwrap();
        for key in ["external-ui", "external-ui-url", "external-controller-cors"] {
            assert!(top_level(&closed, key).is_empty(), "{key} must be dropped");
        }

        let open = compose(
            &imported,
            &RuntimeOptions {
                dashboard: Some(Dashboard {
                    address: LAN,
                    directory: "/data/open-u60/clash/ui",
                }),
                ..options(false)
            },
        )
        .unwrap();
        assert_eq!(
            top_level(&open, "external-controller"),
            ["external-controller: 192.168.0.1:9091"]
        );
        assert_eq!(top_level(&open, "secret"), ["secret: ''"]);
        assert_eq!(
            top_level(&open, "external-ui"),
            ["external-ui: '/data/open-u60/clash/ui'"]
        );
        assert!(open.contains(
            "external-controller-cors:\n  allow-origins:\n    - http://192.168.0.1:9091\n  allow-private-network: false\n"
        ));
        // Nothing makes Mihomo download a dashboard, and no other address is opened.
        assert!(top_level(&open, "external-ui-url").is_empty());
        assert!(!open.contains("0.0.0.0"));
        assert!(!open.contains("controller-fixture"));
        assert!(!open.contains("example.com/ui.zip"));
    }

    #[test]
    fn dns_listener_moves_to_the_redirect_port() {
        let plain = compose(SUBSCRIPTION, &options(false)).unwrap();
        assert!(plain.contains("dns:\n  listen: :1053\n  enable: false\n"));
        assert!(!plain.contains("0.0.0.0:53\n"));
        assert!(plain.contains("    listen: kept-because-it-is-nested\n"));

        let hijacked = compose(SUBSCRIPTION, &options(true)).unwrap();
        assert!(hijacked.contains("dns:\n  listen: :1053\n  enable: true\n  nameserver:\n"));
        assert!(!hijacked.contains("enable: false"));
        assert_eq!(top_level(&hijacked, "dns").len(), 1);
    }

    #[test]
    fn dns_interception_supplies_a_resolver_when_the_file_has_none() {
        let minimal = "mixed-port: 7890\nrules:\n  - MATCH,DIRECT\n";
        assert!(top_level(&compose(minimal, &options(false)).unwrap(), "dns").is_empty());
        let hijacked = compose(minimal, &options(true)).unwrap();
        assert_eq!(top_level(&hijacked, "dns").len(), 1);
        assert!(hijacked.contains("  listen: :1053\n"));
        assert!(hijacked.contains("  nameserver:\n    - 192.168.0.1\n"));
        assert!(hijacked.contains("  enhanced-mode: fake-ip\n"));

        let real = compose(
            minimal,
            &RuntimeOptions {
                real_addresses: true,
                ..options(true)
            },
        )
        .unwrap();
        assert_eq!(top_level(&real, "dns").len(), 1);
        assert!(real.contains("  enhanced-mode: redir-host\n"));
        assert!(real.contains("  nameserver:\n    - 192.168.0.1\n"));
        assert!(!real.contains("fake-ip"));

        let own = "dns:\n  enable: true\n  enhanced-mode: fake-ip\nrules:\n  - MATCH,DIRECT\n";
        let kept = compose(
            own,
            &RuntimeOptions {
                real_addresses: true,
                ..options(true)
            },
        )
        .unwrap();
        assert!(kept.contains("  enhanced-mode: fake-ip\n"));
        assert!(!kept.contains("redir-host"));

        let empty = "dns:\n  # nothing yet\nrules:\n  - MATCH,DIRECT\n";
        assert_eq!(
            top_level(&compose(empty, &options(true)).unwrap(), "dns").len(),
            1
        );

        let flow = "dns: {enable: true, listen: ':53'}\nrules:\n  - MATCH,DIRECT\n";
        assert!(compose(flow, &options(true)).is_err());
        assert!(compose(flow, &options(false))
            .unwrap()
            .contains("dns: {enable: true"));
    }

    #[test]
    fn text_that_is_not_a_clash_file_is_refused() {
        assert!(compose("c3M6Ly9hYmNkZWY=\n", &options(false)).is_err());
        assert!(compose("", &options(false)).is_err());
        assert!(compose("\u{feff}mode: rule\n", &options(false)).is_ok());
    }

    #[test]
    fn the_udp_device_is_the_only_tun_section() {
        let plain = compose(SUBSCRIPTION, &options(true)).unwrap();
        assert!(top_level(&plain, "tun").is_empty());
        assert!(top_level(&plain, "tproxy-port").is_empty());

        let with_tun = compose(
            SUBSCRIPTION,
            &RuntimeOptions {
                tun: true,
                ..options(true)
            },
        )
        .unwrap();
        assert_eq!(top_level(&with_tun, "tun").len(), 1);
        assert!(with_tun.contains(
            "tun:\n  enable: true\n  device: ouclash0\n  stack: gvisor\n  auto-route: false\n"
        ));
        // The imported file's own TUN settings never survive.
        assert!(!with_tun.contains("auto-route: true"));
    }

    #[test]
    fn summary_counts_without_exposing_content() {
        let summary = summarize(SUBSCRIPTION);
        assert_eq!(
            summary,
            Summary {
                proxies: 2,
                proxy_providers: 0,
                proxy_groups: 1,
                rules: 2,
                has_dns: true,
            }
        );
        let providers = "proxy-providers:\n  first:\n    type: http\n    url: https://example.com/a\n  second:\n    type: file\n";
        assert_eq!(summarize(providers).proxy_providers, 2);
    }
}
