//! Transparent-proxy firewall state for Mihomo.
//!
//! LAN clients' TCP is redirected to Mihomo's redir port and their DNS to its
//! resolver. Their UDP is marked and policy-routed into Mihomo's TUN device:
//! on this firmware the bridge runs iptables itself
//! (`bridge-nf-call-iptables=1`) and UDP handed to TPROXY there never reaches
//! the proxy's socket, while routing into a TUN device does.
//!
//! Everything lives in chains, ipsets, one routing rule and one routing table
//! created here, so [`remove`] restores the firmware's own forwarding exactly.
//!
//! Fail-open by construction: every jump that diverts traffic matches only
//! while the guard ipset holds its two entries, and those entries expire
//! unless the agent keeps refreshing them ([`refresh_guard`]). If the agent
//! stops for any reason the rules stop matching within [`GUARD_TIMEOUT`]
//! seconds and the hotspot forwards directly again. The UDP route disappears
//! with the TUN device as soon as Mihomo exits.

use ipnet::Ipv4Net;

use crate::clash_config::{DNS_PORT, REDIR_PORT, TUN_DEVICE};
use crate::net_rules::{self, LAN_INTERFACE};

const CHAIN: &str = "OPENU60_CLASH";
const DNS_CHAIN: &str = "OPENU60_CLASH_DNS";
const GUARD_SET: &str = "openu60_clash_guard";
const GUARD_SET_V6: &str = "openu60_clash_guard6";
const LOCAL_SET: &str = "openu60_clash_local";
const CHINA_SET: &str = "openu60_clash_cn";
const UDP_MARK: &str = "0x2000000/0x6000000";
const ROUTE_TABLE: &str = "120";
const RULE_PRIORITY: &str = "990";
pub const GUARD_TIMEOUT: u32 = 180;

/// Destinations that are never proxied. 198.18.0.0/15 is left out on purpose:
/// it is where Mihomo's fake-ip answers live.
const RESERVED: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub udp: bool,
    pub dns_hijack: bool,
    pub bypass_china: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Rule {
    pub program: &'static str,
    pub table: &'static str,
    pub args: Vec<String>,
}

fn rule(program: &'static str, table: &'static str, args: &[&str]) -> Rule {
    Rule {
        program,
        table,
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
    }
}

/// The networks of a downloaded address list, one per line; anything that is
/// not an IPv4 network is skipped.
pub fn parse_networks(list: &str) -> Vec<Ipv4Net> {
    list.lines()
        .filter_map(|line| line.split('#').next()?.trim().parse::<Ipv4Net>().ok())
        .collect()
}

fn guard_entries() -> String {
    format!(
        "add {GUARD_SET} 0.0.0.0/1 timeout {GUARD_TIMEOUT}\nadd {GUARD_SET} 128.0.0.0/1 timeout {GUARD_TIMEOUT}\nadd {GUARD_SET_V6} ::/1 timeout {GUARD_TIMEOUT}\nadd {GUARD_SET_V6} 8000::/1 timeout {GUARD_TIMEOUT}\n"
    )
}

/// Input for `ipset -! restore`.
pub fn ipset_script(china: &[Ipv4Net]) -> String {
    let mut script = format!(
        "create {GUARD_SET} hash:net family inet timeout {GUARD_TIMEOUT}\ncreate {GUARD_SET_V6} hash:net family inet6 timeout {GUARD_TIMEOUT}\ncreate {LOCAL_SET} hash:net family inet\nflush {LOCAL_SET}\ncreate {CHINA_SET} hash:net family inet maxelem 65536\nflush {CHINA_SET}\n"
    );
    for network in RESERVED {
        script.push_str(&format!("add {LOCAL_SET} {network}\n"));
    }
    for network in china {
        script.push_str(&format!("add {CHINA_SET} {network}\n"));
    }
    script.push_str(&guard_entries());
    script
}

/// The chain contents and the jumps into them, in installation order.
pub fn rules(plan: Plan) -> Vec<Rule> {
    let redir = REDIR_PORT.to_string();
    let dns = DNS_PORT.to_string();
    let mut rules = Vec::new();
    let bypass = |table: &'static str| {
        let skip = |set| {
            rule(
                "iptables",
                table,
                &[
                    "-A",
                    CHAIN,
                    "-m",
                    "set",
                    "--match-set",
                    set,
                    "dst",
                    "-j",
                    "RETURN",
                ],
            )
        };
        let mut list = vec![skip(LOCAL_SET)];
        if plan.bypass_china {
            list.push(skip(CHINA_SET));
        }
        list
    };
    let guarded = |program: &'static str,
                   table: &'static str,
                   position: &[&str],
                   guard: &str,
                   rest: &[&str]| {
        let mut args = position.to_vec();
        args.extend(["-i", LAN_INTERFACE]);
        args.extend(rest);
        args.extend(["-m", "set", "--match-set", guard, "src"]);
        rule(program, table, &args)
    };
    let with_jump = |mut rule: Rule, chain: &str| {
        rule.args.extend(["-j".to_owned(), chain.to_owned()]);
        rule
    };

    rules.extend(bypass("nat"));
    rules.push(rule(
        "iptables",
        "nat",
        &[
            "-A",
            CHAIN,
            "-p",
            "tcp",
            "-j",
            "REDIRECT",
            "--to-ports",
            &redir,
        ],
    ));
    rules.push(with_jump(
        guarded(
            "iptables",
            "nat",
            &["-I", "PREROUTING", "1"],
            GUARD_SET,
            &["-p", "tcp"],
        ),
        CHAIN,
    ));

    if plan.udp {
        rules.extend(bypass("mangle"));
        // DNS is answered by the resolver, never carried as proxied UDP.
        rules.push(rule(
            "iptables",
            "mangle",
            &["-A", CHAIN, "-p", "udp", "--dport", "53", "-j", "RETURN"],
        ));
        rules.push(rule(
            "iptables",
            "mangle",
            &[
                "-A",
                CHAIN,
                "-p",
                "udp",
                "-j",
                "MARK",
                "--set-xmark",
                UDP_MARK,
            ],
        ));
        // Appended, so the firmware's own access-control chains still run first.
        rules.push(with_jump(
            guarded(
                "iptables",
                "mangle",
                &["-A", "PREROUTING"],
                GUARD_SET,
                &["-p", "udp"],
            ),
            CHAIN,
        ));
        for (from, to) in [(LAN_INTERFACE, TUN_DEVICE), (TUN_DEVICE, LAN_INTERFACE)] {
            rules.push(rule(
                "iptables",
                "filter",
                &["-A", CHAIN, "-i", from, "-o", to, "-j", "ACCEPT"],
            ));
        }
        rules.push(rule(
            "iptables",
            "filter",
            &["-I", "FORWARD", "1", "-j", CHAIN],
        ));
    }

    if plan.dns_hijack {
        for (program, guard) in [("iptables", GUARD_SET), ("ip6tables", GUARD_SET_V6)] {
            for protocol in ["udp", "tcp"] {
                rules.push(rule(
                    program,
                    "nat",
                    &[
                        "-A",
                        DNS_CHAIN,
                        "-p",
                        protocol,
                        "--dport",
                        "53",
                        "-j",
                        "REDIRECT",
                        "--to-ports",
                        &dns,
                    ],
                ));
            }
            // Inserted after the TCP jump, so DNS is matched before it.
            rules.push(with_jump(
                guarded(program, "nat", &["-I", "PREROUTING", "1"], guard, &[]),
                DNS_CHAIN,
            ));
        }
    }
    rules
}

/// One owned chain: (program, table, parent chain, chain).
type Owned = (&'static str, &'static str, &'static str, &'static str);

const OWNED_CHAINS: &[Owned] = &[
    ("iptables", "nat", "PREROUTING", CHAIN),
    ("iptables", "nat", "PREROUTING", DNS_CHAIN),
    ("iptables", "mangle", "PREROUTING", CHAIN),
    ("iptables", "filter", "FORWARD", CHAIN),
    ("ip6tables", "nat", "PREROUTING", DNS_CHAIN),
];

fn chains_for(plan: Plan) -> Vec<Owned> {
    OWNED_CHAINS
        .iter()
        .copied()
        .filter(|(_, table, _, chain)| match (*table, *chain) {
            ("nat", CHAIN) => true,
            (_, DNS_CHAIN) => plan.dns_hijack,
            _ => plan.udp,
        })
        .collect()
}

fn udp_route_installed() -> bool {
    net_rules::output("ip", &["rule", "show"]).is_ok_and(|rules| {
        rules.lines().any(|line| {
            line.contains("fwmark") && line.trim_end().ends_with(&format!("lookup {ROUTE_TABLE}"))
        })
    }) && net_rules::output("ip", &["route", "show", "table", ROUTE_TABLE])
        .is_ok_and(|routes| routes.contains(TUN_DEVICE))
}

/// Replace whatever is installed with the rules for `plan`. On any failure
/// everything is removed again, so a half-installed proxy cannot stay behind.
/// With `plan.udp` the TUN device must already exist.
pub fn install(plan: Plan, china: &[Ipv4Net]) -> Result<(), String> {
    remove();
    let result = (|| {
        net_rules::run_with_input("ipset", &["-!", "restore"], ipset_script(china).as_bytes())?;
        for (program, table, _, chain) in chains_for(plan) {
            net_rules::fresh_chain(program, table, chain)?;
        }
        if plan.udp {
            net_rules::run_required(
                "ip",
                &[
                    "route",
                    "replace",
                    "default",
                    "dev",
                    TUN_DEVICE,
                    "table",
                    ROUTE_TABLE,
                ],
            )?;
            net_rules::run_required(
                "ip",
                &[
                    "rule",
                    "add",
                    "fwmark",
                    UDP_MARK,
                    "lookup",
                    ROUTE_TABLE,
                    "pref",
                    RULE_PRIORITY,
                ],
            )?;
        }
        for rule in rules(plan) {
            let mut args = vec!["-w", "2", "-t", rule.table];
            args.extend(rule.args.iter().map(String::as_str));
            net_rules::run_required(rule.program, &args)?;
        }
        Ok(())
    })();
    if result.is_err() {
        remove();
    }
    result
}

/// Remove everything this module can have installed. Safe to call when
/// nothing is installed; the agent calls it at every start.
pub fn remove() {
    for (program, table, parent, chain) in OWNED_CHAINS {
        if let Err(error) = net_rules::drop_chain(program, table, parent, chain) {
            eprintln!("[clash] remove {table} {chain}: {error}");
        }
    }
    for _ in 0..4 {
        if !net_rules::succeeds(
            "ip",
            &["rule", "del", "fwmark", UDP_MARK, "lookup", ROUTE_TABLE],
        ) {
            break;
        }
    }
    let _ = net_rules::succeeds("ip", &["route", "flush", "table", ROUTE_TABLE]);
    for set in [GUARD_SET, GUARD_SET_V6, LOCAL_SET, CHINA_SET] {
        let _ = net_rules::succeeds("ipset", &["destroy", set]);
    }
}

/// Keep the rules armed for another [`GUARD_TIMEOUT`] seconds.
pub fn refresh_guard() -> Result<(), String> {
    net_rules::run_with_input("ipset", &["-!", "restore"], guard_entries().as_bytes())
}

/// Whether everything `plan` needs is still in place (a firewall restart by
/// the firmware removes the jumps).
pub fn intact(plan: Plan) -> bool {
    chains_for(plan)
        .into_iter()
        .all(|(program, table, parent, chain)| {
            net_rules::jump_installed(program, table, parent, chain)
        })
        && (!plan.udp || udp_route_installed())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Plan = Plan {
        udp: true,
        dns_hijack: true,
        bypass_china: true,
    };
    const MINIMAL: Plan = Plan {
        udp: false,
        dns_hijack: false,
        bypass_china: false,
    };

    fn lines(plan: Plan) -> Vec<String> {
        rules(plan)
            .into_iter()
            .map(|rule| format!("{} {} {}", rule.program, rule.table, rule.args.join(" ")))
            .collect()
    }

    #[test]
    fn minimal_plan_only_redirects_tcp() {
        assert_eq!(
            lines(MINIMAL),
            [
                "iptables nat -A OPENU60_CLASH -m set --match-set openu60_clash_local dst -j RETURN",
                "iptables nat -A OPENU60_CLASH -p tcp -j REDIRECT --to-ports 7892",
                "iptables nat -I PREROUTING 1 -i br-lan -p tcp -m set --match-set openu60_clash_guard src -j OPENU60_CLASH",
            ]
        );
        assert_eq!(
            chains_for(MINIMAL),
            [("iptables", "nat", "PREROUTING", CHAIN)]
        );
        assert_eq!(chains_for(FULL).len(), OWNED_CHAINS.len());
    }

    #[test]
    fn every_diverting_jump_is_limited_to_the_lan_and_armed_by_the_guard() {
        let all = lines(FULL);
        let jumps: Vec<&String> = all
            .iter()
            .filter(|line| line.contains("PREROUTING"))
            .collect();
        assert_eq!(jumps.len(), 4);
        for jump in jumps {
            assert!(jump.contains("-i br-lan"), "{jump}");
            assert!(jump.contains("--match-set openu60_clash_guard"), "{jump}");
        }
        assert!(all
            .iter()
            .any(|line| line.starts_with("ip6tables nat -I PREROUTING")
                && line.contains("openu60_clash_guard6 src")));
        // The forward chain only lets the LAN and the TUN device talk to each other.
        let forward: Vec<&String> = all
            .iter()
            .filter(|line| line.starts_with("iptables filter"))
            .collect();
        assert_eq!(
            forward,
            [
                "iptables filter -A OPENU60_CLASH -i br-lan -o ouclash0 -j ACCEPT",
                "iptables filter -A OPENU60_CLASH -i ouclash0 -o br-lan -j ACCEPT",
                "iptables filter -I FORWARD 1 -j OPENU60_CLASH",
            ]
        );
    }

    #[test]
    fn bypasses_come_before_the_proxy_targets() {
        let all = lines(FULL);
        for table in ["nat", "mangle"] {
            let chain: Vec<&String> = all
                .iter()
                .filter(|line| line.starts_with(&format!("iptables {table} -A OPENU60_CLASH ")))
                .collect();
            assert!(chain[0].ends_with("openu60_clash_local dst -j RETURN"));
            assert!(chain[1].ends_with("openu60_clash_cn dst -j RETURN"));
            let target = chain.last().unwrap();
            assert!(
                target.contains("REDIRECT") || target.contains("MARK"),
                "{target}"
            );
        }
        let dns_return = all
            .iter()
            .position(|line| line.ends_with("-p udp --dport 53 -j RETURN"))
            .unwrap();
        let mark = all
            .iter()
            .position(|line| line.contains("-j MARK"))
            .unwrap();
        assert!(dns_return < mark);
        assert!(all[mark].ends_with("-p udp -j MARK --set-xmark 0x2000000/0x6000000"));
        assert!(!all.iter().any(|line| line.contains("TPROXY")));
    }

    #[test]
    fn fake_ip_range_is_not_bypassed() {
        let fake: Ipv4Net = "198.18.0.0/16".parse().unwrap();
        for network in RESERVED {
            let network: Ipv4Net = network.parse().unwrap();
            assert!(!network.contains(&fake) && !fake.contains(&network));
        }
    }

    #[test]
    fn ipset_input_takes_only_parsed_networks() {
        let china = parse_networks(
            "1.0.1.0/24\n# comment\nnot-an-address\n1.0.2.0/23 # note\nflush everything\n\n",
        );
        assert_eq!(china.len(), 2);
        let script = ipset_script(&china);
        assert!(
            script.contains("add openu60_clash_cn 1.0.1.0/24\nadd openu60_clash_cn 1.0.2.0/23\n")
        );
        assert!(script.contains("add openu60_clash_local 192.168.0.0/16\n"));
        assert!(script.ends_with("add openu60_clash_guard6 8000::/1 timeout 180\n"));
        assert!(!script.contains("everything"));
    }
}
