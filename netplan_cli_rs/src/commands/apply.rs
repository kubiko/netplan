//! `netplan apply` – apply current netplan config to the running system.
//!
//! Mirrors `netplan_cli/cli/commands/apply.py`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Result};
use clap::Args;

use crate::{netplan, utils};

const IF_NAMESIZE: usize = 16;

#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// Only apply SR-IOV related configuration and exit
    #[arg(long)]
    sriov_only: bool,

    /// Only clean up old OpenVSwitch interfaces and exit
    #[arg(long)]
    only_ovs_cleanup: bool,

    /// Directory containing previous YAML configuration (for virtual link cleanup)
    #[arg(long)]
    state: Option<String>,
}

pub fn run(args: ApplyArgs) -> Result<()> {
    // SR-IOV-only path (stub — not exposed through public libnetplan API)
    if args.sriov_only {
        eprintln!("[netplan] SR-IOV config apply is not supported in the Rust CLI");
        return Ok(());
    }

    // OVS-cleanup-only path (stub)
    if args.only_ovs_cleanup {
        eprintln!("[netplan] OVS cleanup is not supported in the Rust CLI");
        return Ok(());
    }

    // ── SNAP environment: delegate to D-Bus ──────────────────────────────────
    if std::env::var("SNAP").is_ok() {
        let busctl = which("busctl")?;
        let rc = Command::new(&busctl)
            .args([
                "call",
                "--quiet",
                "--system",
                "io.netplan.Netplan",
                "/io/netplan/Netplan",
                "io.netplan.Netplan",
                "Apply",
            ])
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run busctl: {}", e))?
            .code()
            .unwrap_or(1);

        if rc == 130 {
            bail!("PermissionError: failed to communicate with dbus service");
        } else if rc != 0 {
            bail!("failed to communicate with dbus service: error {}", rc);
        }
        return Ok(());
    }

    let ovs_cleanup_path = format!(
        "{}{}",
        utils::GENERATOR_LATE_DIR,
        utils::OVS_CLEANUP_SERVICE
    );

    // ── Snapshot pre-generate state ───────────────────────────────────────────
    let old_files_networkd = !utils::glob_paths("/run/systemd/network/*netplan-*").is_empty();

    let mut old_ovs_glob =
        utils::glob_paths(&format!("{}netplan-ovs-*", utils::GENERATOR_LATE_DIR));
    old_ovs_glob.retain(|p| p != &ovs_cleanup_path);
    let old_files_ovs = !old_ovs_glob.is_empty();

    let old_nm_glob = utils::glob_paths("/run/NetworkManager/system-connections/netplan-*");
    let old_files_nm = !old_nm_glob.is_empty();

    // Collect interface names for NM interface detection (pre-generate snapshot)
    let pre_devices = utils::get_interfaces();
    let pre_names: Vec<String> = pre_devices.iter().map(|(n, _, _)| n.clone()).collect();
    let mut nm_ifaces: HashSet<String> = utils::nm_interfaces(&old_nm_glob, &pre_names)
        .into_iter()
        .cloned()
        .collect();

    // ── Run configure + daemon-reload ─────────────────────────────────────────
    let configure = utils::get_configure_path();
    let rc = Command::new(&configure)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run configure ({}): {}", configure, e))?
        .code()
        .unwrap_or(1);
    if rc != 0 {
        std::process::exit(78); // EX_CONFIG
    }
    utils::systemctl_daemon_reload()?;

    // ── Re-glob to determine what needs restarting ────────────────────────────
    let restart_networkd_new = !utils::glob_paths("/run/systemd/network/*netplan-*").is_empty();
    let mut restart_networkd = restart_networkd_new || old_files_networkd;

    let mut restart_ovs_glob =
        utils::glob_paths(&format!("{}netplan-ovs-*", utils::GENERATOR_LATE_DIR));
    restart_ovs_glob.retain(|p| p != &ovs_cleanup_path);
    let restart_ovs = !restart_ovs_glob.is_empty();
    if !restart_ovs && old_files_ovs {
        restart_networkd = true;
    }

    let restart_nm_glob = utils::glob_paths("/run/NetworkManager/system-connections/netplan-*");
    // Accumulate NM ifaces from the post-generate glob (still using pre-stop device list)
    for iface in utils::nm_interfaces(&restart_nm_glob, &pre_names) {
        nm_ifaces.insert(iface.clone());
    }
    let restart_nm = !restart_nm_glob.is_empty() || old_files_nm;

    // ── Stop backends that need restarting ────────────────────────────────────
    if restart_networkd {
        utils::systemctl("stop", &["netplan-wpa-*.service"], false);
    }

    let mut loopback_connection = String::new();
    if restart_nm && utils::nm_running() {
        if nm_ifaces.contains("lo") {
            loopback_connection = utils::nm_get_connection_for_interface("lo");
        }
        // Disconnect NM-managed interfaces before stopping NM
        for name in &pre_names {
            if nm_ifaces.contains(name.as_str()) {
                utils::nmcli(&["device", "disconnect", name]);
            }
        }
        utils::systemctl_network_manager("stop", false);
    }

    // ── Refresh devices after stops ───────────────────────────────────────────
    let devices: Vec<(String, String, Option<String>)> = utils::get_interfaces();
    let device_names: Vec<String> = devices.iter().map(|(n, _, _)| n.clone()).collect();

    // ── Parse current config and process link changes ─────────────────────────
    let state = netplan::load_state("/")?;
    let changes = process_link_changes(&devices, &state);

    // ── Delete virtual links removed from config ──────────────────────────────
    if let Some(ref state_dir) = args.state {
        if let Ok(prev_state) = netplan::load_state(state_dir) {
            let prev_links: Vec<String> = prev_state
                .iter_netdefs()
                .filter(|nd| nd.is_virtual())
                .map(|nd| nd.id())
                .collect();
            let curr_links: Vec<String> = state
                .iter_netdefs()
                .filter(|nd| nd.is_virtual())
                .map(|nd| nd.id())
                .collect();
            clear_virtual_links(&prev_links, &curr_links, &device_names);
        }
    }

    // ── Trigger .link rules via udevadm ───────────────────────────────────────
    for (dev, _, _) in &devices {
        let syspath = format!("/sys/class/net/{}", dev);
        let _ = Command::new("udevadm")
            .args(["test-builtin", "net_setup_link", &syspath])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("udevadm")
            .args(["test", &syspath])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    let devices_after_udev = utils::get_interface_names();

    // ── Apply interface renames ────────────────────────────────────────────────
    for (iface, new_name) in &changes {
        if new_name.len() >= IF_NAMESIZE {
            eprintln!(
                "[netplan] Interface name {} is too long; {} will not be renamed",
                new_name, iface
            );
            continue;
        }
        // Skip if rename already happened via udevadm
        if device_names.contains(iface) && devices_after_udev.contains(new_name) {
            continue;
        }
        let _ = Command::new("ip")
            .args(["link", "set", "dev", iface, "down"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("ip")
            .args(["link", "set", "dev", iface, "name", new_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // ── udevadm reload + trigger ──────────────────────────────────────────────
    let _ = Command::new("udevadm")
        .args(["control", "--reload"])
        .status();
    // Returns 1 in containers (LP: #2095203) — ignore the error
    let _ = Command::new("udevadm")
        .args([
            "trigger",
            "--action=move",
            "--subsystem-match=net",
            "--settle",
        ])
        .status();

    // ── Regulatory domain ─────────────────────────────────────────────────────
    if Path::new(&format!(
        "{}netplan-regdom.service",
        utils::GENERATOR_LATE_DIR
    ))
    .exists()
    {
        utils::systemctl("start", &["netplan-regdom.service"], false);
    }

    // ── (Re)start networkd backend ────────────────────────────────────────────
    if restart_networkd {
        let netplan_wpa = glob_wants_services(utils::GENERATOR_LATE_DIR, "netplan-wpa-*.service");
        let netplan_ovs: Vec<String> =
            glob_wants_services(utils::GENERATOR_LATE_DIR, "netplan-ovs-*.service")
                .into_iter()
                .filter(|name| name != utils::OVS_CLEANUP_SERVICE)
                .collect();

        // networkctl reload/reconfigure; fall back to hard restart if it fails
        if utils::networkctl_reload().is_err()
            || utils::networkctl_reconfigure(&utils::networkd_interfaces()).is_err()
        {
            eprintln!("[netplan] Falling back to hard restart of systemd-networkd.service");
            utils::systemctl("restart", &["systemd-networkd.service"], true);
        }

        // 1st: OVS cleanup (synchronous, avoids races)
        utils::systemctl("start", &[utils::OVS_CLEANUP_SERVICE], true);

        // 2nd: WPA + other OVS services (synchronous for oneshot units)
        let start: Vec<&str> = netplan_wpa
            .iter()
            .chain(netplan_ovs.iter())
            .map(String::as_str)
            .collect();
        if !start.is_empty() {
            utils::systemctl("start", &start, true);
        }
    }

    // ── (Re)start NetworkManager backend ──────────────────────────────────────
    if restart_nm {
        // Use the refreshed (post-stop) device list for final NM interface detection
        let nm_ifaces_final: Vec<String> = utils::nm_interfaces(&restart_nm_glob, &device_names)
            .into_iter()
            .cloned()
            .collect();

        for iface in &nm_ifaces_final {
            utils::ip_addr_flush(iface);
        }

        // Clear NM runtime state (NM_UNMANAGED udev rules etc.)
        let _ = std::fs::remove_dir_all("/run/NetworkManager/devices");

        utils::systemctl_network_manager("start", false);

        // If 'lo' was managed by NM, wait for NM to be ready then bring it back
        let nm_ifaces_set: HashSet<&str> = nm_ifaces_final.iter().map(String::as_str).collect();

        if nm_ifaces_set.contains("lo") {
            // Wait up to ~5 s for NM to report connected
            for _ in 0..10 {
                let out = Command::new("nmcli").args(["general", "status"]).output();
                match out {
                    Ok(o) if o.status.code() == Some(8) => {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    }
                    Ok(o) => {
                        if String::from_utf8_lossy(&o.stdout).contains("\nconnected") {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    Err(_) => break,
                }
            }

            if !loopback_connection.is_empty() {
                let _ = Command::new("nmcli")
                    .args(["connection", "up", &loopback_connection])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    Ok(())
}

// ── process_link_changes ──────────────────────────────────────────────────────

/// Find physical interfaces that have a `set-name` + `match:` stanza and
/// return a map of `current_name -> new_name` for those that need renaming.
fn process_link_changes(
    interfaces: &[(String, String, Option<String>)],
    state: &netplan::State,
) -> HashMap<String, String> {
    let mut changes = HashMap::new();

    for netdef in state.iter_netdefs() {
        if !netdef.is_physical() {
            continue;
        }
        let Some(new_name) = netdef.set_name() else {
            continue;
        };
        if !netdef.has_match() {
            continue;
        }

        let Some(current_iface) = find_matching_iface(interfaces, &netdef) else {
            eprintln!(
                "[netplan] Cannot find unique matching interface for {}",
                netdef.id()
            );
            continue;
        };

        if current_iface == new_name {
            continue; // already correctly named
        }

        changes.insert(current_iface, new_name);
    }

    changes
}

/// Return the single interface from `interfaces` whose name, MAC, and driver
/// all satisfy `netdef`'s match rules.  Returns `None` when zero or multiple
/// interfaces match (ambiguous).
fn find_matching_iface(
    interfaces: &[(String, String, Option<String>)],
    netdef: &netplan::NetDef,
) -> Option<String> {
    let candidates: Vec<&String> = interfaces
        .iter()
        .filter(|(name, mac, driver)| {
            let mac_opt = if mac.is_empty() {
                None
            } else {
                Some(mac.as_str())
            };
            netdef.matches_interface(name, mac_opt, driver.as_deref())
        })
        .map(|(name, _, _)| name)
        .collect();

    if candidates.len() == 1 {
        Some(candidates[0].clone())
    } else {
        None
    }
}

// ── clear_virtual_links ───────────────────────────────────────────────────────

/// Delete virtual interfaces that were in the previous config but are absent
/// from the current config, if they still exist on the system.
fn clear_virtual_links(prev: &[String], curr: &[String], devices: &[String]) {
    if devices.is_empty() {
        eprintln!("[netplan] Cannot clear virtual links: no network interfaces provided.");
        return;
    }
    let curr_set: HashSet<&str> = curr.iter().map(String::as_str).collect();
    let dev_set: HashSet<&str> = devices.iter().map(String::as_str).collect();

    for link in prev {
        if curr_set.contains(link.as_str()) || !dev_set.contains(link.as_str()) {
            continue;
        }
        let rc = Command::new("ip")
            .args(["link", "delete", "dev", link])
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1);
        if rc != 0 {
            eprintln!("[netplan] Could not delete interface {}", link);
        }
    }
}

// ── Glob helpers ──────────────────────────────────────────────────────────────

/// Collect the basenames of files matching `file_pat` inside every `*.wants/`
/// subdirectory of `base_dir`.
///
/// Equivalent to `[os.path.basename(f) for f in glob.glob(base_dir + "*.wants/" + file_pat)]`.
fn glob_wants_services(base_dir: &str, file_pat: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(base_dir) else {
        return vec![];
    };

    let mut results = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".wants") {
            continue;
        }
        let Ok(sub) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for svc in sub.flatten() {
            let svc_name = svc.file_name().to_string_lossy().into_owned();
            if utils::fnmatch(file_pat, &svc_name) {
                results.push(svc_name);
            }
        }
    }
    results
}

// ── which ─────────────────────────────────────────────────────────────────────

fn which(name: &str) -> Result<String> {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/snap/bin".to_string());
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(name);
        if candidate.exists() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    bail!("'{}' not found in PATH", name)
}
