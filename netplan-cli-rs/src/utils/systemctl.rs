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

//! Wrappers around `systemctl`.

use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use super::{check_cmd, run_cmd};

pub const NM_SERVICE_NAME: &str = "NetworkManager.service";
pub const NM_SNAP_SERVICE_NAME: &str = "snap.network-manager.networkmanager.service";

/// Equivalent to Python's `utils.systemctl(action, services, sync)`.
///
/// Uses `--no-block` when `sync=false`. Ignores errors (best-effort),
/// matching Python behaviour where service restarts may fail gracefully.
pub fn run(action: &str, services: &[&str], sync: bool) {
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

pub fn daemon_reload() -> Result<()> {
    check_cmd("systemctl", &["daemon-reload", "--no-ask-password"])
        .context("systemctl daemon-reload failed")
}

pub fn is_enabled(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--quiet", "is-enabled", unit])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start or stop NetworkManager, using the snap service name if the snap is
/// enabled.
pub fn network_manager(action: &str, sync: bool) {
    let svc = if is_enabled(NM_SNAP_SERVICE_NAME) {
        NM_SNAP_SERVICE_NAME
    } else {
        NM_SERVICE_NAME
    };
    run(action, &[svc], sync);
}
