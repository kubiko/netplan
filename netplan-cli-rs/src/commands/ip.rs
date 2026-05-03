//! `netplan ip leases` – display DHCP lease information for a network interface.
//!
//! Mirrors `netplan_cli/cli/commands/ip.py`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};

use crate::utils;

// ── Argument types ────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct IpArgs {
    #[command(subcommand)]
    subcommand: Option<IpSubcommand>,
}

#[derive(Subcommand, Debug)]
enum IpSubcommand {
    /// Display DHCP lease information for a network interface
    Leases(LeasesArgs),
}

#[derive(Args, Debug)]
pub struct LeasesArgs {
    /// Network interface to display lease for
    interface: String,

    /// Search for configuration files in this root directory instead of /
    #[arg(long, default_value = "/")]
    root_dir: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: IpArgs) -> Result<()> {
    match args.subcommand {
        Some(IpSubcommand::Leases(a)) => run_leases(a),
        None => {
            eprintln!("Available commands:");
            eprintln!("  leases   Display IP leases");
            std::process::exit(1);
        }
    }
}

// ── ip leases ─────────────────────────────────────────────────────────────────

fn run_leases(args: LeasesArgs) -> Result<()> {
    let iface = &args.interface;
    let root_dir = &args.root_dir;

    // Call the generator with --mapping to resolve backend for this interface.
    // Uses NETPLAN_GENERATE_PATH when set (test environments point this to the C binary).
    let generator = utils::get_generator_path();
    let mut cmd_args = vec!["--mapping".to_string(), iface.clone()];
    if root_dir != "/" {
        cmd_args.extend_from_slice(&["--root-dir".to_string(), root_dir.clone()]);
    }

    let output = Command::new(&generator)
        .args(&cmd_args)
        .output()
        .with_context(|| format!("failed to run generator: {}", generator))?;

    if !output.status.success() {
        eprintln!(
            "No lease found for interface '{}' (not managed by Netplan)",
            iface
        );
        std::process::exit(1);
    }

    // Parse "id=enlol, backend=networkd, set_name=(null), ..." CSV output.
    let mapping_str = String::from_utf8_lossy(&output.stdout);
    let mapping = parse_mapping(mapping_str.trim());

    let backend = mapping.get("backend").map(String::as_str).unwrap_or("");

    let lease_result = match backend {
        "networkd" => find_networkd_lease(iface, root_dir),
        "NetworkManager" => find_nm_lease(iface, root_dir),
        other => bail!("unknown backend '{}' for interface '{}'", other, iface),
    };

    match lease_result {
        Ok(path) => {
            let content = fs::read_to_string(&path).with_context(|| {
                format!(
                    "No lease found for interface '{}': cannot read {:?}",
                    iface, path
                )
            })?;
            for line in content.lines() {
                println!("{}", line);
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("No lease found for interface '{}': {}", iface, e);
            std::process::exit(1);
        }
    }
}

// ── Backend-specific lease finders ───────────────────────────────────────────

fn find_networkd_lease(iface: &str, root_dir: &str) -> Result<PathBuf> {
    let ifindex_path = format!("/sys/class/net/{}/ifindex", iface);
    let ifindex = fs::read_to_string(&ifindex_path)
        .with_context(|| format!("Cannot read {}", ifindex_path))?;
    let ifindex = ifindex.trim();

    let lease_path = PathBuf::from(root_dir)
        .join("run/systemd/netif/leases")
        .join(ifindex);

    if lease_path.exists() {
        Ok(lease_path)
    } else {
        bail!("no lease file at {:?}", lease_path)
    }
}

fn find_nm_lease(iface: &str, root_dir: &str) -> Result<PathBuf> {
    // Step 1: get the NM connection name via `nmcli dev show <iface>`.
    let dev_out = Command::new("nmcli")
        .args(["dev", "show", iface])
        .output()
        .context("failed to run nmcli dev show")?;

    if !dev_out.status.success() {
        bail!(
            "Could not find a NetworkManager connection for the interface: \
             nmcli exited with {}",
            dev_out.status
        );
    }

    let dev_text = String::from_utf8_lossy(&dev_out.stdout);
    let conn_id = dev_text
        .lines()
        .find(|l| l.contains("GENERAL.CONNECTION"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty() && s != "--");

    let conn_id = conn_id
        .ok_or_else(|| anyhow::anyhow!("Could not find a NetworkManager connection for the interface"))?;

    // Step 2: get the connection UUID via `nmcli con show id <conn_id>`.
    let con_out = Command::new("nmcli")
        .args(["con", "show", "id", &conn_id])
        .output()
        .context("failed to run nmcli con show")?;

    if !con_out.status.success() {
        bail!(
            "Could not find a NetworkManager connection for the interface: \
             nmcli exited with {}",
            con_out.status
        );
    }

    let con_text = String::from_utf8_lossy(&con_out.stdout);
    let uuid = con_text
        .lines()
        .find(|l| l.contains("connection.uuid"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty());

    let uuid = uuid.ok_or_else(|| {
        anyhow::anyhow!("Could not find a NetworkManager connection for the interface")
    })?;

    let base = PathBuf::from(root_dir).join("var/lib/NetworkManager");

    // NM internal DHCP client lease (preferred).
    let internal = base.join(format!("internal-{}-{}.lease", uuid, iface));
    if internal.exists() {
        return Ok(internal);
    }

    // dhclient fallback.
    let dhclient = base.join(format!("dhclient-{}-{}.lease", uuid, iface));
    if dhclient.exists() {
        return Ok(dhclient);
    }

    bail!("no lease file found (tried internal and dhclient paths in {:?})", base)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse the C generator's `--mapping` output into a key→value map.
///
/// Format: `id=enlol, backend=networkd, set_name=(null), match_name=lo, ...`
fn parse_mapping(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            let value = if value == "(null)" { "" } else { value };
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}
