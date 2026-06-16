//! `netplan ip leases` – display DHCP lease information for a network interface.
//!
//! Mirrors `netplan_cli/cli/commands/ip.py`.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};

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

pub fn run(args: IpArgs) -> Result<ExitCode> {
    match args.subcommand {
        Some(IpSubcommand::Leases(a)) => run_leases(a),
        None => return Err(anyhow!("Available commands:\n  leases   Display IP leases")),
    }
}

// ── ip leases ─────────────────────────────────────────────────────────────────

fn run_leases(args: LeasesArgs) -> Result<ExitCode> {
    let iface = &args.interface;
    let root_dir = &args.root_dir;

    // Use libnetplan to resolve which netdef manages this interface.
    // Mirrors find_interface() in generate.c: match by id, set-name, or match rules.
    let state = crate::netplan::load_state(root_dir)
        .map_err(|_| anyhow!("No lease found for interface '{iface}' (not managed by Netplan)"))?;

    let matches: Vec<_> = state
        .netdefs()
        .filter(|nd| {
            nd.id() == iface.as_str()
                || nd.set_name().as_deref() == Some(iface.as_str())
                || nd.matches_interface(iface, None, None)
        })
        .collect();

    let netdef = match matches.as_slice() {
        [netdef] => netdef,
        [] => {
            return Err(anyhow!(
                "No lease found for interface '{iface}' (not managed by Netplan)"
            ))
        }
        _ => {
            return Err(anyhow!(
                "No lease found for interface '{iface}': multiple netplan configurations match it"
            ))
        }
    };

    let backend = netdef.backend_name();

    let lease_result = match backend {
        "networkd" => find_networkd_lease(iface, root_dir),
        "NetworkManager" => find_nm_lease(iface, root_dir),
        other => return Err(anyhow!("unknown backend '{other}' for interface '{iface}'")),
    };

    match lease_result {
        Ok(path) => {
            let content = fs::read_to_string(&path).with_context(|| {
                format!("No lease found for interface '{iface}': cannot read {path:?}")
            })?;
            for line in content.lines() {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => return Err(anyhow!("No lease found for interface '{iface}': {e}")),
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
        return Err(anyhow!("no lease file at {:?}", lease_path));
    }
}

fn find_nm_lease(iface: &str, root_dir: &str) -> Result<PathBuf> {
    // Step 1: get the NM connection name via `nmcli dev show <iface>`.
    let dev_out = Command::new("nmcli")
        .args(["dev", "show", iface])
        .output()
        .context("failed to run nmcli dev show")?;

    if !dev_out.status.success() {
        return Err(anyhow!(
            "Could not find a NetworkManager connection for the interface: \
             nmcli exited with {}",
            dev_out.status
        ));
    }

    let dev_text = String::from_utf8_lossy(&dev_out.stdout);
    let conn_id = dev_text
        .lines()
        .find(|l| l.contains("GENERAL.CONNECTION"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty() && s != "--");

    let conn_id = conn_id.ok_or_else(|| {
        anyhow::anyhow!("Could not find a NetworkManager connection for the interface")
    })?;

    // Step 2: get the connection UUID via `nmcli con show id <conn_id>`.
    let con_out = Command::new("nmcli")
        .args(["con", "show", "id", &conn_id])
        .output()
        .context("failed to run nmcli con show")?;

    if !con_out.status.success() {
        return Err(anyhow!(
            "Could not find a NetworkManager connection for the interface: \
             nmcli exited with {}",
            con_out.status
        ));
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

    return Err(anyhow!(
        "no lease file found (tried internal and dhclient paths in {:?})",
        base
    ));
}
