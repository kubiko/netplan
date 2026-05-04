//! `netplan status` – show system network state.
//!
//! Mirrors `netplan_cli/cli/commands/status.py` and `netplan_cli/cli/state.py`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Map, Value};

// ── Argument types ────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show only this interface
    pub ifname: Option<String>,

    /// Show all interface data (incl. inactive)
    #[arg(short = 'a', long)]
    pub all: bool,

    /// Show extra information (all route tables, etc.)
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Output format: tabular (default), json, or yaml
    #[arg(short = 'f', long, default_value = "tabular")]
    pub format: String,

    /// Show the differences between the system's and netplan's states
    #[arg(long)]
    pub diff: bool,

    /// Only show the differences between the system's and netplan's states
    #[arg(long)]
    pub diff_only: bool,

    /// Search for configuration files in this root directory instead of /
    #[arg(long, default_value = "/")]
    pub root_dir: String,
}

// ── Device type mapping ───────────────────────────────────────────────────────

fn device_type(nd_type: &str) -> Option<&'static str> {
    match nd_type {
        "bond"               => Some("bond"),
        "bridge"             => Some("bridge"),
        "dummy"              => Some("dummy-device"),
        "erspan"             => Some("tunnel"),
        "ether"              => Some("ethernet"),
        "gretap"             => Some("tunnel"),
        "ipgre"              => Some("tunnel"),
        "ip6gre"             => Some("tunnel"),
        "loopback"           => Some("ethernet"),
        "sit"                => Some("tunnel"),
        "tunnel"             => Some("tunnel"),
        "tun"                => Some("tunnel"),
        "tunnel6"            => Some("tunnel"),
        "wireguard"          => Some("tunnel"),
        "wlan"               => Some("wifi"),
        "wwan"               => Some("modem"),
        "veth"               => Some("virtual-ethernet"),
        "vlan"               => Some("vlan"),
        "vrf"                => Some("vrf"),
        "ieee80211_radiotap" => Some("wifi"),
        "none"               => None,
        _                    => None,
    }
}

// ── Per-interface data ────────────────────────────────────────────────────────

#[derive(Debug)]
struct IfaceData {
    idx: u64,
    name: String,
    adminstate: String,
    operstate: String,
    iproute_type: Option<String>,
    macaddress: Option<String>,

    // networkd JSON fields
    nd_type: Option<String>,
    nd_kind: Option<String>,
    nd_setup_state: Option<String>,
    nd_network_file: Option<String>,
    nd_vendor: Option<String>,
    // networkctl text (for activation_mode and wifi SSID)
    networkctl_text: String,

    // NetworkManager CSV fields
    nm_filename: Option<String>,
    nm_name: Option<String>,
    nm_type: Option<String>,
    nm_autoconnect: Option<String>,

    // DNS
    dns_addresses: Option<Vec<String>>,
    dns_search: Option<Vec<String>>,

    // Routes (raw iproute2 route objects filtered to this dev)
    routes: Option<Vec<Map<String, Value>>>,

    // Addresses
    addresses: Option<Vec<Value>>,

    // Members and uplink (filled later by correlate step)
    bridge: Option<String>,
    bond: Option<String>,
    vrf: Option<String>,
    members: Vec<String>,
}

impl IfaceData {
    fn iface_type(&self) -> Option<&'static str> {
        let mut t = self.nd_type.as_deref();
        // 'none' → fall back to Kind
        if t == Some("none") {
            t = self.nd_kind.as_deref();
        }
        // 'ether' with a Kind → use Kind
        if t == Some("ether") {
            if let Some(k) = &self.nd_kind {
                t = Some(k.as_str());
            }
        }
        device_type(t.unwrap_or(""))
    }

    fn tunnel_mode(&self) -> Option<&str> {
        if self.iface_type() == Some("tunnel") {
            self.iproute_type.as_deref()
        } else {
            None
        }
    }

    fn backend(&self) -> Option<&'static str> {
        let setup = self.nd_setup_state.as_deref().unwrap_or("");
        let netfile = self.nd_network_file.as_deref().unwrap_or("");
        if !setup.contains("unmanaged") && netfile.contains("run/systemd/network/10-netplan-") {
            return Some("networkd");
        }
        let nm_file = self.nm_filename.as_deref().unwrap_or("");
        if nm_file.contains("run/NetworkManager/system-connections/netplan-") {
            return Some("NetworkManager");
        }
        None
    }

    fn netdef_id(&self) -> Option<String> {
        match self.backend() {
            Some("networkd") => {
                let netfile = self.nd_network_file.as_deref()?;
                let after = netfile.split("run/systemd/network/10-netplan-").nth(1)?;
                Some(after.split(".network").next()?.to_string())
            }
            Some("NetworkManager") => {
                let nm_file = self.nm_filename.as_deref()?;
                let after = nm_file
                    .split("run/NetworkManager/system-connections/netplan-")
                    .nth(1)?;
                let mut netdef = after.split(".nmconnection").next()?.to_string();
                if self.nm_type.as_deref() == Some("802-11-wireless") {
                    if let Some(ssid) = self.ssid() {
                        let suffix = format!("-{}", ssid);
                        if let Some(pos) = netdef.find(&suffix) {
                            netdef.truncate(pos);
                        }
                    }
                }
                Some(netdef)
            }
            _ => None,
        }
    }

    fn vendor(&self) -> Option<&str> {
        self.nd_vendor.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty())
    }

    fn ssid(&self) -> Option<String> {
        if self.iface_type() != Some("wifi") {
            return None;
        }
        if self.backend() == Some("NetworkManager") {
            if let Some(nm_name) = &self.nm_name {
                return query_nm_ssid(nm_name);
            }
        }
        // Parse networkctl text for "Wi-Fi access point: <SSID> (<mac>)"
        for line in self.networkctl_text.lines() {
            let line = line.trim();
            // Match "WiFi access point:" or "Wi-Fi access point:"
            let key_variants = [
                "WiFi access point: ",
                "Wi-Fi access point: ",
            ];
            for key in &key_variants {
                if let Some(rest) = line.strip_prefix(key) {
                    // strip trailing " (mac)" part
                    let ssid = if let Some(pos) = rest.rfind(" (") {
                        &rest[..pos]
                    } else {
                        rest
                    };
                    if !ssid.is_empty() {
                        return Some(ssid.to_string());
                    }
                }
            }
        }
        None
    }

    fn activation_mode(&self) -> Option<String> {
        match self.backend() {
            Some("networkd") => {
                for line in self.networkctl_text.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("Activation Policy: ") {
                        let mode = rest.trim();
                        return if mode == "up" { None } else { Some(mode.to_string()) };
                    }
                }
                None
            }
            Some("NetworkManager") => {
                let autoconnect = self.nm_autoconnect.as_deref().unwrap_or("yes");
                if autoconnect == "no" {
                    Some("manual".to_string())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Is interface up (both admin and oper)?
    fn is_up(&self) -> bool {
        self.adminstate == "UP" && self.operstate == "UP"
    }

    fn to_json_obj(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("index".into(), Value::Number(self.idx.into()));
        m.insert("adminstate".into(), Value::String(self.adminstate.clone()));
        m.insert("operstate".into(), Value::String(self.operstate.clone()));

        if let Some(t) = self.iface_type() {
            m.insert("type".into(), Value::String(t.to_string()));
        }
        if let Some(s) = self.ssid() {
            m.insert("ssid".into(), Value::String(s));
        }
        if let Some(tm) = self.tunnel_mode() {
            m.insert("tunnel_mode".into(), Value::String(tm.to_string()));
        }
        if let Some(b) = self.backend() {
            m.insert("backend".into(), Value::String(b.to_string()));
        }
        if let Some(id) = self.netdef_id() {
            m.insert("id".into(), Value::String(id));
        }
        if let Some(mac) = &self.macaddress {
            m.insert("macaddress".into(), Value::String(mac.clone()));
        }
        if let Some(v) = self.vendor() {
            m.insert("vendor".into(), Value::String(v.to_string()));
        }
        if let Some(addrs) = &self.addresses {
            if !addrs.is_empty() {
                m.insert("addresses".into(), Value::Array(addrs.clone()));
            }
        }
        if let Some(dns) = &self.dns_addresses {
            if !dns.is_empty() {
                m.insert("dns_addresses".into(), Value::Array(
                    dns.iter().map(|s| Value::String(s.clone())).collect(),
                ));
            }
        }
        if let Some(srch) = &self.dns_search {
            if !srch.is_empty() {
                m.insert("dns_search".into(), Value::Array(
                    srch.iter().map(|s| Value::String(s.clone())).collect(),
                ));
            }
        }
        if let Some(routes) = &self.routes {
            if !routes.is_empty() {
                m.insert("routes".into(), Value::Array(
                    routes.iter().map(|r| Value::Object(r.clone())).collect(),
                ));
            }
        }
        if let Some(am) = self.activation_mode() {
            m.insert("activation_mode".into(), Value::String(am));
        }
        if let Some(br) = &self.bridge {
            m.insert("bridge".into(), Value::String(br.clone()));
        }
        if let Some(bo) = &self.bond {
            m.insert("bond".into(), Value::String(bo.clone()));
        }
        if let Some(vf) = &self.vrf {
            m.insert("vrf".into(), Value::String(vf.clone()));
        }
        if !self.members.is_empty() {
            m.insert("interfaces".into(), Value::Array(
                self.members.iter().map(|s| Value::String(s.clone())).collect(),
            ));
        }
        m
    }
}

// ── System queries ────────────────────────────────────────────────────────────

fn run_cmd(args: &[&str]) -> Result<String> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .output()
        .with_context(|| format!("failed to run {:?}", args[0]))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn query_iproute2() -> Result<Vec<Value>> {
    let out = run_cmd(&["ip", "-d", "-j", "addr"])?;
    let v: Value = serde_json::from_str(&out).context("ip addr JSON parse failed")?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

fn query_networkd() -> Result<Vec<Value>> {
    let out = run_cmd(&["networkctl", "--json=short"])?;
    let v: Value = serde_json::from_str(&out).context("networkctl JSON parse failed")?;
    Ok(v["Interfaces"].as_array().cloned().unwrap_or_default())
}

fn query_nm() -> Vec<Value> {
    let out = match run_cmd(&["nmcli", "-t", "-f",
                               "DEVICE,NAME,UUID,FILENAME,TYPE,AUTOCONNECT",
                               "con", "show"]) {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    let mut data = vec![];
    for line in out.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 6 && !parts[0].is_empty() {
            let mut m = Map::new();
            m.insert("device".into(),      Value::String(parts[0].to_string()));
            m.insert("name".into(),        Value::String(parts[1].to_string()));
            m.insert("uuid".into(),        Value::String(parts[2].to_string()));
            m.insert("filename".into(),    Value::String(parts[3].to_string()));
            m.insert("type".into(),        Value::String(parts[4].to_string()));
            m.insert("autoconnect".into(), Value::String(parts[5].to_string()));
            data.push(Value::Object(m));
        }
    }
    data
}

fn query_routes() -> (Vec<Value>, Vec<Value>) {
    let mut r4 = vec![];
    let mut r6 = vec![];
    if let Ok(o) = run_cmd(&["ip", "-d", "-j", "-4", "route", "show", "table", "all"]) {
        if let Ok(v) = serde_json::from_str::<Value>(&o) {
            if let Some(arr) = v.as_array() {
                r4 = arr.iter().map(|x| {
                    let mut v = x.clone();
                    v.as_object_mut().map(|m| m.insert("family".into(), Value::Number(2.into())));
                    v
                }).collect();
            }
        }
    }
    if let Ok(o) = run_cmd(&["ip", "-d", "-j", "-6", "route", "show", "table", "all"]) {
        if let Ok(v) = serde_json::from_str::<Value>(&o) {
            if let Some(arr) = v.as_array() {
                r6 = arr.iter().map(|x| {
                    let mut v = x.clone();
                    v.as_object_mut().map(|m| m.insert("family".into(), Value::Number(10.into())));
                    v
                }).collect();
            }
        }
    }
    (r4, r6)
}

/// Parse busctl DNS data. Returns (addresses, search_domains).
fn query_resolved() -> (Vec<(u64, u64, Vec<u8>)>, Vec<(u64, String)>) {
    let busctl = match which_busctl() {
        Some(b) => b,
        None => return (vec![], vec![]),
    };
    let out = match run_cmd(&[
        &busctl, "--json=short", "call", "--system",
        "org.freedesktop.resolve1",
        "/org/freedesktop/resolve1",
        "org.freedesktop.DBus.Properties",
        "GetAll", "s",
        "org.freedesktop.resolve1.Manager",
    ]) {
        Ok(o) => o,
        Err(_) => return (vec![], vec![]),
    };
    parse_resolved_json(&out)
}

fn which_busctl() -> Option<String> {
    let out = Command::new("which").arg("busctl").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn parse_resolved_json(json_str: &str) -> (Vec<(u64, u64, Vec<u8>)>, Vec<(u64, String)>) {
    let v: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return (vec![], vec![]),
    };
    let data = match v.get("data").and_then(|d| d.get(0)) {
        Some(d) => d,
        None => return (vec![], vec![]),
    };

    let mut addresses = vec![];
    if let Some(dns_arr) = data.get("DNS").and_then(|d| d.get("data")).and_then(|d| d.as_array()) {
        for entry in dns_arr {
            if let Some(arr) = entry.as_array() {
                if arr.len() >= 3 {
                    let ifidx = arr[0].as_u64().unwrap_or(0);
                    let family = arr[1].as_u64().unwrap_or(0);
                    let bytes: Vec<u8> = arr[2].as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect())
                        .unwrap_or_default();
                    addresses.push((ifidx, family, bytes));
                }
            }
        }
    }

    let mut search = vec![];
    if let Some(dom_arr) = data.get("Domains").and_then(|d| d.get("data")).and_then(|d| d.as_array()) {
        for entry in dom_arr {
            if let Some(arr) = entry.as_array() {
                if arr.len() >= 2 {
                    let ifidx = arr[0].as_u64().unwrap_or(0);
                    let domain = arr[1].as_str().unwrap_or("").to_string();
                    search.push((ifidx, domain));
                }
            }
        }
    }

    (addresses, search)
}

fn query_nm_ssid(con_name: &str) -> Option<String> {
    let out = run_cmd(&["nmcli", "--get-values", "802-11-wireless.ssid",
                        "con", "show", "id", con_name]).ok()?;
    let s = out.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn query_networkctl_text(ifname: &str) -> String {
    run_cmd(&["networkctl", "status", "--", ifname]).unwrap_or_default()
}

fn query_members(ifname: &str) -> Vec<String> {
    let out = match run_cmd(&["ip", "-d", "-j", "link", "show", "master", ifname]) {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    let v: Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    v.as_array()
        .map(|a| a.iter()
            .filter_map(|m| m.get("ifname").and_then(|n| n.as_str()).map(|s| s.to_string()))
            .collect())
        .unwrap_or_default()
}

/// Parse {root_dir}/etc/resolv.conf → {addresses, search, mode}
fn resolvconf_json(root_dir: &str) -> Map<String, Value> {
    let mut res = Map::new();
    res.insert("addresses".into(), Value::Array(vec![]));
    res.insert("search".into(),    Value::Array(vec![]));
    res.insert("mode".into(),      Value::Null);

    let path = std::path::PathBuf::from(root_dir).join("etc/resolv.conf");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return res,
    };
    let mut lines = content.lines();
    let firstline = lines.next().unwrap_or("");
    let mode = if firstline.contains("# This is /run/systemd/resolve/stub-resolv.conf") {
        Some("stub")
    } else if firstline.contains("# This is /run/systemd/resolve/resolv.conf") {
        Some("compat")
    } else {
        None
    };
    if let Some(m) = mode {
        *res.get_mut("mode").unwrap() = Value::String(m.to_string());
    }

    let mut addresses: Vec<Value> = vec![];
    let mut search: Vec<Value> = vec![];

    // process firstline + rest
    let all_lines = std::iter::once(firstline).chain(lines);
    for line in all_lines {
        if let Some(rest) = line.strip_prefix("nameserver") {
            for addr in rest.split_whitespace() {
                addresses.push(Value::String(addr.to_string()));
            }
        }
        if let Some(rest) = line.strip_prefix("search") {
            search = rest.split_whitespace()
                .map(|s| Value::String(s.to_string()))
                .collect();
        }
    }
    *res.get_mut("addresses").unwrap() = Value::Array(addresses);
    *res.get_mut("search").unwrap()    = Value::Array(search);
    res
}

// ── DNS bytes → IP string ─────────────────────────────────────────────────────

fn bytes_to_ip(family: u64, bytes: &[u8]) -> Option<String> {
    match family {
        2 if bytes.len() == 4 => {
            let addr = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            Some(addr.to_string())
        }
        10 if bytes.len() == 16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            let addr = Ipv6Addr::from(octets);
            Some(addr.to_string())
        }
        _ => None,
    }
}

// ── Interface builder ─────────────────────────────────────────────────────────

fn build_iface(
    ip: &Value,
    nd_data: &[Value],
    nm_data: &[Value],
    dns_addresses: &[(u64, u64, Vec<u8>)],
    dns_search: &[(u64, String)],
    routes4: &[Value],
    routes6: &[Value],
) -> IfaceData {
    let idx = ip.get("ifindex").and_then(|v| v.as_u64()).unwrap_or(0);
    let name = ip.get("ifname").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
    let flags: Vec<&str> = ip.get("flags")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let adminstate = if flags.contains(&"UP") { "UP" } else { "DOWN" };
    let operstate = ip.get("operstate").and_then(|v| v.as_str()).unwrap_or("UNKNOWN").to_uppercase();
    let macaddress = ip.get("address").and_then(|v| v.as_str())
        .filter(|s| s.len() == 17)
        .map(|s| s.to_lowercase());
    let iproute_type = ip.get("linkinfo")
        .and_then(|li| li.get("info_kind"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    // Match networkd entry by index
    let nd = nd_data.iter().find(|x| x.get("Index").and_then(|i| i.as_u64()) == Some(idx));
    let nd_type = nd.and_then(|x| x.get("Type")).and_then(|v| v.as_str()).map(|s| s.to_lowercase());
    let nd_kind = nd.and_then(|x| x.get("Kind")).and_then(|v| v.as_str()).map(|s| s.to_lowercase());
    let nd_setup_state = nd.and_then(|x| x.get("SetupState")).and_then(|v| v.as_str()).map(String::from);
    let nd_network_file = nd.and_then(|x| x.get("NetworkFile")).and_then(|v| v.as_str()).map(String::from);
    let nd_vendor = nd.and_then(|x| x.get("Vendor")).and_then(|v| v.as_str()).map(String::from);

    // Match NM entry by device name
    let nm = nm_data.iter().find(|x| x.get("device").and_then(|d| d.as_str()) == Some(&name));
    let nm_filename = nm.and_then(|x| x.get("filename")).and_then(|v| v.as_str()).map(String::from);
    let nm_name = nm.and_then(|x| x.get("name")).and_then(|v| v.as_str()).map(String::from);
    let nm_type = nm.and_then(|x| x.get("type")).and_then(|v| v.as_str()).map(String::from);
    let nm_autoconnect = nm.and_then(|x| x.get("autoconnect")).and_then(|v| v.as_str()).map(String::from);

    // DNS addresses for this interface
    let dns_addr_list: Option<Vec<String>> = if dns_addresses.is_empty() {
        None
    } else {
        let addrs: Vec<String> = dns_addresses.iter()
            .filter(|(ifidx, _, _)| *ifidx == idx)
            .filter_map(|(_, family, bytes)| bytes_to_ip(*family, bytes))
            .collect();
        Some(addrs)
    };

    let dns_search_list: Option<Vec<String>> = if dns_search.is_empty() {
        None
    } else {
        let srch: Vec<String> = dns_search.iter()
            .filter(|(ifidx, _)| *ifidx == idx)
            .map(|(_, domain)| domain.clone())
            .collect();
        Some(srch)
    };

    // Routes filtered to this dev
    let all_routes: Vec<&Value> = routes4.iter().chain(routes6.iter()).collect();
    let filtered_routes: Option<Vec<Map<String, Value>>> = if all_routes.is_empty() {
        None
    } else {
        let mut out = vec![];
        for r in all_routes {
            if r.get("dev").and_then(|d| d.as_str()) != Some(&name) {
                continue;
            }
            let mut m = Map::new();
            if let Some(v) = r.get("dst") { m.insert("to".into(), v.clone()); }
            if let Some(v) = r.get("family") { m.insert("family".into(), v.clone()); }
            if let Some(v) = r.get("gateway") { m.insert("via".into(), v.clone()); }
            if let Some(v) = r.get("prefsrc") { m.insert("from".into(), v.clone()); }
            if let Some(v) = r.get("metric") { m.insert("metric".into(), v.clone()); }
            if let Some(v) = r.get("type") { m.insert("type".into(), v.clone()); }
            if let Some(v) = r.get("scope") { m.insert("scope".into(), v.clone()); }
            if let Some(v) = r.get("protocol") { m.insert("protocol".into(), v.clone()); }
            if let Some(v) = r.get("table") { m.insert("table".into(), v.clone()); }
            out.push(m);
        }
        Some(out)
    };

    // Addresses
    let addresses = build_addresses(ip, filtered_routes.as_deref());

    let networkctl_text = query_networkctl_text(&name);

    IfaceData {
        idx,
        name,
        adminstate: adminstate.to_string(),
        operstate,
        iproute_type,
        macaddress,
        nd_type,
        nd_kind,
        nd_setup_state,
        nd_network_file,
        nd_vendor,
        networkctl_text,
        nm_filename,
        nm_name,
        nm_type,
        nm_autoconnect,
        dns_addresses: dns_addr_list,
        dns_search: dns_search_list,
        routes: filtered_routes,
        addresses,
        bridge: None,
        bond: None,
        vrf: None,
        members: vec![],
    }
}

fn build_addresses(ip: &Value, routes: Option<&[Map<String, Value>]>) -> Option<Vec<Value>> {
    let addr_info = match ip.get("addr_info").and_then(|a| a.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return None,
    };

    // Collect RA networks from routes (IPv6, family=10, not default, protocol=ra)
    let mut ra_networks: Vec<(Ipv6Addr, u32)> = vec![];
    if let Some(routes) = routes {
        for route in routes {
            let family = route.get("family").and_then(|v| v.as_u64()).unwrap_or(0);
            let protocol = route.get("protocol").and_then(|v| v.as_str()).unwrap_or("");
            let to = route.get("to").and_then(|v| v.as_str()).unwrap_or("");
            if family == 10 && protocol == "ra" && to != "default" {
                if let Some((net, prefix)) = parse_ipv6_cidr(to) {
                    ra_networks.push((net, prefix));
                }
            }
        }
    }

    let mut addresses = vec![];
    for addr in addr_info {
        let local = match addr.get("local").and_then(|v| v.as_str()) {
            Some(l) => l,
            None => continue,
        };
        let prefixlen = addr.get("prefixlen").and_then(|v| v.as_u64()).unwrap_or(0);
        let dynamic = addr.get("dynamic").and_then(|v| v.as_bool()).unwrap_or(false);

        let mut flags: Vec<&str> = vec![];

        // link-local detection
        if is_link_local(local) {
            flags.push("link");
        }
        if dynamic {
            flags.push("dynamic");
        }

        // RA detection for IPv6
        if let Ok(ip6) = local.parse::<Ipv6Addr>() {
            for (net, prefix) in &ra_networks {
                if ipv6_in_network(&ip6, net, *prefix) {
                    flags.push("ra");
                    break;
                }
            }
        }

        // DHCP detection from routes
        if let Some(routes) = routes {
            'outer: for route in routes {
                let from_addr = route.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let protocol = route.get("protocol").and_then(|v| v.as_str()).unwrap_or("");
                if from_addr == local && protocol == "dhcp" && !flags.contains(&"dhcp") {
                    flags.push("dhcp");
                    break 'outer;
                }
            }
        }

        let ip_key = local.to_lowercase();
        let mut inner = Map::new();
        inner.insert("prefix".into(), Value::Number(prefixlen.into()));
        if !flags.is_empty() {
            inner.insert("flags".into(), Value::Array(
                flags.iter().map(|s| Value::String(s.to_string())).collect(),
            ));
        }
        let mut outer = Map::new();
        outer.insert(ip_key, Value::Object(inner));
        addresses.push(Value::Object(outer));
    }
    Some(addresses)
}

fn is_link_local(addr: &str) -> bool {
    if let Ok(ip4) = addr.parse::<Ipv4Addr>() {
        return ip4.is_link_local();
    }
    if let Ok(ip6) = addr.parse::<Ipv6Addr>() {
        let segs = ip6.segments();
        return segs[0] == 0xfe80;
    }
    false
}

fn parse_ipv6_cidr(cidr: &str) -> Option<(Ipv6Addr, u32)> {
    let parts: Vec<&str> = cidr.splitn(2, '/').collect();
    let addr: Ipv6Addr = parts[0].parse().ok()?;
    let prefix: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);
    Some((addr, prefix))
}

fn ipv6_in_network(addr: &Ipv6Addr, net: &Ipv6Addr, prefix: u32) -> bool {
    if prefix == 0 { return true; }
    if prefix > 128 { return false; }
    let addr_bits = u128::from_be_bytes(addr.octets());
    let net_bits  = u128::from_be_bytes(net.octets());
    let mask = if prefix == 128 { u128::MAX } else { !((1u128 << (128 - prefix)) - 1) };
    (addr_bits & mask) == (net_bits & mask)
}

// ── Correlate bridge/bond/vrf ─────────────────────────────────────────────────

fn correlate_members_and_uplinks(ifaces: &mut Vec<IfaceData>) {
    let uplink_types = ["bond", "bridge", "vrf"];
    let mut members_to_uplink: HashMap<String, (String, &'static str)> = HashMap::new();
    let mut uplink_to_members: HashMap<String, Vec<String>> = HashMap::new();

    let uplinks: Vec<(String, &'static str)> = ifaces.iter()
        .filter_map(|iface| {
            if let Some(t) = iface.iface_type() {
                if uplink_types.contains(&t) {
                    return Some((iface.name.clone(), t));
                }
            }
            None
        })
        .collect();

    for (name, utype) in &uplinks {
        let members = query_members(name);
        for member in &members {
            members_to_uplink.insert(member.clone(), (name.clone(), utype));
        }
        uplink_to_members.insert(name.clone(), members);
    }

    for iface in ifaces.iter_mut() {
        if let Some((uplink_name, uplink_type)) = members_to_uplink.get(&iface.name) {
            match *uplink_type {
                "bridge" => iface.bridge = Some(uplink_name.clone()),
                "bond"   => iface.bond   = Some(uplink_name.clone()),
                "vrf"    => iface.vrf    = Some(uplink_name.clone()),
                _ => {}
            }
        }
        if uplink_types.contains(&iface.iface_type().unwrap_or("")) {
            if let Some(members) = uplink_to_members.get(&iface.name) {
                iface.members = members.clone();
            }
        }
    }
}

// ── Online state ──────────────────────────────────────────────────────────────

fn query_online_state(ifaces: &[&IfaceData]) -> bool {
    for iface in ifaces {
        if !iface.is_up() { continue; }
        let has_addrs = iface.addresses.as_ref().map(|a| !a.is_empty()).unwrap_or(false);
        let has_routes = iface.routes.as_ref().map(|r| !r.is_empty()).unwrap_or(false);
        let has_dns = iface.dns_addresses.as_ref().map(|d| !d.is_empty()).unwrap_or(false);
        if !has_addrs || !has_routes || !has_dns { continue; }

        // check non-link-local IPs
        let non_local: Vec<_> = iface.addresses.as_ref().unwrap().iter().filter_map(|entry| {
            let obj = entry.as_object()?;
            let (ip, extra) = obj.iter().next()?;
            let flags = extra.get("flags")?.as_array()?;
            let is_link = flags.iter().any(|f| f.as_str() == Some("link"));
            if is_link { None } else { Some(ip.clone()) }
        }).collect();

        let has_default = iface.routes.as_ref().unwrap().iter()
            .any(|r| r.get("to").and_then(|v| v.as_str()) == Some("default"));

        if !non_local.is_empty() && has_default {
            return true;
        }
    }
    false
}

// ── JSON serializer (Python-compatible) ───────────────────────────────────────

fn python_json_dumps(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            let escaped = s
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            format!("\"{}\"", escaped)
        }
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(python_json_dumps).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => {
            let items: Vec<String> = map.iter()
                .map(|(k, v)| format!("\"{}\": {}", k, python_json_dumps(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

// ── YAML serializer (Python yaml.dump-compatible: sorted keys, null/false) ────

fn to_yaml(v: &Value, indent: usize) -> String {
    match v {
        Value::Null => "null\n".to_string(),
        Value::Bool(b) => format!("{}\n", if *b { "true" } else { "false" }),
        Value::Number(n) => format!("{}\n", n),
        Value::String(s) => format!("{}\n", yaml_scalar(s)),
        Value::Array(arr) => {
            if arr.is_empty() {
                return "[]\n".to_string();
            }
            let pad = " ".repeat(indent);
            let mut out = "\n".to_string();
            for item in arr {
                let rendered = to_yaml(item, indent + 2);
                let trimmed = rendered.trim_end_matches('\n');
                out.push_str(&format!("{}- {}\n", pad, trimmed.trim_start()));
            }
            out
        }
        Value::Object(map) => {
            if map.is_empty() {
                return "{}\n".to_string();
            }
            let pad = " ".repeat(indent);
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let mut out = "\n".to_string();
            for key in keys {
                let val = &map[key];
                let rendered = to_yaml(val, indent + 2);
                if rendered.starts_with('\n') {
                    // nested block
                    out.push_str(&format!("{}{}:{}", pad, key, rendered));
                } else {
                    out.push_str(&format!("{}{}: {}", pad, key, rendered));
                }
            }
            out
        }
    }
}

fn yaml_scalar(s: &str) -> String {
    // Strings that need quoting: empty, contain ': ', start with special chars,
    // are YAML reserved words, or contain newlines
    let needs_quote = s.is_empty()
        || s == "true" || s == "false" || s == "null" || s == "yes" || s == "no"
        || s.contains(": ")
        || s.contains('\n')
        || s.starts_with('{') || s.starts_with('[')
        || s.starts_with('\'') || s.starts_with('"')
        || s.starts_with('*') || s.starts_with('&') || s.starts_with('!')
        || s.starts_with('#') || s.starts_with('%') || s.starts_with('@');

    if needs_quote {
        let escaped = s.replace('\\', "\\\\").replace('\'', "''");
        format!("'{}'", escaped)
    } else {
        s.to_string()
    }
}

fn render_yaml(data: &Map<String, Value>) -> String {
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut out = String::new();
    for key in keys {
        let val = &data[key];
        let rendered = to_yaml(val, 2);
        if rendered.starts_with('\n') {
            out.push_str(&format!("{}:{}", key, rendered));
        } else {
            out.push_str(&format!("{}: {}", key, rendered));
        }
    }
    out
}

// ── Tabular output ────────────────────────────────────────────────────────────


const PAD: usize = 18;

fn pline(title: &str, value: &str) {
    println!("{:>pad$} {}", title, value, pad = PAD);
}

fn pretty_print(
    data: &Map<String, Value>,
    total: usize,
    ifname_filter: Option<&str>,
    verbose: bool,
) {
    // Global state
    let global = data.get("netplan-global-state").and_then(|v| v.as_object());
    if let Some(gs) = global {
        let online = gs.get("online").and_then(|v| v.as_bool()).unwrap_or(false);
        pline("Online state:", if online { "online" } else { "offline" });

        if let Some(ns) = gs.get("nameservers").and_then(|v| v.as_object()) {
            let addresses = ns.get("addresses").and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            let mode = ns.get("mode").and_then(|v| v.as_str());
            let search = ns.get("search").and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();

            for (i, addr) in addresses.iter().enumerate() {
                let title = if i == 0 { "DNS Addresses:" } else { "" };
                if let Some(m) = mode {
                    pline(title, &format!("{} ({})", addr, m));
                } else {
                    pline(title, addr);
                }
            }
            for (i, s) in search.iter().enumerate() {
                pline(if i == 0 { "DNS Search:" } else { "" }, s);
            }
        }
        println!();
    }

    // Per interface
    let interfaces: Vec<(&str, &Value)> = data.iter()
        .filter(|(k, _)| *k != "netplan-global-state")
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    let total_shown = interfaces.len();
    for (idx, (ifname, ifconfig)) in interfaces.iter().enumerate() {
        if let Some(filter) = ifname_filter {
            if *ifname != filter { continue; }
        }

        let obj = match ifconfig.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Interface header
        display_interface_header(ifname, obj);
        display_mac_address(obj);
        display_ip_addresses(obj);
        display_dns_addresses(obj);
        display_dns_search(obj);
        display_routes(obj, verbose);
        display_bridge(obj);
        display_bond(obj);
        display_vrf(obj);
        display_members(obj);
        display_activation_mode(obj);

        // newline separator between interfaces (not after the last one, unless more follow)
        if ifname_filter.is_none() && idx + 1 < total_shown {
            println!();
        }
    }

    let hidden = total.saturating_sub(total_shown);
    if hidden > 0 {
        println!();
        println!("{} inactive interfaces hidden. Use \"--all\" to show all.", hidden);
    }
}

fn display_interface_header(ifname: &str, obj: &Map<String, Value>) {
    let operstate = obj.get("operstate").and_then(|v| v.as_str()).unwrap_or("UNKNOWN");
    let adminstate = obj.get("adminstate").and_then(|v| v.as_str()).unwrap_or("UNKNOWN");
    let state = if operstate == "UP" && adminstate == "UP" {
        "UP".to_string()
    } else if operstate == "DOWN" && adminstate == "DOWN" {
        "DOWN".to_string()
    } else {
        format!("{}/{}", operstate, adminstate)
    };

    let idx = obj.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
    let full_type = {
        let t = obj.get("type").and_then(|v| v.as_str()).unwrap_or("other");
        let ssid = obj.get("ssid").and_then(|v| v.as_str());
        let tunnel_mode = obj.get("tunnel_mode").and_then(|v| v.as_str());
        if t == "wifi" {
            if let Some(ssid) = ssid {
                format!("{}/\"{}\"", t, ssid)
            } else {
                t.to_string()
            }
        } else if t == "tunnel" {
            if let Some(mode) = tunnel_mode {
                format!("{}/{}", t, mode)
            } else {
                t.to_string()
            }
        } else {
            t.to_string()
        }
    };

    let backend = obj.get("backend").and_then(|v| v.as_str()).unwrap_or("unmanaged");
    let netdef = match obj.get("id").and_then(|v| v.as_str()) {
        Some(id) => format!("{}: {}", backend, id),
        None => backend.to_string(),
    };

    println!("● {:>2}: {} {} {} ({})", idx, ifname, full_type, state, netdef);
}

fn display_mac_address(obj: &Map<String, Value>) {
    if let Some(mac) = obj.get("macaddress").and_then(|v| v.as_str()) {
        let vendor = obj.get("vendor").and_then(|v| v.as_str());
        if let Some(v) = vendor {
            pline("MAC Address:", &format!("{} ({})", mac, v));
        } else {
            pline("MAC Address:", mac);
        }
    }
}

fn display_ip_addresses(obj: &Map<String, Value>) {
    let addrs = match obj.get("addresses").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return,
    };
    for (i, entry) in addrs.iter().enumerate() {
        let title = if i == 0 { "Addresses:" } else { "" };
        if let Some(map) = entry.as_object() {
            if let Some((ip, extra)) = map.iter().next() {
                let prefix = extra.get("prefix").and_then(|v| v.as_u64()).unwrap_or(0);
                let flags: Vec<&str> = extra.get("flags")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let addr_str = format!("{}/{}", ip, prefix);
                if flags.is_empty() {
                    pline(title, &addr_str);
                } else {
                    pline(title, &format!("{} ({})", addr_str, flags.join(", ")));
                }
            }
        }
    }
}

fn display_dns_addresses(obj: &Map<String, Value>) {
    let addrs = match obj.get("dns_addresses").and_then(|v| v.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return,
    };
    for (i, addr) in addrs.iter().enumerate() {
        if let Some(s) = addr.as_str() {
            pline(if i == 0 { "DNS Addresses:" } else { "" }, s);
        }
    }
}

fn display_dns_search(obj: &Map<String, Value>) {
    let search = match obj.get("dns_search").and_then(|v| v.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return,
    };
    for (i, s) in search.iter().enumerate() {
        if let Some(domain) = s.as_str() {
            pline(if i == 0 { "DNS Search:" } else { "" }, domain);
        }
    }
}

fn display_routes(obj: &Map<String, Value>, verbose: bool) {
    let routes = match obj.get("routes").and_then(|v| v.as_array()) {
        Some(r) if !r.is_empty() => r,
        _ => return,
    };
    let mut displayed = 0;
    for route in routes {
        let r = match route.as_object() {
            Some(r) => r,
            None => continue,
        };
        let table_id = r.get("table").and_then(|v| v.as_str()).unwrap_or("main");
        // non-verbose: only main table (table==254 or table=="main")
        if !verbose {
            let is_main = table_id == "main" || table_id == "254";
            if !is_main { continue; }
        }

        let to = r.get("to").and_then(|v| v.as_str()).unwrap_or("");
        let via = r.get("via").and_then(|v| v.as_str()).unwrap_or("");
        let from = r.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let metric = r.get("metric").and_then(|v| v.as_u64());
        let protocol = r.get("protocol").and_then(|v| v.as_str()).unwrap_or("");
        let scope = r.get("scope").and_then(|v| v.as_str()).unwrap_or("");
        let rtype = r.get("type").and_then(|v| v.as_str()).unwrap_or("");

        let mut route_str = to.to_string();
        if !via.is_empty() { route_str.push_str(&format!(" via {}", via)); }
        if !from.is_empty() { route_str.push_str(&format!(" from {}", from)); }
        if let Some(m) = metric { route_str.push_str(&format!(" metric {}", m)); }
        if verbose {
            route_str.push_str(&format!(" table {}", table_id));
        }

        let mut extra = vec![];
        if !protocol.is_empty() && protocol != "kernel" { extra.push(protocol); }
        if !scope.is_empty() && scope != "global" { extra.push(scope); }
        if !rtype.is_empty() && rtype != "unicast" { extra.push(rtype); }

        let title = if displayed == 0 { "Routes:" } else { "" };
        if extra.is_empty() {
            pline(title, &route_str);
        } else {
            pline(title, &format!("{} ({})", route_str, extra.join(", ")));
        }
        displayed += 1;
    }
}

fn display_bridge(obj: &Map<String, Value>) {
    if let Some(b) = obj.get("bridge").and_then(|v| v.as_str()) {
        pline("Bridge:", b);
    }
}

fn display_bond(obj: &Map<String, Value>) {
    if let Some(b) = obj.get("bond").and_then(|v| v.as_str()) {
        pline("Bond:", b);
    }
}

fn display_vrf(obj: &Map<String, Value>) {
    if let Some(v) = obj.get("vrf").and_then(|v| v.as_str()) {
        pline("VRF:", v);
    }
}

fn display_members(obj: &Map<String, Value>) {
    let members = match obj.get("interfaces").and_then(|v| v.as_array()) {
        Some(m) if !m.is_empty() => m,
        _ => return,
    };
    for (i, m) in members.iter().enumerate() {
        if let Some(name) = m.as_str() {
            pline(if i == 0 { "Interfaces:" } else { "" }, name);
        }
    }
}

fn display_activation_mode(obj: &Map<String, Value>) {
    if let Some(mode) = obj.get("activation_mode").and_then(|v| v.as_str()) {
        pline("Activation Mode:", mode);
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: StatusArgs) -> Result<()> {
    // --diff-only implies --diff, both need all interfaces
    let show_all = args.all || args.diff || args.diff_only;

    // Gather system data
    let iproute2 = query_iproute2().context("Cannot query iproute2")?;
    let networkd = query_networkd().context("Cannot query systemd-networkd")?;
    if iproute2.is_empty() || networkd.is_empty() {
        eprintln!("Could not query iproute2 or systemd-networkd");
        std::process::exit(1);
    }

    let nm_data = query_nm();
    let (routes4, routes6) = query_routes();
    let (dns_addresses, dns_search) = query_resolved();

    // Build interface list
    let mut ifaces: Vec<IfaceData> = iproute2.iter()
        .map(|ip| build_iface(ip, &networkd, &nm_data, &dns_addresses, &dns_search, &routes4, &routes6))
        .collect();

    correlate_members_and_uplinks(&mut ifaces);

    // Filter for online state: non-DOWN interfaces
    let active: Vec<&IfaceData> = ifaces.iter().filter(|i| i.operstate != "DOWN").collect();
    let online = query_online_state(&active);

    let total = ifaces.len();

    // Apply interface name filter
    if let Some(ifname) = &args.ifname {
        if !ifaces.iter().any(|i| i.name == *ifname) {
            eprintln!("Could not find interface {}", ifname);
            std::process::exit(1);
        }
    }

    // Build state map (insertion order via serde_json preserve_order)
    let mut state: Map<String, Value> = Map::new();

    let mut global = Map::new();
    global.insert("online".into(), Value::Bool(online));
    global.insert("nameservers".into(), Value::Object(resolvconf_json(&args.root_dir)));
    state.insert("netplan-global-state".into(), Value::Object(global));

    let iter: Box<dyn Iterator<Item = &IfaceData>> = if show_all {
        Box::new(ifaces.iter())
    } else if let Some(target) = &args.ifname {
        // When a specific interface is requested: include it even if DOWN
        let target = target.clone();
        Box::new(ifaces.iter().filter(move |i| i.name == target))
    } else {
        Box::new(ifaces.iter().filter(|i| i.operstate != "DOWN"))
    };

    for iface in iter {
        state.insert(iface.name.clone(), Value::Object(iface.to_json_obj()));
    }

    let format = args.format.to_lowercase();
    match format.as_str() {
        "json" => {
            println!("{}", python_json_dumps(&Value::Object(state)));
        }
        "yaml" => {
            print!("{}", render_yaml(&state));
        }
        _ => {
            // tabular
            pretty_print(&state, total, args.ifname.as_deref(), args.verbose);
        }
    }

    Ok(())
}
