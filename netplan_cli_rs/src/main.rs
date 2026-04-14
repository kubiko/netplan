//! netplan – minimal Rust CLI replacing the Python netplan_cli for the five
//! core commands: apply / generate / get / set / info.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod ffi;
mod netplan;
mod utils;

use commands::{apply, generate, get, info, set, try_command};

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

    let cli = Cli::parse();

    if cli.debug {
        std::env::set_var("G_MESSAGES_DEBUG", "all");
        eprintln!("[netplan] debug mode enabled");
    }

    let result: Result<()> = match cli.command {
        Command::Apply(args)    => apply::run(args),
        Command::Generate(args) => generate::run(args),
        Command::Get(args)      => get::run(args),
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
