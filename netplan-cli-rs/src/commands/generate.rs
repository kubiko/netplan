//! `netplan generate` – produce backend-specific config from YAML sources.
//!
//! Mirrors `netplan_cli/cli/commands/generate.py`.
//!
//! This binary also acts as a **systemd generator** when invoked without the
//! `generate` subcommand keyword.  In that case `main()` injects
//! `generate --generator-mode` so this module handles both paths.

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

    /// Injected by main() when the binary is called as a systemd generator
    /// (i.e. without the explicit `generate` subcommand word).
    #[arg(long, hide = true)]
    generator_mode: bool,

    /// Ignore configuration errors (systemd generator convention).
    #[arg(long)]
    ignore_errors: bool,

    /// Trailing positional arguments in systemd-generator calling convention:
    ///   GENERATOR_DIR GENERATOR_EARLY_DIR GENERATOR_LATE_DIR [--flags...]
    ///
    /// `trailing_var_arg` causes clap to capture everything after the first
    /// positional — including tokens starting with `--` — so flags such as
    /// `--ignore-errors` appended after the dirs are parsed manually below.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    generator_dirs: Vec<String>,
}

pub fn run(args: GenerateArgs) -> Result<()> {
    // ── Systemd generator mode ────────────────────────────────────────────────
    if args.generator_mode {
        return run_generator_mode(args);
    }

    // ── SNAP environment: delegate to D-Bus ──────────────────────────────────
    if std::env::var("SNAP").is_ok() {
        let busctl =
            utils::which("busctl").ok_or_else(|| anyhow::anyhow!("'busctl' not found in PATH"))?;
        let rc = Command::new(&busctl)
            .args([
                "call",
                "--quiet",
                "--system",
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

    // ── Legacy --mapping path: use libnetplan to resolve interface→backend ────
    if let Some(ref mapping) = args.mapping {
        let rd = args.root_dir.as_deref().unwrap_or("/");
        return run_mapping(mapping, rd);
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

    let configure = utils::configure_path();

    if let Some(ref rd) = args.root_dir {
        // ── Testing / root-dir path: invoke generator binary directly ─────────
        let sd_gen = Path::new(rd).join("usr/lib/systemd/system-generators/netplan");
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

        // Symlink the real generator binary, removing any existing entry first
        // (setUp stubs or broken symlinks from a previous run would block creation)
        let real_gen = utils::generator_path();
        let _ = fs::remove_file(&sd_gen);
        if let Err(e) = std::os::unix::fs::symlink(&real_gen, &sd_gen) {
            eprintln!("[netplan] Could not symlink {:?}: {}", sd_gen, e);
        }

        let rc = Command::new(&sd_gen)
            .args([
                "--root-dir",
                rd,
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
            .with_context(|| format!("failed to run configure: {}", configure.display()))?
            .code()
            .unwrap_or(1);

        let _ = Command::new("udevadm")
            .args(["control", "--reload"])
            .status();

        std::process::exit(if rc != 0 { rc } else { rc2 });
    } else {
        // ── Normal system path: trigger via daemon-reload ─────────────────────
        utils::systemctl::daemon_reload()?;

        let rc = Command::new(&configure)
            .status()
            .with_context(|| format!("failed to run configure: {}", configure.display()))?
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

/// Handle the systemd-generator calling convention:
///
///   netplan [--root-dir ROOTDIR] GEN_DIR GEN_EARLY_DIR GEN_LATE_DIR [--ignore-errors]
///
/// (The `generate --generator-mode` prefix is injected by `main()` before
/// clap parsing, so by the time we arrive here the generator dirs and any
/// trailing flags are in `args.generator_dirs`.)
fn run_generator_mode(args: GenerateArgs) -> Result<()> {
    let rd = args.root_dir.as_deref().unwrap_or("/");

    // Separate directory paths from flags that may follow them.
    let mut dirs: Vec<String> = Vec::new();
    let mut ignore_errors = args.ignore_errors;

    for item in &args.generator_dirs {
        if let Some(flag) = item.strip_prefix("--") {
            match flag {
                "ignore-errors" => ignore_errors = true,
                _ => {
                    eprintln!("failed to parse options: Unknown option {}", item);
                    std::process::exit(1);
                }
            }
        } else {
            dirs.push(item.clone());
        }
    }

    // Called without generator dirs → invoked directly, not by systemd.
    if dirs.is_empty() {
        eprintln!(
            "{}: can not be called directly as a systemd generator",
            std::env::args().next().unwrap_or_default()
        );
        std::process::exit(1);
    }

    // Call the real C generator binary.  We derive its path from
    // NETPLAN_CONFIGURE_PATH so we never exec ourselves recursively.
    let c_generator = utils::c_generator_path();

    let mut cmd_args: Vec<String> = vec!["--root-dir".to_string(), rd.to_string()];
    cmd_args.extend(dirs);
    if ignore_errors {
        cmd_args.push("--ignore-errors".to_string());
    }

    let rc = Command::new(&c_generator)
        .args(&cmd_args)
        .status()
        .with_context(|| format!("failed to run C generator: {}", c_generator.display()))?
        .code()
        .unwrap_or(1);

    std::process::exit(rc);
}

/// Implement `generate --mapping <iface>` using libnetplan instead of shelling
/// out to the C generator binary.  Mirrors `find_interface()` in generate.c:
/// matches by netdef id, set-name, or match.name/match rules.
fn run_mapping(iface: &str, root_dir: &str) -> Result<()> {
    let state = crate::netplan::load_state(root_dir)?;

    // Collect all netdefs that match the requested interface name.
    let matches: Vec<_> = state
        .netdefs()
        .filter(|nd| {
            nd.id() == iface
                || nd.set_name().as_deref() == Some(iface)
                || nd.matches_interface(iface, None, None)
        })
        .collect();

    if matches.len() != 1 {
        std::process::exit(1);
    }

    let nd = &matches[0];
    let set_name = nd.set_name().unwrap_or_else(|| "(null)".to_string());
    println!(
        "id={}, backend={}, set_name={}, match_name=(null), match_mac=(null), match_driver=(null)",
        nd.id(),
        nd.backend_name(),
        set_name,
    );
    Ok(())
}
