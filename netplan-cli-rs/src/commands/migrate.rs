//! `netplan migrate` – convert /etc/network/interfaces to netplan YAML.
//!
//! Mirrors `netplan_cli/cli/commands/migrate.py`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use clap::Args;

// ── Argument types ────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct MigrateArgs {
    /// Search for and generate configuration files in this root directory instead of /
    #[arg(long, default_value = "/")]
    root_dir: String,

    /// Print converted netplan configuration to stdout instead of writing/changing files
    #[arg(long)]
    dry_run: bool,
}

// ── Parsed ifupdown state ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct IfaceConfig {
    method: String,
    options: HashMap<String, String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: MigrateArgs) -> Result<()> {
    let rootdir = args.root_dir.trim_end_matches('/').to_string();
    let dry_run = args.dry_run;

    let parse_result = parse_ifupdown(&rootdir);
    let (ifaces, auto_ifaces) = match parse_result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(2);
        }
    };

    // map: iface → BTreeMap of netplan keys
    let mut ethernets: BTreeMap<String, IfNetplanConfig> = BTreeMap::new();

    for (iface, families) in &ifaces {
        for (family, config) in families {
            if !auto_ifaces.contains(iface.as_str()) {
                eprintln!("{}: non-automatic interfaces are not supported", iface);
                std::process::exit(2);
            }

            match config.method.as_str() {
                "loopback" => {
                    // systemd sets up lo automatically
                }
                "dhcp" => {
                    let c = ethernets.entry(iface.clone()).or_default();
                    let mut opts = config.options.clone();

                    if let Err(e) = parse_dns_options(&mut opts, c) {
                        eprintln!("{}", e);
                        std::process::exit(2);
                    }
                    if let Err(e) = parse_hwaddress(iface, &mut opts, c) {
                        eprintln!("{}", e);
                        std::process::exit(2);
                    }

                    if !opts.is_empty() {
                        eprintln!(
                            "{}: option(s) {} are not supported for dhcp method",
                            iface,
                            opts.keys().cloned().collect::<Vec<_>>().join(", ")
                        );
                        std::process::exit(2);
                    }

                    if family == "inet" {
                        c.dhcp4 = Some(true);
                    } else {
                        c.dhcp6 = Some(true);
                    }
                }
                "static" => {
                    let base_iface = if iface.contains(':') {
                        iface.split(':').next().unwrap().to_string()
                    } else {
                        iface.clone()
                    };
                    let c = ethernets.entry(base_iface.clone()).or_default();
                    let mut opts = config.options.clone();

                    if let Err(e) = parse_dns_options(&mut opts, c) {
                        eprintln!("{}", e);
                        std::process::exit(2);
                    }
                    if let Err(e) = parse_mtu(&base_iface, &mut opts, c) {
                        eprintln!("{}", e);
                        std::process::exit(2);
                    }
                    if let Err(e) = parse_hwaddress(&base_iface, &mut opts, c) {
                        eprintln!("{}", e);
                        std::process::exit(2);
                    }

                    if family == "inet" {
                        let supported = ["address", "netmask", "gateway"];
                        let unsupported = ["broadcast", "metric", "pointopoint", "scope"];
                        check_options(&base_iface, family, &opts, &supported, &unsupported);

                        let addr_str = opts.get("address").unwrap_or_else(|| {
                            eprintln!("{}: no address supplied in static method", base_iface);
                            std::process::exit(2);
                        });

                        let (addr_part, net_spec) = if addr_str.contains('/') {
                            let addr_part = addr_str.split('/').next().unwrap().to_string();
                            (addr_part, addr_str.clone())
                        } else {
                            let netmask = opts.get("netmask").unwrap_or_else(|| {
                                eprintln!(
                                    "{}: address does not specify prefix length, and netmask not specified",
                                    base_iface
                                );
                                std::process::exit(2);
                            });
                            (addr_str.clone(), format!("{}/{}", addr_str, netmask))
                        };

                        let ipaddr = Ipv4Addr::from_str(&addr_part).unwrap_or_else(|e| {
                            eprintln!(
                                "{}: error parsing \"{}\" as an IPv4 address: {}",
                                base_iface, addr_part, e
                            );
                            std::process::exit(2);
                        });

                        let prefix = parse_ipv4_network(&net_spec).unwrap_or_else(|e| {
                            eprintln!(
                                "{}: error parsing \"{}\" as an IPv4 network: {}",
                                base_iface, net_spec, e
                            );
                            std::process::exit(2);
                        });

                        c.addresses.push(format!("{}/{}", ipaddr, prefix));

                        if let Some(gw) = opts.get("gateway") {
                            c.gateway4 = Some(gw.clone());
                        }
                    } else {
                        // inet6
                        let supported = ["address", "netmask", "gateway", "accept_ra"];
                        let unsupported = [
                            "metric",
                            "media",
                            "autoconf",
                            "privext",
                            "scope",
                            "preferred-lifetime",
                            "dad-attempts",
                            "dad-interval",
                        ];
                        check_options(&base_iface, family, &opts, &supported, &unsupported);

                        let addr_str = opts.get("address").unwrap_or_else(|| {
                            eprintln!("{}: no address supplied in static method", base_iface);
                            std::process::exit(2);
                        });

                        let (addr_part, net_spec) = if addr_str.contains('/') {
                            let addr_part = addr_str.split('/').next().unwrap().to_string();
                            (addr_part, addr_str.clone())
                        } else {
                            let netmask = opts.get("netmask").unwrap_or_else(|| {
                                eprintln!(
                                    "{}: address does not specify prefix length, and netmask not specified",
                                    base_iface
                                );
                                std::process::exit(2);
                            });
                            (addr_str.clone(), format!("{}/{}", addr_str, netmask))
                        };

                        let ipaddr = Ipv6Addr::from_str(&addr_part).unwrap_or_else(|e| {
                            eprintln!(
                                "{}: error parsing \"{}\" as an IPv6 address: {}",
                                base_iface, addr_part, e
                            );
                            std::process::exit(2);
                        });

                        let prefix = parse_ipv6_network(&net_spec).unwrap_or_else(|e| {
                            eprintln!(
                                "{}: error parsing \"{}\" as an IPv6 network: {}",
                                base_iface, net_spec, e
                            );
                            std::process::exit(2);
                        });

                        c.addresses.push(format!("{}/{}", ipaddr, prefix));

                        if let Some(gw) = opts.get("gateway") {
                            c.gateway6 = Some(gw.clone());
                        }

                        if let Some(ra) = opts.get("accept_ra") {
                            match ra.as_str() {
                                "0" => c.accept_ra = Some(false),
                                "1" => c.accept_ra = Some(true),
                                "2" => {
                                    eprintln!(
                                        "{}: netplan does not support accept_ra=2",
                                        base_iface
                                    );
                                    std::process::exit(2);
                                }
                                other => {
                                    eprintln!(
                                        "{}: unexpected accept_ra value \"{}\"",
                                        base_iface, other
                                    );
                                    std::process::exit(2);
                                }
                            }
                        }
                    }
                }
                other => {
                    eprintln!("{}: method {} is not supported", iface, other);
                    std::process::exit(2);
                }
            }
        }
    }

    let if_config = PathBuf::from(format!("{}/etc/network/interfaces", rootdir));

    if !ethernets.is_empty() {
        let yaml = render_yaml(&ethernets);
        if dry_run {
            print!("{}", yaml);
        } else {
            let dest = PathBuf::from(format!("{}/etc/netplan/10-ifupdown.yaml", rootdir));
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).ok();
            }
            // Use exclusive create (fail if exists)
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(yaml.as_bytes())
                        .with_context(|| format!("failed to write {:?}", dest))?;
                    eprintln!("migration complete, wrote {}", dest.display());
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    eprintln!(
                        "{} already exists; remove it if you want to run the migration again",
                        dest.display()
                    );
                    std::process::exit(3);
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("failed to write {:?}", dest));
                }
            }
        }
    } else {
        eprintln!("ifupdown does not configure any interfaces, nothing to migrate");
    }

    if !dry_run {
        let converted = format!("{}.netplan-converted", if_config.display());
        eprintln!(
            "renaming {} to {}",
            if_config.display(),
            if_config.display()
        );
        fs::rename(&if_config, &converted)
            .with_context(|| format!("failed to rename {:?}", if_config))?;
    }

    Ok(())
}

// ── Netplan config structure ──────────────────────────────────────────────────

#[derive(Debug, Default)]
struct IfNetplanConfig {
    dhcp4: Option<bool>,
    dhcp6: Option<bool>,
    addresses: Vec<String>,
    gateway4: Option<String>,
    gateway6: Option<String>,
    accept_ra: Option<bool>,
    mtu: Option<u32>,
    macaddress: Option<String>,
    nameservers_addresses: Vec<String>,
    nameservers_search: Vec<String>,
}

// ── Option parsers ────────────────────────────────────────────────────────────

fn parse_dns_options(opts: &mut HashMap<String, String>, c: &mut IfNetplanConfig) -> Result<()> {
    if let Some(ns) = opts.remove("dns-nameservers") {
        for s in ns.split_whitespace() {
            c.nameservers_addresses.push(s.to_string());
        }
    }
    if let Some(search) = opts.remove("dns-search") {
        for s in search.split_whitespace() {
            c.nameservers_search.push(s.to_string());
        }
    }
    Ok(())
}

fn parse_mtu(
    iface: &str,
    opts: &mut HashMap<String, String>,
    c: &mut IfNetplanConfig,
) -> Result<()> {
    if let Some(mtu_str) = opts.remove("mtu") {
        let mtu: u32 = mtu_str
            .parse()
            .map_err(|_| anyhow::anyhow!("{}: cannot parse \"{}\" as an MTU", iface, mtu_str))?;
        if let Some(existing) = c.mtu {
            if existing != mtu {
                bail!(
                    "{}: tried to set MTU={}, but already have MTU={}",
                    iface,
                    mtu,
                    existing
                );
            }
        }
        c.mtu = Some(mtu);
    }
    Ok(())
}

fn parse_hwaddress(
    iface: &str,
    opts: &mut HashMap<String, String>,
    c: &mut IfNetplanConfig,
) -> Result<()> {
    if let Some(mac) = opts.remove("hwaddress") {
        if let Some(ref existing) = c.macaddress {
            if existing != &mac {
                bail!(
                    "{}: tried to set MAC {}, but already have MAC {}",
                    iface,
                    mac,
                    existing
                );
            }
        }
        c.macaddress = Some(mac);
    }
    Ok(())
}

fn check_options(
    iface: &str,
    family: &str,
    opts: &HashMap<String, String>,
    supported: &[&str],
    unsupported: &[&str],
) {
    for key in opts.keys() {
        if !supported.contains(&key.as_str()) {
            if unsupported.contains(&key.as_str()) {
                eprintln!("{}: unsupported {} option \"{}\"", iface, family, key);
            } else {
                eprintln!("{}: unknown {} option \"{}\"", iface, family, key);
            }
            std::process::exit(2);
        }
    }
}

// ── IP address/network helpers ────────────────────────────────────────────────

/// Parse "addr/prefix_or_netmask" → prefix length. Returns Err with message on failure.
fn parse_ipv4_network(spec: &str) -> Result<u8> {
    let (addr_part, mask_part) = spec.split_once('/').unwrap_or((spec, "32"));

    // Try parsing mask as a number first
    if let Ok(n) = mask_part.parse::<u8>() {
        if n > 32 {
            bail!("{}/{} has host bits set", addr_part, mask_part);
        }
        return Ok(n);
    }

    // Try parsing as dotted-quad netmask
    let mask_addr = Ipv4Addr::from_str(mask_part)
        .map_err(|_| anyhow::anyhow!("Invalid netmask: {}", mask_part))?;
    let mask_bits = u32::from(mask_addr);

    // Validate it's a contiguous mask: leading 1-bits then all 0-bits.
    let leading_ones = mask_bits.leading_ones();
    let expected = if leading_ones == 32 {
        !0u32
    } else {
        !0u32 << (32 - leading_ones)
    };
    if mask_bits != expected {
        bail!("Non-contiguous netmask: {}", mask_part);
    }
    let prefix = leading_ones as u8;
    Ok(prefix)
}

/// Parse "addr/prefix" for IPv6 → prefix length.
fn parse_ipv6_network(spec: &str) -> Result<u8> {
    let (addr_part, prefix_part) = match spec.split_once('/') {
        Some(p) => p,
        None => bail!("missing prefix length in {}", spec),
    };

    // Validate address
    Ipv6Addr::from_str(addr_part)
        .map_err(|e| anyhow::anyhow!("Invalid IPv6 address {}: {}", addr_part, e))?;

    let prefix: u8 = prefix_part
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid prefix length: {}", prefix_part))?;
    if prefix > 128 {
        bail!("prefix length {} > 128", prefix);
    }
    Ok(prefix)
}

// ── YAML renderer ─────────────────────────────────────────────────────────────

fn render_yaml(ethernets: &BTreeMap<String, IfNetplanConfig>) -> String {
    let mut out = String::new();
    out.push_str("network:\n");
    out.push_str("  ethernets:\n");

    for (iface, c) in ethernets {
        out.push_str(&format!("    {}:\n", iface));

        // Collect all keys that are set, sort them (to match Python's yaml.dump sorted output)
        let mut keys: Vec<&str> = Vec::new();
        if c.accept_ra.is_some() {
            keys.push("accept_ra");
        }
        if !c.addresses.is_empty() {
            keys.push("addresses");
        }
        if c.dhcp4.is_some() {
            keys.push("dhcp4");
        }
        if c.dhcp6.is_some() {
            keys.push("dhcp6");
        }
        if c.gateway4.is_some() {
            keys.push("gateway4");
        }
        if c.gateway6.is_some() {
            keys.push("gateway6");
        }
        if c.macaddress.is_some() {
            keys.push("macaddress");
        }
        if c.mtu.is_some() {
            keys.push("mtu");
        }
        if !c.nameservers_addresses.is_empty() || !c.nameservers_search.is_empty() {
            keys.push("nameservers");
        }

        for key in keys {
            match key {
                "accept_ra" => {
                    out.push_str(&format!(
                        "      accept_ra: {}\n",
                        yaml_bool(c.accept_ra.unwrap())
                    ));
                }
                "addresses" => {
                    out.push_str("      addresses:\n");
                    for addr in &c.addresses {
                        out.push_str(&format!("      - {}\n", addr));
                    }
                }
                "dhcp4" => {
                    out.push_str(&format!("      dhcp4: {}\n", yaml_bool(c.dhcp4.unwrap())));
                }
                "dhcp6" => {
                    out.push_str(&format!("      dhcp6: {}\n", yaml_bool(c.dhcp6.unwrap())));
                }
                "gateway4" => {
                    out.push_str(&format!(
                        "      gateway4: {}\n",
                        c.gateway4.as_ref().unwrap()
                    ));
                }
                "gateway6" => {
                    out.push_str(&format!(
                        "      gateway6: {}\n",
                        c.gateway6.as_ref().unwrap()
                    ));
                }
                "macaddress" => {
                    out.push_str(&format!(
                        "      macaddress: {}\n",
                        c.macaddress.as_ref().unwrap()
                    ));
                }
                "mtu" => {
                    out.push_str(&format!("      mtu: {}\n", c.mtu.unwrap()));
                }
                "nameservers" => {
                    out.push_str("      nameservers:\n");
                    // Python yaml.dump sorts: addresses before search
                    if !c.nameservers_addresses.is_empty() {
                        out.push_str("        addresses:\n");
                        for addr in &c.nameservers_addresses {
                            out.push_str(&format!("        - {}\n", addr));
                        }
                    }
                    if !c.nameservers_search.is_empty() {
                        out.push_str("        search:\n");
                        for s in &c.nameservers_search {
                            out.push_str(&format!("        - {}\n", s));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    out.push_str("  version: 2\n");
    out
}

fn yaml_bool(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

// ── ifupdown parser ───────────────────────────────────────────────────────────

/// Returns (iface → family → IfaceConfig, auto_ifaces).
/// Preserves insertion order via Vec.
fn parse_ifupdown(
    rootdir: &str,
) -> Result<(
    Vec<(String, Vec<(String, IfaceConfig)>)>,
    std::collections::HashSet<String>,
)> {
    let lines = ifupdown_lines_from_file(rootdir, "/etc/network/interfaces");

    let mut ifaces: Vec<(String, Vec<(String, IfaceConfig)>)> = Vec::new();
    let mut auto: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut in_iface: Option<String> = None;
    let mut in_family: Option<String> = None;

    let field_lens: HashMap<&str, usize> = [
        ("auto", 1),
        ("allow-auto", 1),
        ("allow-hotplug", 1),
        ("mapping", 1),
        ("no-scripts", 1),
        ("iface", 3),
    ]
    .into_iter()
    .collect();

    for line in &lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.is_empty() {
            continue;
        }

        if let Some(&exp_len) = field_lens.get(fields[0]) {
            in_iface = None;
            in_family = None;

            if fields.len() != exp_len + 1 {
                bail!(
                    "Expected {} fields for stanza type {} but got {}",
                    exp_len,
                    fields[0],
                    fields.len() - 1
                );
            }

            match fields[0] {
                "auto" | "allow-auto" | "allow-hotplug" => {
                    auto.insert(fields[1].to_string());
                }
                "mapping" => {
                    bail!("mapping stanza is not supported");
                }
                "no-scripts" => {}
                "iface" => {
                    if fields[2] != "inet" && fields[2] != "inet6" {
                        bail!("Unknown address family {}", fields[2]);
                    }
                    if fields[3] != "loopback" && fields[3] != "static" && fields[3] != "dhcp" {
                        bail!("Unsupported method {}", fields[3]);
                    }
                    let iface_name = fields[1].to_string();
                    let family = fields[2].to_string();
                    let method = fields[3].to_string();

                    // find or create iface entry
                    if let Some(entry) = ifaces.iter_mut().find(|(n, _)| n == &iface_name) {
                        entry.1.push((
                            family.clone(),
                            IfaceConfig {
                                method,
                                options: HashMap::new(),
                            },
                        ));
                    } else {
                        ifaces.push((
                            iface_name.clone(),
                            vec![(
                                family.clone(),
                                IfaceConfig {
                                    method,
                                    options: HashMap::new(),
                                },
                            )],
                        ));
                    }
                    in_iface = Some(iface_name);
                    in_family = Some(family);
                }
                _ => {}
            }
        } else {
            // Option line
            if let (Some(ref iface_name), Some(ref family)) = (&in_iface, &in_family) {
                let val = line
                    .splitn(2, char::is_whitespace)
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if let Some(entry) = ifaces.iter_mut().find(|(n, _)| n == iface_name) {
                    if let Some((_, cfg)) = entry.1.iter_mut().find(|(f, _)| f == family) {
                        cfg.options.insert(fields[0].to_string(), val);
                    }
                }
            } else {
                bail!("Unknown stanza type {}", fields[0]);
            }
        }
    }

    Ok((ifaces, auto))
}

/// Read and normalize ifupdown config lines, resolving source/source-directory includes.
fn ifupdown_lines_from_file(rootdir: &str, path: &str) -> Vec<String> {
    let full_path = format!("{}{}", rootdir, path);
    let rootdir_prefix_len = rootdir.len();

    let content = match fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let curdir = Path::new(&full_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut lines = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with("source-directory ") {
            let arg = line.split_whitespace().nth(1).unwrap_or("");
            let dir = expand_source_arg(rootdir, &curdir, arg);
            let dir_path = Path::new(&dir);
            if let Ok(entries) = fs::read_dir(dir_path) {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|name| {
                        name.chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                    })
                    .collect();
                names.sort();
                for fname in names {
                    let sub_path = format!("{}/{}", &dir[rootdir_prefix_len..], fname);
                    lines.extend(ifupdown_lines_from_file(rootdir, &sub_path));
                }
            }
        } else if line.starts_with("source ") {
            let arg = line.split_whitespace().nth(1).unwrap_or("");
            let pattern = expand_source_arg(rootdir, &curdir, arg);
            let mut matched = glob_paths(&pattern);
            matched.sort();
            for full in matched {
                let sub_path = &full[rootdir_prefix_len..];
                lines.extend(ifupdown_lines_from_file(rootdir, sub_path));
            }
        } else {
            lines.push(line.to_string());
        }
    }

    lines
}

/// Expand a source/source-directory argument relative to rootdir and curdir.
fn expand_source_arg(rootdir: &str, curdir: &str, arg: &str) -> String {
    if arg.starts_with('/') {
        format!("{}{}", rootdir, arg)
    } else {
        format!("{}/{}", curdir, arg)
    }
}

/// Simple glob that supports `*` wildcard in the filename component.
fn glob_paths(pattern: &str) -> Vec<String> {
    let (dir_str, file_pat) = match pattern.rfind('/') {
        Some(pos) => (&pattern[..pos], &pattern[pos + 1..]),
        None => (".", pattern),
    };

    let dir = Path::new(dir_str);
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if glob_match(file_pat, &name) {
                Some(format!("{}/{}", dir_str, name))
            } else {
                None
            }
        })
        .collect()
}

/// Match a filename against a glob pattern (supports `*`).
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut remaining = name;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !remaining.starts_with(part) {
                return false;
            }
            remaining = &remaining[part.len()..];
        } else if i == parts.len() - 1 {
            if !remaining.ends_with(part) {
                return false;
            }
        } else {
            match remaining.find(part) {
                Some(pos) => remaining = &remaining[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}
