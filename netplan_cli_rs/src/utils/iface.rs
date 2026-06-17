// Copyright (C) 2026 Canonical, Ltd.
// SPDX-License-Identifier: GPL-3.0-only
//
// This program is free software; you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation; version 3.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

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
pub fn all() -> Vec<(String, Option<String>, Option<String>)> {
    names()
        .into_iter()
        .map(|name| {
            let m = mac(&name);
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
