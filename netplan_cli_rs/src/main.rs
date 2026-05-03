//! netplan – Rust CLI replacing the Python netplan_cli for the core commands:
//! apply / generate / get / set / info / ip / try.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod ffi;
mod netplan;
mod utils;

use commands::{apply, generate, get, info, ip, set, try_command};

// ── CLI skeleton ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "netplan",
    about = "Network configuration in YAML",
    version
)]
struct Cli {
    /// Enable verbose debug output
    #[arg(long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply current netplan config to running system
    Apply(apply::ApplyArgs),
    /// Generate backend specific configuration files from /etc/netplan/*.yaml
    Generate(generate::GenerateArgs),
    /// Get a setting by specifying a nested key like "ethernets.eth0.addresses", or "all"
    Get(get::GetArgs),
    /// Retrieve IP information from the system
    Ip(ip::IpArgs),
    /// Add/update/delete a setting via a dotted key=value pair
    Set(set::SetArgs),
    /// Show available features
    Info(info::InfoArgs),
    /// Try to apply a new netplan config with automatic rollback on timeout or rejection
    Try(try_command::TryArgs),
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // Match Python CLI environment setup
    std::env::set_var("LC_ALL", "C.UTF-8");

    // ── Detect systemd-generator calling convention ───────────────────────────
    //
    // When this binary is invoked as a systemd generator (or as `exe_generate`
    // in the test framework), the call looks like:
    //
    //   netplan [--root-dir ROOTDIR] [--ignore-errors] GEN_DIR GEN_EARLY GEN_LATE
    //
    // i.e. there is NO `generate` subcommand keyword.  Detect this by two
    // signals that are specific to the generator calling convention:
    //
    //   1. `--root-dir` is present  (always used in test/dev scenarios)
    //   2. The first non-option token is a filesystem path (starts with `/`),
    //      which is how `systemd` passes the generator output directories on a
    //      real system without --root-dir.
    //
    // Importantly, `netplan --help`, `netplan --version`, and unknown flags like
    // `netplan --foo` must NOT be treated as generator mode; those are handled
    // normally by clap.

    let mut raw_args: Vec<String> = std::env::args().collect();

    const SUBCOMMANDS: &[&str] = &["apply", "generate", "get", "set", "info", "ip", "try"];

    // First non-option argument (if any).
    let first_positional = raw_args.iter().skip(1).find(|a| !a.starts_with('-'));

    let has_known_subcommand = first_positional
        .map(|a| SUBCOMMANDS.contains(&a.as_str()))
        .unwrap_or(false);

    // Is this a systemd-generator / test-framework invocation?
    let has_root_dir   = raw_args.iter().skip(1).any(|a| a == "--root-dir");
    let first_is_path  = first_positional.map(|a| a.starts_with('/')).unwrap_or(false);
    let generator_mode = (has_root_dir || first_is_path) && !has_known_subcommand;

    if generator_mode {
        // Inject subcommand and hidden mode flag.
        raw_args.insert(1, "generate".to_string());
        raw_args.insert(2, "--generator-mode".to_string());
    }

    // ── Parse (with C-compatible error messages) ──────────────────────────────
    let cli = match Cli::try_parse_from(&raw_args) {
        Ok(c) => c,
        Err(e) => {
            // Reformat "unexpected argument" errors to match the C binary:
            //   "failed to parse options: Unknown option --foo"
            // so that existing tests keep passing.
            let msg = e.to_string();
            if msg.contains("unexpected argument '") {
                if let Some(arg) = extract_unknown_arg(&msg) {
                    eprintln!("failed to parse options: Unknown option {}", arg);
                    std::process::exit(1);
                }
            }
            // All other errors: let clap print its own message and exit.
            e.exit();
        }
    };

    if cli.debug {
        std::env::set_var("G_MESSAGES_DEBUG", "all");
        eprintln!("[netplan] debug mode enabled");
    }

    let result: Result<()> = match cli.command {
        Command::Apply(args)    => apply::run(args),
        Command::Generate(args) => generate::run(args),
        Command::Get(args)      => get::run(args),
        Command::Ip(args)       => ip::run(args),
        Command::Set(args)      => set::run(args),
        Command::Info(args)     => info::run(args),
        Command::Try(args)      => try_command::run(args),
    };

    if let Err(e) = result {
        // Format to match Python: "Command failed: <message>"
        eprintln!("Command failed: {}", e);
        std::process::exit(1);
    }
}

/// Extract the unknown argument name from a clap 4 error string.
///
/// Clap says: `error: unexpected argument '--foo' found\n...`
/// We want:   `--foo`
fn extract_unknown_arg(clap_error: &str) -> Option<String> {
    let prefix = "unexpected argument '";
    let start = clap_error.find(prefix)? + prefix.len();
    let rest = &clap_error[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}
