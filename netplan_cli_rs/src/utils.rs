//! System-level helpers: subprocess wrappers, sysfs reads, interface discovery.
//!
//! Mirrors the utility functions in `netplan_cli/cli/utils.py`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

// ── Paths ─────────────────────────────────────────────────────────────────────

pub const GENERATOR_LATE_DIR: &str = "/run/systemd/generator.late/";
/// Relative to a root dir; prepend rootdir before use.
pub const TRY_READY_STAMP: &str = "run/netplan/netplan-try.ready";

pub const NM_SERVICE_NAME: &str = "NetworkManager.service";
pub const NM_SNAP_SERVICE_NAME: &str = "snap.network-manager.networkmanager.service";
pub const OVS_CLEANUP_SERVICE: &str = "netplan-ovs-cleanup.service";

pub fn get_configure_path() -> String {
    std::env::var("NETPLAN_CONFIGURE_PATH")
        .unwrap_or_else(|_| "/usr/libexec/netplan/configure".to_string())
}

pub fn get_generator_path() -> String {
    std::env::var("NETPLAN_GENERATE_PATH")
        .unwrap_or_else(|_| "/usr/libexec/netplan/generate".to_string())
}

/// Return the path to the C generator binary.
///
/// When `NETPLAN_GENERATE_PATH` is set to the Rust wrapper binary itself
/// (as in tests), we must NOT call ourselves recursively.  Instead, derive
/// the C binary's path from `NETPLAN_CONFIGURE_PATH`: configure and generate
/// live in the same directory in both installed and meson-build layouts.
pub fn get_c_generator_path() -> String {
    let configure = std::env::var("NETPLAN_CONFIGURE_PATH")
        .unwrap_or_else(|_| "/usr/libexec/netplan/configure".to_string());
    let gen = std::path::Path::new(&configure).with_file_name("generate");
    if gen.exists() {
        return gen.to_string_lossy().into_owned();
    }
    "/usr/libexec/netplan/generate".to_string()
}

// ── Subprocess helpers ────────────────────────────────────────────────────────

/// Run `program args…` and return the exit code.  stderr is inherited.
pub fn run_cmd(program: &str, args: &[&str]) -> i32 {
    Command::new(program)
        .args(args)
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(1)
}

/// Run `program args…` and fail if the exit code is non-zero.
pub fn check_cmd(program: &str, args: &[&str]) -> Result<()> {
    let rc = run_cmd(program, args);
    if rc != 0 {
        bail!("'{}' exited with code {}", program, rc);
    }
    Ok(())
}

/// Equivalent to Python's `utils.systemctl(action, services, sync)`.
///
/// Uses `--no-block` when `sync=false`.  Ignores errors (best-effort),
/// matching Python behaviour where service restarts may fail gracefully.
pub fn systemctl(action: &str, services: &[&str], sync: bool) {
    if services.is_empty() {
        return;
    }
    let mut args = vec!["--no-ask-password", action];
    if !sync {
        args.push("--no-block");
    }
    args.extend_from_slice(services);
    let _ = run_cmd("systemctl", &args);
}

pub fn systemctl_daemon_reload() -> Result<()> {
    check_cmd("systemctl", &["daemon-reload", "--no-ask-password"])
        .context("systemctl daemon-reload failed")
}

pub fn systemctl_is_enabled(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--quiet", "is-enabled", unit])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start or stop NetworkManager, using the snap service name if the snap is
/// enabled (mirrors `utils.systemctl_network_manager`).
pub fn systemctl_network_manager(action: &str, sync: bool) {
    let svc = if systemctl_is_enabled(NM_SNAP_SERVICE_NAME) {
        NM_SNAP_SERVICE_NAME
    } else {
        NM_SERVICE_NAME
    };
    systemctl(action, &[svc], sync);
}

pub fn networkctl_reload() -> Result<()> {
    check_cmd("networkctl", &["reload"]).context("networkctl reload failed")
}

pub fn networkctl_reconfigure(ifaces: &[String]) -> Result<()> {
    if ifaces.is_empty() {
        return Ok(());
    }
    let mut args = vec!["reconfigure"];
    let refs: Vec<&str> = ifaces.iter().map(String::as_str).collect();
    args.extend_from_slice(&refs);
    check_cmd("networkctl", &args).context("networkctl reconfigure failed")
}

/// Returns the link indices of networkd-managed interfaces by parsing
/// `networkctl --no-pager --no-legend` output.
///
/// Returns index strings (e.g. `["1", "2"]`) that `networkctl reconfigure`
/// accepts as interface identifiers.
pub fn networkd_interfaces() -> Vec<String> {
    let out = Command::new("networkctl")
        .args(["--no-pager", "--no-legend"])
        .output()
        .ok();
    let Some(out) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let idx = parts.next()?;
            if !idx.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            // last field is the setup state; skip "unmanaged" and "linger"
            let state = line.split_whitespace().last()?;
            if matches!(state, "unmanaged" | "linger") {
                return None;
            }
            Some(idx.to_string())
        })
        .collect()
}

pub fn nm_running() -> bool {
    Command::new("nmcli")
        .arg("general")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn nmcli(args: &[&str]) {
    let _ = Command::new("nmcli")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn nm_get_connection_for_interface(iface: &str) -> String {
    let out = Command::new("nmcli")
        .args(["-m", "tabular", "-f", "GENERAL.CONNECTION", "device", "show", iface])
        .output()
        .ok();
    let Some(out) = out else { return String::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    // Output is two lines: header then value
    let value = text.lines().nth(1).unwrap_or("").trim();
    if value == "--" { String::new() } else { value.to_string() }
}

pub fn ip_addr_flush(iface: &str) {
    let _ = Command::new("ip")
        .args(["addr", "flush", iface])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ── Interface / sysfs helpers ─────────────────────────────────────────────────

/// Returns all current interface names from `/sys/class/net/`.
pub fn get_interface_names() -> Vec<String> {
    fs::read_dir("/sys/class/net")
        .ok()
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Read MAC address from `/sys/class/net/<iface>/address`.
pub fn get_interface_mac(iface: &str) -> Option<String> {
    fs::read_to_string(format!("/sys/class/net/{}/address", iface))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read driver name via `readlink /sys/class/net/<iface>/device/driver`.
pub fn get_interface_driver(iface: &str) -> Option<String> {
    fs::read_link(format!("/sys/class/net/{}/device/driver", iface))
        .ok()
        .and_then(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
}

/// Returns `(ifname, mac, driver)` tuples for all current interfaces.
pub fn get_interfaces() -> Vec<(String, String, Option<String>)> {
    get_interface_names()
        .into_iter()
        .map(|name| {
            let mac = get_interface_mac(&name).unwrap_or_default();
            let driver = get_interface_driver(&name);
            (name, mac, driver)
        })
        .collect()
}

// ── NetworkManager connection file parsing ────────────────────────────────────

/// Parse NM `.nmconnection` files at `paths` and return the set of interface
/// names found in `interface-name=` lines, filtered to `devices`.
///
/// Mirrors `utils.nm_interfaces(paths, devices)`.
pub fn nm_interfaces<'a>(paths: &[String], devices: &'a [String]) -> Vec<&'a String> {
    let mut matched = std::collections::HashSet::new();

    for path in paths {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                if let Some(value) = line.trim().strip_prefix("interface-name=") {
                    // Apply shell glob matching against device list
                    for dev in devices {
                        if fnmatch(value, dev) {
                            matched.insert(dev);
                        }
                    }
                    break; // one interface-name per file
                }
            }
        }
    }

    matched.into_iter().collect()
}

/// Minimal `fnmatch`-style glob matching: supports `*` and `?` wildcards.
pub fn fnmatch(pattern: &str, name: &str) -> bool {
    fn inner(pat: &[u8], s: &[u8]) -> bool {
        match (pat.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                // try matching zero or more characters
                inner(&pat[1..], s) || (!s.is_empty() && inner(pat, &s[1..]))
            }
            (Some(b'?'), Some(_)) => inner(&pat[1..], &s[1..]),
            (Some(p), Some(c)) if p == c => inner(&pat[1..], &s[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

// ── Glob helpers ──────────────────────────────────────────────────────────────

/// Return all file paths matching a glob pattern like `/run/systemd/network/*netplan-*`.
///
/// Supports a single `*` wildcard in the filename component only.
pub fn glob_paths(pattern: &str) -> Vec<String> {
    let path = Path::new(pattern);
    let dir = path.parent().unwrap_or(Path::new("/"));
    let file_pat = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("*");

    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };

    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if fnmatch(file_pat, &name) {
                Some(e.path().to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect()
}
