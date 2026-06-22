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

//! System-level helpers: subprocess wrappers, path resolution, sysfs reads,
//! glob/fnmatch.
//!
//! Mirrors the utility functions in `netplan_cli/cli/utils.py`.

pub mod iface;
pub mod networkd;
pub mod nm;
pub mod process;
pub mod systemctl;

use std::path::{Path, PathBuf};

// ── Paths ─────────────────────────────────────────────────────────────────────

pub const GENERATOR_LATE_DIR: &str = "/run/systemd/generator.late/";
/// Relative to a root dir; prepend rootdir before use.
pub const TRY_READY_STAMP: &str = "run/netplan/netplan-try.ready";

pub const OVS_CLEANUP_SERVICE: &str = "netplan-ovs-cleanup.service";

/// Locate the `configure` helper binary: try `NETPLAN_CONFIGURE_PATH`, then
/// the meson build tree next to it, then common install locations.
pub fn configure_path() -> PathBuf {
    resolve_helper_path(
        "NETPLAN_CONFIGURE_PATH",
        "/usr/libexec/netplan/configure",
        "build/src/configure",
        "/usr/lib/netplan/configure",
    )
}

/// Locate the `generate` helper binary, with the same fallback strategy as
/// [`configure_path`].
pub fn generator_path() -> PathBuf {
    resolve_helper_path(
        "NETPLAN_GENERATE_PATH",
        "/usr/libexec/netplan/generate",
        "build/src/generate",
        "/usr/lib/netplan/generate",
    )
}

fn resolve_helper_path(
    env_var: &str,
    default: &str,
    build_tree_suffix: &str,
    lib_fallback: &str,
) -> PathBuf {
    let configured = PathBuf::from(std::env::var(env_var).unwrap_or_else(|_| default.to_string()));

    if configured.exists() {
        return configured;
    }

    // When the configured path doesn't exist (e.g. tests pointing at the
    // project root before the C generator is installed there), try the
    // meson build tree and common system install locations as fallbacks.
    let build_tree = configured.parent().map(|p| p.join(build_tree_suffix));

    build_tree
        .into_iter()
        .chain([PathBuf::from(default), PathBuf::from(lib_fallback)])
        .find(|p| p.exists())
        .unwrap_or(configured)
}

/// Return the path to the C generator binary.
///
/// When `NETPLAN_GENERATE_PATH` is set to the Rust wrapper binary itself
/// (as in tests), we must NOT call ourselves recursively.  Instead, derive
/// the C binary's path from `NETPLAN_CONFIGURE_PATH`: configure and generate
/// live in the same directory in both installed and meson-build layouts.
pub fn c_generator_path() -> PathBuf {
    let configure = std::env::var("NETPLAN_CONFIGURE_PATH")
        .unwrap_or_else(|_| "/usr/libexec/netplan/configure".to_string());
    let gen = Path::new(&configure).with_file_name("generate");
    if gen.exists() {
        return gen;
    }
    PathBuf::from("/usr/libexec/netplan/generate")
}

/// Find `name` in `$PATH`, returning its full path if found.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/snap/bin".to_string());
    path.split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.exists())
}

// ── Dotted key paths ─────────────────────────────────────────────────────────

/// Split a dotted key path on `.` but not on `\.`.
/// Each part has `\.` sequences replaced with `.`.
///
/// Equivalent to:
/// ```python
/// [s.replace(r'\.', '.') for s in re.split(r'(?<!\\)\.', key)]
/// ```
pub fn split_dotted_path(key: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = key.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'.') => {
                chars.next(); // consume the escaped dot
                current.push('.');
            }
            '.' => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}

// ── Glob helpers ──────────────────────────────────────────────────────────────

/// Minimal `fnmatch`-style glob matching: supports `*` and `?` wildcards.
///
/// Iterative two-pointer matcher: `star` remembers the most recent `*` in
/// the pattern together with the name position it was first tried against,
/// so a failed match can be retried by consuming one more character of
/// `name` instead of recursing.
pub fn fnmatch(pattern: &str, name: &str) -> bool {
    let pat = pattern.as_bytes();
    let s = name.as_bytes();

    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;

    while si < s.len() {
        match pat.get(pi) {
            Some(b'?') => {
                pi += 1;
                si += 1;
            }
            Some(&c) if c == s[si] => {
                pi += 1;
                si += 1;
            }
            Some(b'*') => {
                star = Some((pi, si));
                pi += 1;
            }
            _ => match star {
                Some((star_pi, star_si)) => {
                    pi = star_pi + 1;
                    si = star_si + 1;
                    star = Some((star_pi, si));
                }
                None => return false,
            },
        }
    }

    pat[pi..].iter().all(|&c| c == b'*')
}

/// Return all file paths matching a glob pattern like
/// `/run/systemd/network/*netplan-*`.
pub fn glob_paths(pattern: &str) -> Vec<String> {
    glob::glob(pattern)
        .map(|paths| {
            paths
                .filter_map(|p| p.ok())
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}
