//! `netplan generate` – produce backend-specific config from YAML sources.
//!
//! Mirrors `netplan_cli/cli/commands/generate.py`.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::utils;

#[derive(Args, Debug)]
pub struct GenerateArgs {
    /// Search for and generate config in this root directory instead of /
    #[arg(long)]
    root_dir: Option<String>,

    /// Display the netplan device ID/backend/interface mapping and exit
    /// (legacy option, passes through to the old generator binary)
    #[arg(long)]
    mapping: Option<String>,
}

pub fn run(args: GenerateArgs) -> Result<()> {
    // ── SNAP environment: delegate to D-Bus ──────────────────────────────────
    if std::env::var("SNAP").is_ok() {
        let busctl = which("busctl")?;
        let rc = Command::new(&busctl)
            .args([
                "call", "--quiet", "--system",
                "io.netplan.Netplan",
                "/io/netplan/Netplan",
                "io.netplan.Netplan",
                "Generate",
            ])
            .status()
            .context("failed to run busctl")?
            .code()
            .unwrap_or(1);

        if rc == 130 {
            bail!("PermissionError: failed to communicate with dbus service");
        } else if rc != 0 {
            bail!("failed to communicate with dbus service: error {}", rc);
        }
        return Ok(());
    }

    let rootdir = args.root_dir.as_deref().unwrap_or("/");

    // ── Legacy --mapping path (passes through to old generator) ──────────────
    if let Some(ref mapping) = args.mapping {
        let generator = utils::get_generator_path();
        let mut cmd_args = vec!["--mapping".to_string(), mapping.clone()];
        if let Some(ref rd) = args.root_dir {
            cmd_args.extend_from_slice(&["--root-dir".to_string(), rd.clone()]);
        }
        let rc = Command::new(&generator)
            .args(&cmd_args)
            .status()
            .with_context(|| format!("failed to run generator: {}", generator))?
            .code()
            .unwrap_or(1);
        std::process::exit(rc);
    }

    // ── Standard path ────────────────────────────────────────────────────────

    // If 'netplan try' is restoring config, skip generation to avoid races
    let try_stamp = Path::new(rootdir).join(utils::TRY_READY_STAMP);
    if try_stamp.exists() {
        eprintln!(
            "[netplan] Skipping daemon-reload – 'netplan try' is restoring configuration. \
             Remove {:?} to force re-run.",
            try_stamp
        );
        std::process::exit(1);
    }

    let configure = utils::get_configure_path();

    if let Some(ref rd) = args.root_dir {
        // ── Testing / root-dir path: invoke generator binary directly ─────────
        let sd_gen = Path::new(rd)
            .join("usr/lib/systemd/system-generators/netplan");
        let gen_dir = Path::new(rd).join("run/systemd/generator");
        let gen_early = Path::new(rd).join("run/systemd/generator.early");
        let gen_late = Path::new(rd).join("run/systemd/generator.late");

        // Ensure generator directories exist
        for dir in [
            sd_gen.parent().unwrap(),
            gen_dir.as_path(),
            gen_early.as_path(),
            gen_late.as_path(),
        ] {
            if let Err(e) = fs::create_dir_all(dir) {
                eprintln!("[netplan] Could not create {:?}: {}", dir, e);
            }
        }

        // Symlink the real generator binary if not present
        let real_gen = utils::get_generator_path();
        if !sd_gen.exists() {
            if let Err(e) = std::os::unix::fs::symlink(&real_gen, &sd_gen) {
                eprintln!("[netplan] Could not symlink {:?}: {}", sd_gen, e);
            }
        }

        let rc = Command::new(&sd_gen)
            .args([
                "--root-dir", rd,
                gen_dir.to_str().unwrap_or(""),
                gen_early.to_str().unwrap_or(""),
                gen_late.to_str().unwrap_or(""),
            ])
            .status()
            .with_context(|| format!("failed to run generator: {:?}", sd_gen))?
            .code()
            .unwrap_or(1);

        // Also run configure for the root-dir path
        let rc2 = Command::new(&configure)
            .args(["--root-dir", rd])
            .status()
            .with_context(|| format!("failed to run configure: {}", configure))?
            .code()
            .unwrap_or(1);

        let _ = Command::new("udevadm")
            .args(["control", "--reload"])
            .status();

        std::process::exit(if rc != 0 { rc } else { rc2 });
    } else {
        // ── Normal system path: trigger via daemon-reload ─────────────────────
        utils::systemctl_daemon_reload()?;

        let rc = Command::new(&configure)
            .status()
            .with_context(|| format!("failed to run configure: {}", configure))?
            .code()
            .unwrap_or(1);

        // Best-effort udevadm reload
        if let Err(e) = Command::new("udevadm")
            .args(["control", "--reload"])
            .status()
        {
            eprintln!("[netplan] Could not call 'udevadm control --reload': {}", e);
        }

        std::process::exit(rc);
    }
}

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
