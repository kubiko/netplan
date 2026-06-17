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

//! Wrappers around `networkctl`.

use std::process::Command;

use anyhow::{Context, Result};

use super::check_cmd;

pub fn reload() -> Result<()> {
    check_cmd("networkctl", &["reload"]).context("networkctl reload failed")
}

pub fn reconfigure(ifaces: &[String]) -> Result<()> {
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
pub fn managed_interfaces() -> Vec<String> {
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
