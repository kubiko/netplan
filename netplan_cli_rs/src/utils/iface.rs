//! `/sys/class/net` interface discovery and address manipulation.

use std::fs;
use std::process::{Command, Stdio};

/// Returns all current interface names from `/sys/class/net/`.
pub fn names() -> Vec<String> {
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
pub fn mac(iface: &str) -> Option<String> {
    fs::read_to_string(format!("/sys/class/net/{}/address", iface))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read driver name via `readlink /sys/class/net/<iface>/device/driver`.
pub fn driver(iface: &str) -> Option<String> {
    fs::read_link(format!("/sys/class/net/{}/device/driver", iface))
        .ok()
        .and_then(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
}

/// Returns `(ifname, mac, driver)` tuples for all current interfaces.
pub fn all() -> Vec<(String, String, Option<String>)> {
    names()
        .into_iter()
        .map(|name| {
            let m = mac(&name).unwrap_or_default();
            let d = driver(&name);
            (name, m, d)
        })
        .collect()
}

/// Flush all addresses from `iface` (`ip addr flush <iface>`).
pub fn flush_addrs(iface: &str) {
    let _ = Command::new("ip")
        .args(["addr", "flush", iface])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
