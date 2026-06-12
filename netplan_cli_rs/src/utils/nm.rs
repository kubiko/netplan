//! Wrappers around `nmcli` and NetworkManager connection-file parsing.

use std::collections::HashSet;
use std::fs;
use std::process::{Command, Stdio};

use super::fnmatch;

/// Whether NetworkManager is running and reachable via `nmcli`.
pub fn running() -> bool {
    Command::new("nmcli")
        .arg("general")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `nmcli args…`. Errors are logged (via the `log` crate) but otherwise
/// ignored, matching Python's best-effort behaviour for these calls.
pub fn run(args: &[&str]) {
    match Command::new("nmcli")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) if !out.status.success() => {
            log::warn!(
                "nmcli {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => log::warn!("failed to run nmcli {}: {}", args.join(" "), e),
        _ => {}
    }
}

/// Returns the NM connection name currently active on `iface`, or `""` if
/// none.
pub fn connection_for_interface(iface: &str) -> String {
    let out = Command::new("nmcli")
        .args([
            "-m",
            "tabular",
            "-f",
            "GENERAL.CONNECTION",
            "device",
            "show",
            iface,
        ])
        .output()
        .ok();
    let Some(out) = out else { return String::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    // Output is two lines: header then value
    let value = text.lines().nth(1).unwrap_or("").trim();
    if value == "--" {
        String::new()
    } else {
        value.to_string()
    }
}

/// Parse NM `.nmconnection` files at `paths` and return the set of interface
/// names found in `interface-name=` lines, filtered to `devices`.
///
/// Mirrors `utils.nm_interfaces(paths, devices)`.
pub fn interfaces<'a>(paths: &[String], devices: &'a [String]) -> Vec<&'a String> {
    let mut matched = HashSet::new();

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
